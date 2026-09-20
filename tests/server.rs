mod common;
use common::{http, spawn_server};
use serde_json::json;

fn v1() -> serde_json::Value {
    json!({
        "spec": {
            "name":"img","version":"v1",
            "fields":[
                {"name":"magic","kind":"magic","value":"494d4731"},
                {"name":"kind","kind":"int","width":1,"endian":"big"},
                {"name":"n","kind":"int","width":2,"endian":"big"},
                {"name":"body","kind":"bytes","length":"n"}
            ]
        },
        "base_revision": 0
    })
}

fn v2_inherit() -> serde_json::Value {
    json!({
        "spec": {
            "name":"img","version":"v2",
            "inherit":{"name":"img","revision":1},
            "fields":[
                {"name":"extra","kind":"int","width":2,"endian":"big"}
            ]
        },
        "base_revision": 1
    })
}

#[test]
fn full_lifecycle_idempotency_conflict_and_export() {
    let (port, mut child) = spawn_server();

    // create def r1 with idempotency key; repeated request returns same revision
    let body = v1().to_string();
    let (c1, t1, _) = http("POST", port, "/api/defs", Some(&body), Some("key-def-1"));
    assert_eq!(c1, 200, "{t1}");
    let (c1b, t1b, _) = http("POST", port, "/api/defs", Some(&body), Some("key-def-1"));
    assert_eq!(c1b, 200);
    assert_eq!(
        t1b.chars().filter(|c| c.is_numeric()).collect::<String>(),
        t1.chars().filter(|c| c.is_numeric()).collect::<String>()
    );

    // stale write conflict: send again with base 0 -> 409 with diffs
    let (c2, t2, _) = http("POST", port, "/api/defs", Some(&body), None);
    assert_eq!(c2, 409);
    let v: serde_json::Value = serde_json::from_str(&t2).unwrap();
    assert_eq!(v["code"], "revision_conflict");
    assert!(v["diffs"].is_array());
    assert_eq!(v["actual_revision"], 1);

    // invalid spec -> 400
    let bad = "{not json".to_string();
    let (cb, _, _) = http("POST", port, "/api/defs", Some(&bad), None);
    // malformed JSON -> 400 (axum extractor)
    assert!(cb == 400 || cb == 422);

    // inheritance: child r2 adds extra
    let (ci, ti, _) = http("POST", port, "/api/defs", Some(&v2_inherit().to_string()), Some("key-def-2"));
    assert_eq!(ci, 200, "{ti}");
    let (cv, tv, _) = http("GET", port, "/api/defs/img/2", None, None);
    assert_eq!(cv, 200);
    let rv: serde_json::Value = serde_json::from_str(&tv).unwrap();
    let names: Vec<&str> = rv["resolved"]["fields"].as_array().unwrap().iter()
        .map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["magic", "kind", "n", "body", "extra"]);

    // shadow rejected: try revision 3 re-declaring 'n'
    let shadow = json!({"spec":{"name":"img","version":"v3",
        "inherit":{"name":"img","revision":2},
        "fields":[{"name":"n","kind":"int","width":4}]}, "base_revision":2}).to_string();
    let (cs, ts, _) = http("POST", port, "/api/defs", Some(&shadow), None);
    assert_eq!(cs, 422, "RESP: {ts}");
    assert!(ts.contains("shadow_field"));

    // upload sample (original bytes preserved)
    let sample = json!({"name":"one","format_name":"img","format_revision":1,"hex":"494d4731 07 0002 aabb"}).to_string();
    let (csm, tsm, _) = http("POST", port, "/api/samples", Some(&sample), Some("key-smp-1"));
    assert_eq!(csm, 200, "{tsm}");
    let sv: serde_json::Value = serde_json::from_str(&tsm).unwrap();
    let sample_id = sv["id"].as_str().unwrap().to_string();
    // repeated upload with same idem key -> same id
    let (_, tsm2, _) = http("POST", port, "/api/samples", Some(&sample), Some("key-smp-1"));
    let sv2: serde_json::Value = serde_json::from_str(&tsm2).unwrap();
    assert_eq!(sv2["id"], sv["id"]);

    // parse: identity must be identical
    let (cp, tp, _) = http("GET", port, &format!("/api/parse?name=img&revision=1&sample={sample_id}"), None, None);
    assert_eq!(cp, 200, "{tp}");
    let pv: serde_json::Value = serde_json::from_str(&tp).unwrap();
    assert_eq!(pv["ok"], true);
    assert_eq!(pv["identity_identical"], true);
    assert_eq!(pv["identity_hex"], "494d4731070002aabb");

    child.kill().unwrap();
}

#[test]
fn illegal_plan_transitions_and_lossy_freeze_gate() {
    let (port, mut child) = spawn_server();

    // defs
    let def1 = json!({"spec":{"name":"a","version":"v1","fields":[
        {"name":"magic","kind":"magic","value":"4101"},
        {"name":"v","kind":"int","width":1}]}, "base_revision":0}).to_string();
    let (c,_,_)=http("POST",port,"/api/defs",Some(&def1),Some("d1")); assert_eq!(c,200);
    let def2 = json!({"spec":{"name":"a","version":"v2","fields":[
        {"name":"magic","kind":"magic","value":"4102"},
        {"name":"v","kind":"int","width":2}]}, "base_revision":1}).to_string();
    let (c,tdb,_)=http("POST",port,"/api/defs",Some(&def2),Some("d2")); assert_eq!(c,200,"{tdb}");

    // rule v1->v2 copy v (widened)
    let rule = json!({"rule":{"name":"widen","from_format":"a","from_revision":1,
        "to_format":"a","to_revision":2,
        "mappings":[{"op":"copy","from":"v","to":"v"}]}, "base_revision":0}).to_string();
    let (c,tr,_)=http("POST",port,"/api/rules",Some(&rule),Some("r1")); assert_eq!(c,200,"{tr}");

    // sample v1
    let smp = json!({"name":"s","format_name":"a","format_revision":1,"hex":"41012a"}).to_string();
    let (_,ts,_)=http("POST",port,"/api/samples",Some(&smp),Some("s1"));
    let sid: String = serde_json::from_str::<serde_json::Value>(&ts).unwrap()["id"].as_str().unwrap().into();

    // dryrun
    let dr = json!({"rule_name":"widen","rule_revision":1,"samples":[sid]}).to_string();
    let (c,td,_)=http("POST",port,"/api/dryrun",Some(&dr),None); assert_eq!(c,200,"{td}");
    let dry: serde_json::Value = serde_json::from_str(&td).unwrap();
    assert_eq!(dry["ok"], true);

    // create plan
    let fps = json!({"from":"fp","to":"fp2"});
    let pc = json!({"name":"p","rule_name":"widen","rule_revision":1,"dryrun":dry,"fingerprints":fps}).to_string();
    let (c,tp,_)=http("POST",port,"/api/plans",Some(&pc),Some("p1")); assert_eq!(c,200,"{tp}");
    let pv: serde_json::Value = serde_json::from_str(&tp).unwrap();
    let pid = pv["id"].as_str().unwrap();
    assert_eq!(pv["status"], "draft");

    // illegal: publish directly from draft
    let illegal = json!({"revision":1,"action":"publish"}).to_string();
    let (c,ti,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),Some(&illegal),None);
    assert_eq!(c, 422, "{ti}");
    assert!(ti.contains("illegal state transition"));

    // freeze (no lossy items in this widen rule)
    let fr = json!({"revision":1,"action":"freeze","acceptances":[]}).to_string();
    let (c,tf,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),Some(&fr),None);
    assert_eq!(c,200,"{tf}");

    // stale revision transition -> 409
    let stale = json!({"revision":1,"action":"publish"}).to_string();
    let (c,ts,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),Some(&stale),None);
    assert_eq!(c,409);
    assert!(ts.contains("revision_conflict"));

    // frozen fingerprints are computed server-side (rule + both format revs)
    let (_, tpf, _) = http("GET", port, &format!("/api/plans/{pid}"), None, None);
    let fpv: serde_json::Value = serde_json::from_str(&tpf).unwrap();
    assert_eq!(fpv["fingerprints"]["frozen"], true);
    assert!(fpv["fingerprints"]["rule_fingerprint"].as_str().unwrap().len() == 64);
    assert_eq!(fpv["fingerprints"]["from_format"]["revision"], 1);
    assert_eq!(fpv["fingerprints"]["to_format"]["revision"], 2);
    assert!(fpv["fingerprints"]["to_format"]["fingerprint"].as_str().unwrap().len() == 64);

    // publish with current revision 2
    let pb = json!({"revision":2,"action":"publish"}).to_string();
    let (c,tpb,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),Some(&pb),None);
    assert_eq!(c,200,"{tpb}");

    // batch converts the sample
    let bb = json!({"plan_id":pid}).to_string();
    let (c,tb,_)=http("POST",port,"/api/batches",Some(&bb),Some("b1"));
    assert_eq!(c,200,"{tb}");
    let bv: serde_json::Value = serde_json::from_str(&tb).unwrap();
    assert_eq!(bv["count"], 1);

    // repeated batch with same idem key -> same batch id
    let (_,tb2,_)=http("POST",port,"/api/batches",Some(&bb),Some("b1"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&tb2).unwrap()["batch_id"], bv["batch_id"]);

    // export deterministic: fetch twice, compare bytes
    let (_,_,e1)=http("GET",port,"/api/export.tar",None,None);
    let (_,_,e2)=http("GET",port,"/api/export.tar",None,None);
    assert_eq!(e1,e2,"export must be byte-deterministic");
    assert!(e1.len() > 1024);

    child.kill().unwrap();
}

#[test]
fn batch_with_any_bad_file_creates_no_visible_batch() {
    let (port, mut child) = spawn_server();

    // v1: magic + length-prefixed bytes; v2: same shape, wider length field.
    let def1 = json!({"spec":{"name":"b","version":"v1","fields":[
        {"name":"magic","kind":"magic","value":"4231"},
        {"name":"n","kind":"int","width":1},
        {"name":"data","kind":"bytes","length":"n"}
    ]},"base_revision":0}).to_string();
    let (c,_,_)=http("POST",port,"/api/defs",Some(&def1),Some("bd1")); assert_eq!(c,200);
    let def2 = json!({"spec":{"name":"b","version":"v2","fields":[
        {"name":"magic","kind":"magic","value":"4232"},
        {"name":"n","kind":"int","width":2},
        {"name":"data","kind":"bytes","length":"n"}
    ]},"base_revision":1}).to_string();
    let (c,_,_)=http("POST",port,"/api/defs",Some(&def2),Some("bd2")); assert_eq!(c,200);

    // rule: copy n (1->2 bytes widening) and data
    let rule = json!({"rule":{"name":"bb","from_format":"b","from_revision":1,
        "to_format":"b","to_revision":2,
        "mappings":[{"op":"copy","from":"n","to":"n"},
                    {"op":"copy","from":"data","to":"data"}]},"base_revision":0}).to_string();
    let (c,tr,_)=http("POST",port,"/api/rules",Some(&rule),Some("br1")); assert_eq!(c,200,"{tr}");

    // good sample and bad (declared length exceeds input)
    let good = json!({"name":"g","format_name":"b","format_revision":1,"hex":"4231 02 abcd"}).to_string();
    let (_,tg,_)=http("POST",port,"/api/samples",Some(&good),Some("bg"));
    let gid = serde_json::from_str::<serde_json::Value>(&tg).unwrap()["id"].as_str().unwrap().to_string();
    let bad = json!({"name":"bad","format_name":"b","format_revision":1,"hex":"4231 05 ab"}).to_string();
    let (_,tb,_)=http("POST",port,"/api/samples",Some(&bad),Some("bbad"));
    let bid = serde_json::from_str::<serde_json::Value>(&tb).unwrap()["id"].as_str().unwrap().to_string();

    // dryrun must surface a failure for the bad sample
    let dr = json!({"rule_name":"bb","rule_revision":1,"samples":[gid.clone(),bid.clone()]}).to_string();
    let (_,td,_)=http("POST",port,"/api/dryrun",Some(&dr),None);
    let dv = serde_json::from_str::<serde_json::Value>(&td).unwrap();
    assert_eq!(dv["ok"], false);
    let failed_sample = dv["samples"].as_array().unwrap().iter().any(|s| s["ok"]==false && s["sample_id"]==bid);
    assert!(failed_sample);

    // create + freeze + publish a plan using only the good-sample dryrun
    let dr2 = json!({"rule_name":"bb","rule_revision":1,"samples":[gid.clone()]}).to_string();
    let (_,td2,_)=http("POST",port,"/api/dryrun",Some(&dr2),None);
    let dry2: serde_json::Value = serde_json::from_str(&td2).unwrap();
    assert_eq!(dry2["ok"], true, "{td2}");
    let pc = json!({"name":"bp","rule_name":"bb","rule_revision":1,"dryrun":dry2,
                   "fingerprints":{"src":"s","dst":"d","rule":"r"}}).to_string();
    let (_,tp,_)=http("POST",port,"/api/plans",Some(&pc),Some("bp1"));
    let pid = serde_json::from_str::<serde_json::Value>(&tp).unwrap()["id"].as_str().unwrap().to_string();
    let (_,_,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),
                     Some(&json!({"revision":1,"action":"freeze","acceptances":[]}).to_string()),None);
    let (_,_,_)=http("POST",port,&format!("/api/plans/{pid}/transition"),
                     Some(&json!({"revision":2,"action":"publish"}).to_string()),None);

    // batch over BOTH samples -> 422, no batch row
    let bb2 = json!({"plan_id":pid}).to_string();
    let (c,tfail,_)=http("POST",port,"/api/batches",Some(&bb2),Some("batchfail"));
    assert_eq!(c,422,"{tfail}");
    let fv = serde_json::from_str::<serde_json::Value>(&tfail).unwrap();
    assert!(fv["failures"].as_array().unwrap().iter().any(|f| f["sample_id"]==bid));

    let (_,tl,_)=http("GET",port,"/api/batches",None,None);
    let lv = serde_json::from_str::<serde_json::Value>(&tl).unwrap();
    assert_eq!(lv["batches"].as_array().unwrap().len(), 0, "no visible batch on failure: {tl}");

    // batch over only the good sample succeeds
    let bok = json!({"plan_id":pid,"samples":[gid]}).to_string();
    let (c,tok,_)=http("POST",port,"/api/batches",Some(&bok),Some("batchok"));
    assert_eq!(c,200,"{tok}");
    let (_,tl2,_)=http("GET",port,"/api/batches",None,None);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&tl2).unwrap()["batches"].as_array().unwrap().len(), 1);

    child.kill().unwrap();
}
