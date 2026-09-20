//! HTTP surface. All rules live here server-side; the browser is a thin viewer.

use crate::canonical::{canonical_json, fingerprint};
use crate::migrate::{convert_one, run_dry_run, validate_rule, DryRunInput};
use crate::model::{FieldDef, FormatSpec, RuleSpec};
use crate::parse::{resolve_inheritance, validate_resolved, ParentLookup, Issue};
use crate::store::{ConvertedFile, Store, StoreError};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = Response::new(axum::body::Body::from(self.1));
        *res.status_mut() = self.0;
        res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        res
    }
}

fn json_ok<T: Serialize>(v: T) -> Response {
    let body = serde_json::to_vec_pretty(&v).unwrap();
    let mut res = Response::new(axum::body::Body::from(body));
    res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    res
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, serde_json::json!({"error": msg.into()}).to_string())
}

// --------------------------------------------------------- inheritance service

struct DbLookup<'a> {
    store: &'a Store,
}

impl<'a> ParentLookup for DbLookup<'a> {
    fn resolve_parent(&self, name: &str, revision: i64) -> Result<Vec<FieldDef>, Issue> {
        let row = self
            .store
            .def_at(name, revision)
            .map_err(|e| Issue::spec("parent_lookup", e.to_string()))?
            .ok_or_else(|| {
                Issue::spec("missing_parent", format!("parent {name} r{revision} not found"))
            })?;
        let spec: FormatSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| Issue::spec("parent_json", e.to_string()))?;
        // Resolve using the stored spec (with its own inherit pointer) so chains
        // recurse through every level; key identity is preserved by name.
        let stored = FormatSpec {
            name: spec.name.clone(),
            version: format!("r{}", row.revision),
            inherit: spec.inherit.clone(),
            fields: spec.fields,
        };
        let mut chain = Vec::new();
        let resolved = resolve_inheritance(&stored, self, &mut chain).map_err(|i| i)?;
        Ok(resolved.fields)
    }
}

/// Resolve a stored definition (flattening inheritance) and validate it.
fn load_resolved(store: &Store, name: &str, revision: i64) -> Result<(FormatSpec, Vec<Issue>), ApiError> {
    let row = store
        .def_at(name, revision)
        .map_err(|e| bad_request(e.to_string()))?
        .ok_or_else(|| bad_request(format!("definition {name} r{revision} not found")))?;
    let spec: FormatSpec = serde_json::from_str(&row.spec_json).map_err(|e| bad_request(e.to_string()))?;
    let lookup = DbLookup { store };
    let mut chain = Vec::new();
    let resolved = resolve_inheritance(&spec, &lookup, &mut chain).map_err(|i| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"errors": [i]}).to_string(),
        )
    })?;
    let issues = validate_resolved(&resolved);
    Ok((resolved, issues))
}

// ------------------------------------------------------------- DTOs

#[derive(Deserialize)]
struct SaveDefBody {
    #[serde(default)]
    base_revision: Option<i64>,
    spec: serde_json::Value,
}

#[derive(Deserialize)]
struct SaveRuleBody {
    #[serde(default)]
    base_revision: Option<i64>,
    rule: serde_json::Value,
}

#[derive(Deserialize)]
struct SampleBody {
    name: String,
    format_name: String,
    format_revision: i64,
    /// hex string
    hex: String,
}

#[derive(Deserialize)]
struct ParseQuery {
    name: String,
    revision: i64,
    sample: Option<String>,
    /// hex literal (used when no sample id supplied)
    hex: Option<String>,
}

#[derive(Deserialize)]
struct DryRunBody {
    rule_name: String,
    rule_revision: i64,
    #[serde(default)]
    samples: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct PlanCreateBody {
    name: String,
    rule_name: String,
    rule_revision: i64,
    dryrun: serde_json::Value,
    fingerprints: serde_json::Value,
}

#[derive(Deserialize)]
struct PlanTransitionBody {
    revision: i64,
    action: String, // freeze | publish | unfreeze | retire
    #[serde(default)]
    acceptances: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct BatchBody {
    plan_id: String,
    #[serde(default)]
    samples: Option<Vec<String>>,
}

// ------------------------------------------------------------- handlers: defs

async fn list_defs(State(s): State<AppState>) -> Response {
    match s.store.list_defs() {
        Ok(rows) => json_ok(serde_json::json!({"definitions": rows.iter().map(|(n,r,f)|
            serde_json::json!({"name":n,"revision":r,"fingerprint":f})).collect::<Vec<_>>()})),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn get_def(
    State(s): State<AppState>,
    Path((name, rev)): Path<(String, i64)>,
) -> Response {
    match s.store.def_at(&name, rev) {
        Ok(Some(row)) => {
            let spec: serde_json::Value = serde_json::from_str(&row.spec_json).unwrap_or(serde_json::Value::Null);
            let (resolved, issues) = match load_resolved(&s.store, &name, rev) {
                Ok((r, i)) => (Some(serde_json::to_value(&r).unwrap_or(serde_json::Value::Null)), i),
                Err(_) => (None, Vec::new()),
            };
            json_ok(serde_json::json!({
                "id": row.id, "name": row.name, "revision": row.revision,
                "fingerprint": row.fingerprint, "spec": spec,
                "resolved": resolved, "validation_issues": issues,
            }))
        }
        Ok(None) => ApiError(StatusCode::NOT_FOUND, serde_json::json!({"error":"not found"}).to_string()).into_response(),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn validate_def(
    State(s): State<AppState>,
    Path((name, rev)): Path<(String, i64)>,
) -> Response {
    match load_resolved(&s.store, &name, rev) {
        Ok((spec, issues)) => json_ok(serde_json::json!({
            "ok": issues.is_empty(),
            "resolved_fingerprint": fingerprint(&serde_json::to_value(&spec).unwrap()),
            "issues": issues,
        })),
        Err(e) => e.into_response(),
    }
}

async fn save_def(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<SaveDefBody>) -> Response {
    if let Some(resp) = idem_lookup(&s, &headers, "POST /api/defs") {
        return resp;
    }
    let raw = canonical_json(&body.spec);
    // Server-side validation BEFORE persisting: resolve inheritance (this also
    // rejects cycles and same-name shadowing) and run structural validation.
    {
        let spec: FormatSpec = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => return bad_request(format!("spec schema: {e}")).into_response(),
        };
        let lookup = DbLookup { store: &s.store };
        let mut chain = Vec::new();
        match resolve_inheritance(&spec, &lookup, &mut chain) {
            Ok(resolved) => {
                let issues = validate_resolved(&resolved);
                let hard: Vec<&Issue> = issues
                    .iter()
                    .filter(|i| {
                        !matches!(i.code.as_str(), "checksum_mismatch" | "pad_nonzero" | "fixed_mismatch")
                    })
                    .collect();
                if !hard.is_empty() {
                    return ApiError(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        serde_json::json!({"errors": hard}).to_string(),
                    )
                    .into_response();
                }
            }
            Err(issue) => {
                return ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({"errors": [issue]}).to_string(),
                )
                .into_response();
            }
        }
    }
    match s.store.insert_def(&raw, body.base_revision.or(Some(0))) {
        Ok(row) => {
            let out = serde_json::to_vec_pretty(&serde_json::json!({
                "id": row.id, "name": row.name, "revision": row.revision, "fingerprint": row.fingerprint
            })).unwrap();
            if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
                s.store.idem_put(key, "POST /api/defs", 200, &out, "application/json");
            }
            let mut res = Response::new(axum::body::Body::from(out));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            res
        }
        Err(StoreError::Conflict(body)) => {
            ApiError(StatusCode::CONFLICT, body).into_response()
        }
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

// ------------------------------------------------------------- idempotency helpers

fn idem_lookup(s: &AppState, h: &HeaderMap, route: &str) -> Option<Response> {
    let key = h.get("idempotency-key")?.to_str().ok()?.to_string();
    let (code, body, ct) = s.store.idem_get(&key)?;
    let mut res = Response::new(axum::body::Body::from(body));
    *res.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
    res.headers_mut().insert(header::CONTENT_TYPE, ct.parse().unwrap());
    let _ = route;
    Some(res)
}

async fn list_samples(State(s): State<AppState>) -> Response {
    match s.store.list_samples() {
        Ok(rows) => json_ok(serde_json::json!({"samples": rows.iter().map(|(id,n,f,r,sha)|
            serde_json::json!({"id":id,"name":n,"format":f,"revision":r,"sha256":sha})).collect::<Vec<_>>()})),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn upload_sample(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SampleBody>,
) -> Response {
    if let Some(resp) = idem_lookup(&s, &headers, "POST /api/samples") {
        return resp;
    }
    let cleaned: String = body.hex.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = match hex::decode(&cleaned) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid hex: {e}")).into_response(),
    };
    match s.store.insert_sample(&body.name, &body.format_name, body.format_revision, &bytes) {
        Ok(row) => {
            let out = serde_json::to_vec_pretty(&serde_json::json!({
                "id": row.id, "name": row.name, "sha256": row.sha256, "length": row.bytes.len()
            }))
            .unwrap();
            if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
                s.store.idem_put(key, "POST /api/samples", 200, &out, "application/json");
            }
            let mut res = Response::new(axum::body::Body::from(out));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            res
        }
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn get_sample_bytes(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.store.get_sample(&id) {
        Ok(Some(row)) => {
            let mut res = Response::new(axum::body::Body::from(row.bytes.clone()));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
            res
        }
        Ok(None) => ApiError(StatusCode::NOT_FOUND, "not found".into()).into_response(),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

// ------------------------------------------------------------- parse endpoint

#[derive(Serialize)]
struct ParseOut {
    ok: bool,
    spec_issues: Vec<Issue>,
    #[serde(flatten)]
    result: crate::parse::ParseResult,
    identity_hex: String,
    identity_identical: bool,
}

async fn parse(State(s): State<AppState>, Query(q): Query<ParseQuery>) -> Response {
    let (spec, spec_issues) = match load_resolved(&s.store, &q.name, q.revision) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let bytes = if let Some(id) = &q.sample {
        match s.store.get_sample(id) {
            Ok(Some(row)) => row.bytes,
            Ok(None) => return bad_request("sample not found").into_response(),
            Err(e) => return bad_request(e.to_string()).into_response(),
        }
    } else if let Some(h) = &q.hex {
        let cleaned: String = h.chars().filter(|c| !c.is_whitespace()).collect();
        match hex::decode(&cleaned) {
            Ok(b) => b,
            Err(e) => return bad_request(format!("bad hex: {e}")).into_response(),
        }
    } else {
        return bad_request("provide ?sample=<id> or ?hex=<hex>").into_response();
    };

    let result = crate::parse::parse_input(&spec, &bytes);
    let (identity_hex, identity_identical) = match crate::parse::write_identity(&result, &bytes) {
        Ok(b) => (hex::encode(&b), b == bytes),
        Err(e) => (String::new(), false_with_msg(&e)),
    };
    let _ = identity_identical;
    let ok = result.ok && identity_hex == hex::encode(&bytes);
    json_ok(ParseOut {
        ok,
        spec_issues,
        result,
        identity_hex,
        identity_identical: ok,
    })
}

fn false_with_msg(_e: &str) -> bool {
    false
}

// ------------------------------------------------------------- rules + dry-run

async fn list_rules(State(s): State<AppState>) -> Response {
    match s.store.list_rules() {
        Ok(rows) => json_ok(serde_json::json!({"rules": rows.iter().map(|(n,r,f)|
            serde_json::json!({"name":n,"revision":r,"fingerprint":f})).collect::<Vec<_>>()})),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn get_rule(State(s): State<AppState>, Path((name, rev)): Path<(String, i64)>) -> Response {
    match s.store.rule_at(&name, rev) {
        Ok(Some(row)) => json_ok(serde_json::json!({
            "id": row.id, "name": row.name, "revision": row.revision,
            "fingerprint": row.fingerprint,
            "rule": serde_json::from_str::<serde_json::Value>(&row.spec_json).unwrap_or(serde_json::Value::Null),
        })),
        Ok(None) => ApiError(StatusCode::NOT_FOUND, "not found".into()).into_response(),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn save_rule(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<SaveRuleBody>) -> Response {
    if let Some(resp) = idem_lookup(&s, &headers, "POST /api/rules") {
        return resp;
    }
    let raw = canonical_json(&body.rule);
    match s.store.insert_rule(&raw, body.base_revision.or(Some(0))) {
        Ok(row) => {
            let out = serde_json::to_vec_pretty(&serde_json::json!({
                "id": row.id, "name": row.name, "revision": row.revision, "fingerprint": row.fingerprint
            }))
            .unwrap();
            if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
                s.store.idem_put(key, "POST /api/rules", 200, &out, "application/json");
            }
            let mut res = Response::new(axum::body::Body::from(out));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            res
        }
        Err(StoreError::Conflict(b)) => ApiError(StatusCode::CONFLICT, b).into_response(),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn validate_rule_handler(
    State(s): State<AppState>,
    Path((name, rev)): Path<(String, i64)>,
) -> Response {
    let rule_row = match s.store.rule_at(&name, rev) {
        Ok(Some(r)) => r,
        Ok(None) => return ApiError(StatusCode::NOT_FOUND, "not found".into()).into_response(),
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let rule: RuleSpec = match serde_json::from_str(&rule_row.spec_json) {
        Ok(r) => r,
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let src = match load_resolved(&s.store, &rule.from_format, rule.from_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };
    let dst = match load_resolved(&s.store, &rule.to_format, rule.to_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };
    let issues = validate_rule(&rule, &src, &dst);
    json_ok(serde_json::json!({"ok": issues.is_empty(), "issues": issues}))
}

async fn dry_run(State(s): State<AppState>, Json(body): Json<DryRunBody>) -> Response {
    let rule_row = match s.store.rule_at(&body.rule_name, body.rule_revision) {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("rule not found").into_response(),
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let rule: RuleSpec = match serde_json::from_str(&rule_row.spec_json) {
        Ok(r) => r,
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let src = match load_resolved(&s.store, &rule.from_format, rule.from_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };
    let dst = match load_resolved(&s.store, &rule.to_format, rule.to_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };

    // pick samples
    let all = match s.store.list_samples() {
        Ok(v) => v,
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let mut samples: Vec<(String, Vec<u8>)> = Vec::new();
    let wanted: BTreeSet<String> = body.samples.map(|x| x.into_iter().collect()).unwrap_or_default();
    for (id, _name, fmt, rev, _sha) in all {
        if fmt != rule.from_format || rev != rule.from_revision {
            continue;
        }
        if !wanted.is_empty() && !wanted.contains(&id) {
            continue;
        }
        if let Ok(Some(row)) = s.store.get_sample(&id) {
            samples.push((id, row.bytes));
        }
    }

    let report = run_dry_run(DryRunInput {
        rule: &rule,
        rule_revision: rule_row.revision,
        src: &src,
        dst: &dst,
        samples: &samples,
    });
    json_ok(report)
}

// ------------------------------------------------------------- plans

async fn list_plans(State(s): State<AppState>) -> Response {
    match s.store.list_plans() {
        Ok(rows) => json_ok(serde_json::json!({"plans": rows.iter().map(|p| serde_json::json!({
            "id": p.id, "revision": p.revision, "name": p.name, "status": p.status,
            "rule_revision": p.rule_revision,
            "fingerprints": serde_json::from_str::<serde_json::Value>(&p.fingerprints_json).unwrap_or(serde_json::Value::Null),
            "acceptances": serde_json::from_str::<serde_json::Value>(&p.acceptances_json).unwrap_or(serde_json::Value::Null),
        })).collect::<Vec<_>>()})),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn get_plan(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.store.get_plan(&id) {
        Ok(Some(p)) => json_ok(serde_json::json!({
            "id": p.id, "revision": p.revision, "name": p.name, "status": p.status,
            "rule_id": p.rule_id, "rule_revision": p.rule_revision,
            "fingerprints": serde_json::from_str::<serde_json::Value>(&p.fingerprints_json).unwrap_or(serde_json::Value::Null),
            "acceptances": serde_json::from_str::<serde_json::Value>(&p.acceptances_json).unwrap_or(serde_json::Value::Null),
            "dryrun": serde_json::from_str::<serde_json::Value>(&p.dryrun_json).unwrap_or(serde_json::Value::Null),
        })),
        Ok(None) => ApiError(StatusCode::NOT_FOUND, "not found".into()).into_response(),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn create_plan(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<PlanCreateBody>) -> Response {
    if let Some(resp) = idem_lookup(&s, &headers, "POST /api/plans") {
        return resp;
    }
    let dry = canonical_json(&body.dryrun);
    let fps = canonical_json(&body.fingerprints);
    match s.store.create_plan(&body.name, &body.rule_name, body.rule_revision, &dry, &fps) {
        Ok(p) => {
            let out = serde_json::to_vec_pretty(&serde_json::json!({"id": p.id, "revision": p.revision, "status": p.status}))
                .unwrap();
            if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
                s.store.idem_put(key, "POST /api/plans", 200, &out, "application/json");
            }
            let mut res = Response::new(axum::body::Body::from(out));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            res
        }
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn transition_plan(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PlanTransitionBody>,
) -> Response {
    let new_status = match body.action.as_str() {
        "freeze" => "frozen",
        "unfreeze" => "draft",
        "publish" => "published",
        "retire" => "retired",
        other => return bad_request(format!("unknown action {other}")).into_response(),
    };
    let acc = body.acceptances.as_ref().map(|v| canonical_json(v));
    match s.store.transition_plan(&id, body.revision, new_status, acc.as_deref(), None) {
        Ok(p) => json_ok(serde_json::json!({"id": p.id, "revision": p.revision, "status": p.status})),
        Err(StoreError::Conflict(b)) => ApiError(StatusCode::CONFLICT, b).into_response(),
        Err(e) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error": e.to_string()}).to_string())
            .into_response(),
    }
}

// ------------------------------------------------------------- batches + export

async fn run_batch(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<BatchBody>) -> Response {
    if let Some(resp) = idem_lookup(&s, &headers, "POST /api/batches") {
        return resp;
    }
    let plan = match s.store.get_plan(&body.plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return ApiError(StatusCode::NOT_FOUND, "plan not found".into()).into_response(),
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    if plan.status != "published" {
        return ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": format!("plan is {}; only published plans run batches", plan.status)}).to_string(),
        )
        .into_response();
    }
    let rule_row = match s.store.rule_by_id(&plan.rule_id, plan.rule_revision) {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("bound rule revision missing").into_response(),
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let rule: RuleSpec = match serde_json::from_str(&rule_row.spec_json) {
        Ok(r) => r,
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let src = match load_resolved(&s.store, &rule.from_format, rule.from_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };
    let dst = match load_resolved(&s.store, &rule.to_format, rule.to_revision) {
        Ok((v, _)) => v,
        Err(e) => return e.into_response(),
    };

    // sample selection
    let all = match s.store.list_samples() {
        Ok(v) => v,
        Err(e) => return bad_request(e.to_string()).into_response(),
    };
    let wanted: BTreeSet<String> = body.samples.map(|x| x.into_iter().collect()).unwrap_or_default();
    let mut chosen: Vec<(String, String, Vec<u8>)> = Vec::new();
    for (id, _name, fmt, rev, _sha) in all {
        if fmt != rule.from_format || rev != rule.from_revision {
            continue;
        }
        if !wanted.is_empty() && !wanted.contains(&id) {
            continue;
        }
        if let Ok(Some(row)) = s.store.get_sample(&id) {
            chosen.push((id, row.name, row.bytes));
        }
    }
    if chosen.is_empty() {
        return bad_request("no matching samples to convert").into_response();
    }

    // Convert EVERYTHING first. Any single failure aborts before a batch row exists,
    // so no visible batch is ever produced on partial failure.
    let mut converted: Vec<ConvertedFile> = Vec::new();
    let mut failures: Vec<serde_json::Value> = Vec::new();
    for (id, name, bytes) in &chosen {
        match convert_one(&rule, &src, &dst, bytes) {
            Ok(out) => converted.push(ConvertedFile { sample_id: id.clone(), bytes: out }),
            Err(e) => failures.push(serde_json::json!({"sample_id": id, "sample_name": name, "error": e})),
        }
    }
    if !failures.is_empty() {
        return ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "error": "batch aborted: one or more files failed; no batch was created",
                "failures": failures,
                "succeeded": converted.len(),
                "attempted": chosen.len(),
            })
            .to_string(),
        )
        .into_response();
    }

    match s.store.commit_batch(&body.plan_id, converted) {
        Ok(b) => {
            let out = serde_json::to_vec_pretty(&serde_json::json!({
                "batch_id": b.id, "status": b.status, "count": b.count
            }))
            .unwrap();
            if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
                s.store.idem_put(key, "POST /api/batches", 200, &out, "application/json");
            }
            let mut res = Response::new(axum::body::Body::from(out));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            res
        }
        Err(e) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error": e.to_string()}).to_string())
            .into_response(),
    }
}

async fn list_batches(State(s): State<AppState>) -> Response {
    match s.store.list_batches(false) {
        Ok(rows) => json_ok(serde_json::json!({"batches": rows.iter().map(|b| serde_json::json!({
            "id": b.id, "plan_id": b.plan_id, "status": b.status, "count": b.count
        })).collect::<Vec<_>>()})),
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn export(State(s): State<AppState>) -> Response {
    match s.store.export_tar() {
        Ok(bytes) => {
            let mut res = Response::new(axum::body::Body::from(bytes));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/x-tar".parse().unwrap());
            res.headers_mut()
                .insert(header::CONTENT_DISPOSITION, "attachment; filename=workbench-export.tar".parse().unwrap());
            res
        }
        Err(e) => bad_request(e.to_string()).into_response(),
    }
}

async fn root_page() -> Response {
    let mut res = Response::new(axum::body::Body::from(include_str!("web/index.html")));
    res.headers_mut().insert(header::CONTENT_TYPE, "text/html; charset=utf-8".parse().unwrap());
    res
}

async fn app_js() -> Response {
    let mut res = Response::new(axum::body::Body::from(include_str!("web/app.js")));
    res.headers_mut().insert(header::CONTENT_TYPE, "application/javascript".parse().unwrap());
    res
}

async fn app_css() -> Response {
    let mut res = Response::new(axum::body::Body::from(include_str!("web/app.css")));
    res.headers_mut().insert(header::CONTENT_TYPE, "text/css".parse().unwrap());
    res
}

// ------------------------------------------------------------- server bootstrap

pub fn router(store: Arc<Store>) -> Router {
    let state = AppState { store };
    Router::new()
        .route("/", get(root_page))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/defs", get(list_defs).post(save_def))
        .route("/api/defs/{name}/{revision}", get(get_def))
        .route("/api/defs/{name}/{revision}/validate", get(validate_def))
        .route("/api/samples", get(list_samples).post(upload_sample))
        .route("/api/samples/{id}/bytes", get(get_sample_bytes))
        .route("/api/parse", get(parse))
        .route("/api/rules", get(list_rules).post(save_rule))
        .route("/api/rules/{name}/{revision}", get(get_rule))
        .route("/api/rules/{name}/{revision}/validate", get(validate_rule_handler))
        .route("/api/dryrun", post(dry_run))
        .route("/api/plans", get(list_plans).post(create_plan))
        .route("/api/plans/{id}", get(get_plan))
        .route("/api/plans/{id}/transition", post(transition_plan))
        .route("/api/batches", get(list_batches).post(run_batch))
        .route("/api/export.tar", get(export))
        .with_state(state)
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut listen = "127.0.0.1:5219".to_string();
    let mut db = "workbench.db".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                listen = args.get(i).cloned().unwrap_or_else(|| "--listen needs a value".into());
            }
            a if let Some(v) = a.strip_prefix("--listen=") => {
                listen = v.to_string();
            }
            "--db" => {
                i += 1;
                db = args.get(i).cloned().unwrap_or_else(|| "--db needs a value".into());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let store = Arc::new(Store::open(&db).unwrap_or_else(|e| {
        eprintln!("cannot open database {db}: {e}");
        std::process::exit(1);
    }));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
            eprintln!("cannot bind {listen}: {e}");
            std::process::exit(1);
        });
        eprintln!("binfmt-workbench listening on http://{listen} (db: {db})");
        axum::serve(listener, router(store)).with_graceful_shutdown(shutdown()).await.unwrap();
    });
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("shutting down");
}

// exposed for integration tests
pub fn build_test_state(path: &str) -> Arc<Store> {
    Arc::new(Store::open(path).expect("open store"))
}
