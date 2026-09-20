//! White-box crash recovery and batch atomicity tests against the store directly.

use binfmt_workbench::store::{ConvertedFile, Store};

fn fresh_db() -> (String, Store) {
    let path = std::env::temp_dir().join(format!(
        "wbench_rec_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let p = path.to_string_lossy().to_string();
    let store = Store::open(&p).unwrap();
    (p, store)
}

#[test]
fn running_batch_after_abnormal_exit_is_recovered_to_aborted() {
    let (path, _store) = {
        let (p, s) = fresh_db();
        // Simulate a process that inserted a 'running' batch marker and then died
        // mid-conversion (transaction lost). This is the state startup recovery
        // must find.
        {
            // need a published plan first for commit_batch, but recovery applies
            // to raw 'running' rows regardless, so inject directly.
            let c = s.conn_lock();
            c.execute(
                "INSERT INTO batches(id,plan_id,status,count,created_at)
                 VALUES('batch_stuck','plan_x','running',0,'1')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO outputs(batch_id,sample_id,bytes,sha256,ord)
                 VALUES('batch_stuck','partial',X'0001','deadbeef',0)",
                [],
            )
            .unwrap();
        }
        // Simulate abnormal exit by dropping without any completing transaction.
        drop(s);
        (p, ())
    };
    let _ = path;

    // Reopen with a fresh Store (as the server would on restart).
    let store2 = Store::open(&path).unwrap();
    let visible = store2.list_batches(false).unwrap();
    assert!(visible.is_empty(), "recovered partial batch must not be visible: {visible:?}");

    // Forensically, it is recorded as aborted_crash and outputs stay for audit.
    let all = store2.list_batches(true).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "aborted_crash");

    // Recovery is idempotent: reopening again leaves state stable.
    drop(store2);
    let store3 = Store::open(&path).unwrap();
    let all3 = store3.list_batches(true).unwrap();
    assert_eq!(all3.len(), 1);
    assert_eq!(all3[0].status, "aborted_crash");
}

#[test]
fn completed_batch_outputs_are_durable_across_reopen() {
    let (path, store) = fresh_db();
    // inject a completed batch + outputs directly (commit_batch needs published plan)
    {
        let c = store.conn_lock();
        c.execute(
            "INSERT INTO batches(id,plan_id,status,count,created_at)
             VALUES('b_ok','p','completed',2,'1')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO outputs(batch_id,sample_id,bytes,sha256,ord) VALUES('b_ok','s1',X'aabb','s1h',0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO outputs(batch_id,sample_id,bytes,sha256,ord) VALUES('b_ok','s2',X'ccdd','s2h',1)",
            [],
        )
        .unwrap();
    }
    drop(store);
    let s2 = Store::open(&path).unwrap();
    let outs = s2.batch_outputs("b_ok").unwrap();
    assert_eq!(outs.len(), 2);
    let all = s2.list_batches(false).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "completed");
}

#[test]
fn batch_commit_is_atomic_and_visible_only_fully() {
    let (path, store) = fresh_db();
    // commit_batch itself only runs after every file is converted upstream, and
    // writes batch+outputs in one transaction. Here we assert the durable result
    // either contains every file or none is listed.
    let files = vec![
        ConvertedFile { sample_id: "a".into(), bytes: vec![1] },
        ConvertedFile { sample_id: "b".into(), bytes: vec![2, 2] },
    ];
    // Without a published plan this must fail BEFORE creating any batch row.
    let err = store.commit_batch("no_such_plan", files).is_err();
    assert!(err);
    let visible = store.list_batches(false).unwrap();
    assert!(visible.is_empty(), "failed batch must leave no visible batch: {visible:?}");
    // and no stray outputs
    {
        let c = store.conn_lock();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM outputs", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
    let _ = path;
}

#[test]
fn export_tar_is_deterministic_on_reopen() {
    let (path, store) = {
        let (p, s) = fresh_db();
        s.insert_def(
            &serde_json::json!({"name":"f","version":"v1","fields":[
                {"name":"magic","kind":"magic","value":"aa"}
            ]}).to_string(),
            None,
        )
        .unwrap();
        s.insert_sample("one", "f", 1, &[0xAA, 0xBB]).unwrap();
        (p, s)
    };
    let t1 = store.export_tar().unwrap();
    drop(store);
    let s2 = Store::open(&path).unwrap();
    let t2 = s2.export_tar().unwrap();
    assert_eq!(t1, t2, "export must survive restart deterministically");
    // tar ends with two zero blocks
    assert!(t1.ends_with(&[0u8; 1024]));
}
