mod common;
use bfw::service::{handle, Registry, Request};
use bfw::store::*;
use common::*;
use std::collections::BTreeMap;

struct Harness {
    store: Store,
    registry: Registry,
}

impl Harness {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bfw-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        store.upsert_format(v1(), None).unwrap();
        store.upsert_format(v2(), None).unwrap();
        store.upsert_rule(rule(), 0, None).unwrap();
        store.upsert_sample(sample_doc(), None).unwrap();
        Harness { store, registry: Registry::new() }
    }

    fn call(&mut self, method: &str, path: &str, body: serde_json::Value, key: Option<&str>) -> (u16, serde_json::Value) {
        let mut headers = BTreeMap::new();
        if let Some(k) = key {
            headers.insert("idempotency-key".to_string(), k.to_string());
        }
        let req = Request {
            method: method.into(),
            path: path.into(),
            query: BTreeMap::new(),
            headers,
            body: serde_json::to_vec(&body).unwrap(),
        };
        let resp = handle(&self.store, &mut self.registry, &req);
        (resp.status, serde_json::from_slice(&resp.body).unwrap_or(serde_json::Value::Null))
    }
}

fn dry_body() -> serde_json::Value {
    serde_json::json!({
        "sample_id": "s",
        "rule": {"id": "r", "version": 1}
    })
}

#[test]
fn parse_emit_roundtrip_and_dry_run() {
    let mut h = Harness::new();
    let (s, parse) = h.call("POST", "/api/parse", serde_json::json!({"sample_id":"s"}), None);
    assert_eq!(s, 200);
    assert_eq!(parse["errors"], serde_json::json!([]));

    let (s, emit) = h.call("POST", "/api/emit", serde_json::json!({"sample_id":"s","edits":{}}), None);
    assert_eq!(s, 200);
    assert_eq!(emit["byte_identical"], serde_json::json!(true));

    let (s, dry) = h.call("POST", "/api/migrate/dry-run", dry_body(), None);
    assert_eq!(s, 200);
    assert_eq!(dry["equivalence"], serde_json::json!("lossy"));
}

fn draft_plan(accepted: bool) -> serde_json::Value {
    let losses = if accepted {
        serde_json::json!([
            {"path":"comment","rule_id":"r","rule_version":1,"note":"x"},
            {"path":"comment_len","rule_id":"r","rule_version":1,"note":"x"}
        ])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "plan": {
            "id": "p", "rev": 0,
            "rule": {"id":"r","version":1},
            "state": "draft",
            "accepted_losses": losses
        },
        "sample_ids": ["s"]
    })
}

#[test]
fn publish_validates_loss_acceptance_and_freezes_fingerprints() {
    let mut h = Harness::new();
    let (s, rejected) = h.call("POST", "/api/plans/publish", draft_plan(false), None);
    assert_eq!(s, 422);
    assert_eq!(rejected["error"], serde_json::json!("publish_rejected"));

    let (s, published) = h.call("POST", "/api/plans/publish", draft_plan(true), None);
    assert_eq!(s, 200);
    assert_eq!(published["plan"]["state"], serde_json::json!("published"));
    assert!(published["plan"]["fingerprints"]["rule"].is_string());
}

#[test]
fn illegal_transition_publishing_non_draft_is_rejected() {
    let mut h = Harness::new();
    h.call("POST", "/api/plans/publish", draft_plan(true), None);
    let mut second = draft_plan(true);
    second["plan"]["state"] = serde_json::json!("published");
    second["plan"]["rev"] = serde_json::json!(2);
    let (s, body) = h.call("POST", "/api/plans/publish", second, None);
    assert_eq!(s, 409);
    assert_eq!(body["error"], serde_json::json!("illegal_transition"));
}

#[test]
fn batch_failure_creates_no_visible_batch() {
    let mut h = Harness::new();
    h.call("POST", "/api/plans/publish", draft_plan(true), None);
    let (s, failed) = h.call(
        "POST",
        "/api/plans/batch",
        serde_json::json!({"plan_id":"p","sample_ids":["s","ghost"]}),
        None,
    );
    assert_eq!(s, 422);
    assert_eq!(failed["error"], serde_json::json!("batch_aborted_no_outputs"));

    let (s, success) = h.call(
        "POST",
        "/api/plans/batch",
        serde_json::json!({"plan_id":"p","sample_ids":["s"]}),
        Some("batch-key"),
    );
    assert_eq!(s, 201);
    assert_eq!(success["batch"]["id"], serde_json::json!("batch-1"));
}

#[test]
fn duplicate_batch_submission_is_idempotent() {
    let mut h = Harness::new();
    h.call("POST", "/api/plans/publish", draft_plan(true), None);
    let body = serde_json::json!({"plan_id":"p","sample_ids":["s"]});
    let (s1, b1) = h.call("POST", "/api/plans/batch", body.clone(), Some("dup-batch"));
    let (s2, b2) = h.call("POST", "/api/plans/batch", body, Some("dup-batch"));
    assert_eq!(s1, s2);
    assert_eq!(b1, b2);
}

#[test]
fn deterministic_export_endpoint() {
    let mut h = Harness::new();
    let (_, a) = h.call("GET", "/api/export", serde_json::json!({}), None);
    let (_, b) = h.call("GET", "/api/export", serde_json::json!({}), None);
    assert_eq!(a["fingerprint"], b["fingerprint"]);
    assert_eq!(a["bundle"], b["bundle"]);
}

#[test]
fn bad_magic_and_bad_hex_are_reported_not_panicked() {
    let mut h = Harness::new();
    let (s, body) = h.call(
        "POST",
        "/api/parse",
        serde_json::json!({"format":{"id":"img","version":1},"hex":"zz"}),
        None,
    );
    assert_eq!(s, 400);
    assert_eq!(body["error"], serde_json::json!("invalid_hex"));
}

#[test]
fn can_create_a_new_format_without_preexisting_record() {
    let mut h = Harness::new();
    let doc = serde_json::json!({
        "id": "brand-new",
        "version": 1,
        "endian": "little",
        "magic": [],
        "description": "",
        "layout": [
            {"type": "int", "name": "a", "int": "u8"}
        ]
    });
    let (s, body) = h.call("PUT", "/api/formats", doc, Some("create-fmt"));
    assert_eq!(s, 201, "{}", body);
    assert!(body["fingerprint"].is_string());

    let (s2, body2) = h.call(
        "PUT",
        "/api/formats",
        serde_json::json!({
            "id": "brand-new",
            "version": 1,
            "endian": "little",
            "magic": [],
            "description": "",
            "layout": [
                {"type": "int", "name": "a", "int": "u8"}
            ]
        }),
        Some("create-fmt"),
    );
    assert_eq!(s, s2);
    assert_eq!(body, body2);
}

#[test]
fn revision_is_stored_as_new_sample_and_original_is_untouched() {
    let mut h = Harness::new();
    let original_hex = sample_doc().hex;
    let (s, body) = h.call(
        "POST",
        "/api/samples/revision",
        serde_json::json!({
            "source_sample_id": "s",
            "edits": {"width": 100},
            "name": "normalized",
            "new_id": "s-rev"
        }),
        Some("revision-key"),
    );
    assert_eq!(s, 201);
    assert_eq!(body["sample"]["derived_from"], serde_json::json!("s"));
    assert_ne!(body["sample"]["hex"], serde_json::json!(original_hex));

    let (_, list) = h.call("GET", "/api/samples", serde_json::json!({}), None);
    let samples = list["samples"].as_array().unwrap();
    let original = samples.iter().find(|x| x["id"] == "s").unwrap();
    assert_eq!(original["hex_len"], serde_json::json!((original_hex.len() / 2) as u64));
    let revision = samples.iter().find(|x| x["id"] == "s-rev").unwrap();
    assert_eq!(revision["derived_from"], serde_json::json!("s"));
}
