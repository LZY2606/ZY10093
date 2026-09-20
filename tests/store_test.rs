mod common;
use bfw::store::*;
use common::*;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bfw-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample(id: &str, hex: &str) -> SampleDoc {
    SampleDoc {
        id: id.into(), rev: 0,
        format: bfw::model::Ref { id: "img".into(), version: 1 },
        name: id.into(), hex: hex.into(), note: "".into(), derived_from: None,
    }
}

#[test]
fn duplicate_request_with_same_idempotency_key_returns_cached_response() {
    let dir = temp_dir("idem");
    let store = Store::open(&dir).unwrap();
    let (s1, b1) = store
        .upsert_sample(sample("a", "01"), Some("key-x"))
        .unwrap();
    let (s2, b2) = store
        .upsert_sample(sample("a-different-body", "02"), Some("key-x"))
        .unwrap();
    assert_eq!(s1, s2);
    assert_eq!(b1, b2);
    assert_eq!(store.snapshot().samples.len(), 1);
}

#[test]
fn stale_rule_write_returns_409_with_two_sided_diff() {
    let dir = temp_dir("conflict");
    let store = Store::open(&dir).unwrap();
    store.upsert_format(v1(), None).unwrap();
    store.upsert_format(v2(), None).unwrap();
    store.upsert_rule(rule(), 0, None).unwrap();
    let mut newer = rule();
    newer.description = "changed by someone else".into();
    let err = store.upsert_rule(newer, 0, Some("retry")).unwrap_err();
    assert_eq!(err.status, 409);
    assert_eq!(err.body["diff"]["server_rev"], serde_json::json!(1));
    assert!(err.body["diff"]["fields"].is_array());
    assert!(err.body["diff"]["server"].is_object());
}

#[test]
fn recovers_state_after_reopening_store() {
    let dir = temp_dir("recover");
    {
        let store = Store::open(&dir).unwrap();
        store.upsert_format(v1(), None).unwrap();
        store.upsert_sample(sample_doc(), Some("seeded-key")).unwrap();
    }
    let reopened = Store::open(&dir).unwrap();
    let state = reopened.snapshot();
    assert!(state.format("img", 1).is_some());
    assert!(state.samples.contains_key("s"));
    assert!(state.idempotency.contains_key("seeded-key"));
}

#[test]
fn deterministic_export_is_stable_across_reopens() {
    let dir = temp_dir("export");
    let bundle_one = {
        let store = Store::open(&dir).unwrap();
        store.upsert_format(v1(), None).unwrap();
        store.upsert_format(v2(), None).unwrap();
        store.upsert_rule(rule(), 0, None).unwrap();
        let state = store.snapshot();
        let value = serde_json::json!({
            "formats": state.formats,
            "rules": state.rules.values().map(|s| serde_json::to_value(s).unwrap()).collect::<Vec<_>>(),
            "samples": state.samples,
        });
        bfw::util::canonical_json(&value)
    };
    let bundle_two = {
        let store = Store::open(&dir).unwrap();
        let state = store.snapshot();
        let value = serde_json::json!({
            "formats": state.formats,
            "rules": state.rules.values().map(|s| serde_json::to_value(s).unwrap()).collect::<Vec<_>>(),
            "samples": state.samples,
        });
        bfw::util::canonical_json(&value)
    };
    assert_eq!(bundle_one, bundle_two);
}

#[test]
fn snapshot_recovery_after_many_events() {
    let dir = temp_dir("snapshot");
    {
        let store = Store::open(&dir).unwrap();
        for i in 0..60 {
            store
                .upsert_sample(sample(&format!("s{i}"), &format!("{:02x}", i as u8)), None)
                .unwrap();
        }
    }
    assert!(std::fs::metadata(dir.join("snapshot.json")).is_ok());
    let reopened = Store::open(&dir).unwrap();
    assert_eq!(reopened.snapshot().samples.len(), 60);
}
