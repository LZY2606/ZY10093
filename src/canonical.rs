//! Deterministic canonicalization and fingerprints.
//!
//! Definitions arrive as arbitrary JSON. We parse into `serde_json::Value`,
//! sort object keys recursively, and emit compact UTF-8. The SHA-256 of that
//! document is the fingerprint used to freeze migration plans.
//! Definitions are never mutated in place: every save creates a new revision.

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut sorted: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
            for (k, val) in m {
                sorted.insert(k.clone(), canonicalize(val));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

pub fn canonical_json(v: &Value) -> String {
    let c = canonicalize(v);
    // serde_json with BTreeMap objects already emits keys in sorted order.
    serde_json::to_string(&c).expect("canonical serialization")
}

pub fn fingerprint(v: &Value) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(v).as_bytes());
    hex::encode(h.finalize())
}

pub fn fingerprint_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}

/// Parse an untrusted JSON document and return its canonical text + fingerprint.
pub fn normalize_document(raw: &str) -> Result<(Value, String, String), String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;
    let text = canonical_json(&v);
    let fp = fingerprint(&v);
    Ok((v, text, fp))
}
