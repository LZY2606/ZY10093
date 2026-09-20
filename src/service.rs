use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::migration::{dry_run, dry_to_json, accepted_losses_cover, MigrationCtx, RuleDoc};
use crate::model::*;
use crate::parser::{outcome_to_json, Parser};
use crate::store::*;
use crate::util::{fnv_fingerprint, parse_hex, to_hex};
use crate::writer::{ranges_to_json, write, WriteRequest};

pub struct Registry {
    compiled: BTreeMap<String, Compiled>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: &Value) -> Response {
        Response {
            status,
            content_type: "application/json; charset=utf-8".to_string(),
            body: serde_json::to_vec(value).unwrap_or_default(),
        }
    }
    pub fn error(status: u16, code: &str, message: &str) -> Response {
        Self::json(
            status,
            &json!({ "ok": false, "error": code, "message": message }),
        )
    }
}

struct StateLookup<'a>(&'a State);
impl<'a> FormatLookup for StateLookup<'a> {
    fn get(&self, id: &str, version: u32) -> Option<&FormatDoc> {
        self.0.format(id, version)
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            compiled: BTreeMap::new(),
        }
    }

    pub fn compile(&mut self, state: &State, id: &str, version: u32) -> Result<Compiled, Response> {
        let key = format!("{id}@{version}");
        if let Some(compiled) = self.compiled.get(&key) {
            return Ok(compiled.clone());
        }
        let doc = state
            .format(id, version)
            .ok_or_else(|| Response::error(404, "format_not_found", &format!("{key} not found")))?;
        let compiled = compile(&StateLookup(state), doc).map_err(|e| {
            Response::json(
                400,
                &json!({ "ok": false, "error": "format_invalid", "message": e.to_string() }),
            )
        })?;
        self.compiled.insert(key, compiled.clone());
        Ok(compiled)
    }

    pub fn compile_doc(
        &mut self,
        state: &State,
        doc: &FormatDoc,
    ) -> Result<Compiled, Response> {
        let key = format!("{}@{}", doc.id, doc.version);
        if let Some(compiled) = self.compiled.get(&key) {
            return Ok(compiled.clone());
        }
        let compiled = compile(&StateLookup(state), doc).map_err(|e| {
            Response::json(
                400,
                &json!({ "ok": false, "error": "format_invalid", "message": e.to_string() }),
            )
        })?;
        self.compiled.insert(key, compiled.clone());
        Ok(compiled)
    }

    pub fn invalidate(&mut self) {
        self.compiled.clear();
    }
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        _ => "Internal Server Error",
    }
}

fn idem_key(req: &Request) -> Option<String> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("idempotency-key"))
        .map(|(_, v)| v.clone())
}

pub fn handle(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    if req.method == "GET" && (req.path == "/" || req.path == "/index.html") {
        return Response {
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            body: crate::web::INDEX_HTML.to_vec(),
        };
    }
    if req.method == "GET" && req.path == "/app.js" {
        return Response {
            status: 200,
            content_type: "application/javascript; charset=utf-8".to_string(),
            body: crate::web::APP_JS.to_vec(),
        };
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/health") => Response::json(200, &json!({ "ok": true })),
        ("GET", "/api/formats") => list_formats(store),
        ("PUT", "/api/formats") => put_format(store, registry, req),
        ("GET", "/api/samples") => list_samples(store),
        ("PUT", "/api/samples") => put_sample(store, req),
        ("POST", "/api/parse") => parse_sample(store, registry, req),
        ("POST", "/api/emit") => emit_sample(store, registry, req),
        ("POST", "/api/samples/revision") => save_revision(store, registry, req),
        ("GET", "/api/diff") => diff_versions(store, registry, req),
        ("GET", "/api/rules") => list_rules(store),
        ("PUT", "/api/rules") => put_rule(store, registry, req),
        ("POST", "/api/migrate/dry-run") => migrate_dry(store, registry, req),
        ("GET", "/api/plans") => list_plans(store),
        ("PUT", "/api/plans") => save_plan(store, req),
        ("POST", "/api/plans/publish") => publish_plan(store, registry, req),
        ("POST", "/api/plans/batch") => run_batch(store, registry, req),
        ("GET", "/api/export") => export_bundle(store),
        _ => Response::error(404, "not_found", "unknown route"),
    }
}

fn json_body(req: &Request) -> Result<Value, Response> {
    serde_json::from_slice(&req.body).map_err(|e| {
        Response::error(400, "invalid_json", &format!("request body is not JSON: {e}"))
    })
}

fn store_error(resp: StoreError) -> Response {
    Response::json(resp.status, &resp.body)
}

fn list_formats(store: &Store) -> Response {
    let state = store.snapshot();
    let value = json!({
        "ok": true,
        "formats": state.formats.values().flat_map(|v| v.iter()).map(|d| {
            json!({ "id": d.id, "version": d.version, "description": d.description })
        }).collect::<Vec<_>>()
    });
    Response::json(200, &value)
}

fn put_format(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let doc: FormatDoc = match serde_json::from_value(body) {
        Ok(d) => d,
        Err(e) => return Response::error(400, "invalid_format", &e.to_string()),
    };
    let state = store.snapshot();
    let compiled = match registry.compile_doc(&state, &doc) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if state.format(&doc.id, doc.version).is_none() {
        registry.invalidate();
    }
    match store.upsert_format(doc.clone(), idem_key(req).as_deref()) {
        Ok((status, mut body)) => {
            body["ok"] = json!(true);
            body["fingerprint"] = json!(compiled.fingerprint);
            Response::json(status, &body)
        }
        Err(err) => store_error(err),
    }
}

fn list_samples(store: &Store) -> Response {
    let state = store.snapshot();
    Response::json(
        200,
        &json!({
            "ok": true,
            "samples": state.samples.values().map(|s| json!({
                "id": s.id,
                "rev": s.rev,
                "name": s.name,
                "format": s.format,
                "hex_len": s.hex.len() / 2,
                "derived_from": s.derived_from,
            })).collect::<Vec<_>>()
        }),
    )
}

fn put_sample(store: &Store, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut doc: SampleDoc = match serde_json::from_value(body) {
        Ok(d) => d,
        Err(e) => return Response::error(400, "invalid_sample", &e.to_string()),
    };
    if parse_hex(&doc.hex).is_err() {
        return Response::error(400, "invalid_hex", "sample hex is invalid");
    }
    doc.rev = store.snapshot().samples.get(&doc.id).map(|s| s.rev).unwrap_or(0);
    match store.upsert_sample(doc, idem_key(req).as_deref()) {
        Ok((status, body)) => Response::json(status, &json!({ "ok": true, "sample": body })),
        Err(err) => store_error(err),
    }
}

struct InputRef {
    format_id: String,
    version: u32,
    hex: String,
}

fn save_revision(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let source_id = match body.get("source_sample_id").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => return Response::error(400, "missing_source", "source_sample_id is required"),
    };
    let state = store.snapshot();
    let Some(source) = state.samples.get(&source_id).cloned() else {
        return Response::error(404, "sample_not_found", &source_id);
    };
    let format = match body.get("format").cloned() {
        Some(v) => match serde_json::from_value::<Ref>(v) {
            Ok(r) => r,
            Err(e) => return Response::error(400, "invalid_format_ref", &e.to_string()),
        },
        None => source.format.clone(),
    };
    let input = InputRef {
        format_id: format.id.clone(),
        version: format.version,
        hex: source.hex.clone(),
    };
    let data = match parse_hex(&input.hex) {
        Ok(v) => v,
        Err(e) => return Response::error(400, "invalid_hex", &e),
    };
    let compiled = match registry.compile(&state, &input.format_id, input.version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let parsed = Parser::parse(&compiled, &data);
    if !parsed.errors.is_empty() {
        return Response::json(422, &json!({
            "ok": false,
            "error": "source_parse_failed",
            "errors": parsed.errors.iter().map(|e| e.to_json()).collect::<Vec<_>>()
        }));
    }
    let edits: BTreeMap<String, Value> = body
        .get("edits")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let result = match write(WriteRequest {
        compiled: &compiled,
        tree: &parsed.tree,
        edits,
        original: Some(&data),
    }) {
        Ok(v) => v,
        Err(errs) => {
            return Response::json(422, &json!({
                "ok": false,
                "error": "write_failed",
                "errors": errs.iter().map(|e| e.to_json()).collect::<Vec<_>>()
            }))
        }
    };
    let revision = SampleDoc {
        id: body
            .get("new_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("{}.rev{}", source.id, source.rev + 1)),
        rev: 0,
        format,
        name: body
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("{} (revision)", source.name)),
        hex: to_hex(&result.bytes),
        note: body
            .get("note")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| "normalization/revision stored as a new immutable sample".to_string()),
        derived_from: Some(source_id),
    };
    match store.upsert_sample(revision, idem_key(req).as_deref()) {
        Ok((status, value)) => Response::json(status, &json!({
            "ok": true,
            "sample": value,
            "byte_identical": result.bytes == data,
            "auto_fields": result.auto_fields,
        })),
        Err(err) => store_error(err),
    }
}

fn input_ref(store: &Store, value: &Value) -> Result<InputRef, Response> {
    if let Some(sample_id) = value.get("sample_id").and_then(|v| v.as_str()) {
        let state = store.snapshot();
        let sample = state
            .samples
            .get(sample_id)
            .ok_or_else(|| Response::error(404, "sample_not_found", sample_id))?;
        return Ok(InputRef {
            format_id: sample.format.id.clone(),
            version: sample.format.version,
            hex: sample.hex.clone(),
        });
    }
    let format = value
        .get("format")
        .ok_or_else(|| Response::error(400, "missing_format", "format ref required"))?;
    Ok(InputRef {
        format_id: format.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        version: format.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        hex: value.get("hex").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

fn parse_sample(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let input = match input_ref(store, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let data = match parse_hex(&input.hex) {
        Ok(v) => v,
        Err(e) => return Response::error(400, "invalid_hex", &e),
    };
    let state = store.snapshot();
    let compiled = match registry.compile(&state, &input.format_id, input.version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let outcome = Parser::parse(&compiled, &data);
    let ok = outcome.errors.is_empty();
    let mut value = outcome_to_json(&outcome, ok);
    value["format_id"] = json!(input.format_id);
    value["version"] = json!(input.version);
    value["fingerprint"] = json!(compiled.fingerprint);
    value["input_hex"] = json!(input.hex);
    Response::json(if ok { 200 } else { 422 }, &value)
}

fn emit_sample(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let input = match input_ref(store, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let data = match parse_hex(&input.hex) {
        Ok(v) => v,
        Err(e) => return Response::error(400, "invalid_hex", &e),
    };
    let state = store.snapshot();
    let compiled = match registry.compile(&state, &input.format_id, input.version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let parsed = Parser::parse(&compiled, &data);
    if !parsed.errors.is_empty() {
        return Response::json(
            422,
            &json!({
                "ok": false,
                "error": "source_parse_failed",
                "errors": parsed.errors.iter().map(|e| e.to_json()).collect::<Vec<_>>()
            }),
        );
    }
    let edits: BTreeMap<String, Value> = body
        .get("edits")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let preserve = body.get("preserve_original").and_then(|v| v.as_bool()).unwrap_or(true);
    let result = match write(WriteRequest {
        compiled: &compiled,
        tree: &parsed.tree,
        edits,
        original: if preserve { Some(data.as_slice()) } else { None },
    }) {
        Ok(v) => v,
        Err(errs) => {
            return Response::json(
                422,
                &json!({
                    "ok": false,
                    "error": "write_failed",
                    "errors": errs.iter().map(|e| e.to_json()).collect::<Vec<_>>()
                }),
            )
        }
    };
    let byte_identical = result.bytes == data;
    Response::json(
        200,
        &json!({
            "ok": true,
            "hex": to_hex(&result.bytes),
            "byte_identical": byte_identical,
            "auto_fields": result.auto_fields.iter().cloned().collect::<Vec<_>>(),
            "write_ranges": ranges_to_json(&result.ranges),
            "read_ranges": collect_read_ranges(&parsed.tree),
        }),
    )
}

fn collect_read_ranges(nodes: &[crate::parser::Node]) -> Value {
    let mut out = Vec::new();
    fn walk(nodes: &[crate::parser::Node], out: &mut Vec<Value>) {
        for node in nodes {
            out.push(json!({
                "path": node.path,
                "kind": node.kind,
                "start": node.start,
                "end": node.end,
                "identified": node.identified,
            }));
            walk(&node.children, out);
        }
    }
    walk(nodes, &mut out);
    json!(out)
}

fn diff_versions(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let hex = req.query.get("hex").cloned().unwrap_or_default();
    let sample_id = req.query.get("sample_id").cloned();
    let a_id = req.query.get("from_id").cloned().unwrap_or_default();
    let a_version: u32 = req.query.get("from_version").and_then(|v| v.parse().ok()).unwrap_or(0);
    let b_id = req.query.get("to_id").cloned().unwrap_or_default();
    let b_version: u32 = req.query.get("to_version").and_then(|v| v.parse().ok()).unwrap_or(0);

    let hex = if let Some(sid) = sample_id {
        let state = store.snapshot();
        match state.samples.get(&sid) {
            Some(s) => s.hex.clone(),
            None => return Response::error(404, "sample_not_found", &sid),
        }
    } else {
        hex
    };
    let data = match parse_hex(&hex) {
        Ok(v) => v,
        Err(e) => return Response::error(400, "invalid_hex", &e),
    };
    let state = store.snapshot();
    let from = match registry.compile(&state, &a_id, a_version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let to = match registry.compile(&state, &b_id, b_version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let outcome_a = Parser::parse(&from, &data);
    let outcome_b = Parser::parse(&to, &data);
    let mut values_a = BTreeMap::new();
    let mut values_b = BTreeMap::new();
    collect_flat(&outcome_a.tree, &mut values_a);
    collect_flat(&outcome_b.tree, &mut values_b);
    let paths: std::collections::BTreeSet<String> =
        values_a.keys().chain(values_b.keys()).cloned().collect();
    let mut field_diffs = Vec::new();
    for path in paths {
        let a = values_a.get(&path);
        let b = values_b.get(&path);
        let kind = match (a, b) {
            (Some(x), Some(y)) if x == y => "equal",
            (Some(_), Some(_)) => "changed",
            (Some(_), None) => "removed",
            (None, Some(_)) => "added",
            _ => continue,
        };
        field_diffs.push(json!({
            "path": path,
            "kind": kind,
            "from": a,
            "to": b,
        }));
    }
    Response::json(
        200,
        &json!({
            "ok": true,
            "from": { "id": a_id, "version": a_version, "fingerprint": from.fingerprint,
                      "tree": outcome_to_json(&outcome_a, outcome_a.errors.is_empty()) },
            "to": { "id": b_id, "version": b_version, "fingerprint": to.fingerprint,
                    "tree": outcome_to_json(&outcome_b, outcome_b.errors.is_empty()) },
            "field_diffs": field_diffs,
        }),
    )
}

fn collect_flat(nodes: &[crate::parser::Node], map: &mut BTreeMap<String, Value>) {
    for node in nodes {
        if let Some(value) = &node.value {
            map.insert(node.path.clone(), value.clone());
        }
        collect_flat(&node.children, map);
    }
}

fn list_rules(store: &Store) -> Response {
    let state = store.snapshot();
    Response::json(
        200,
        &json!({
            "ok": true,
            "rules": state.rules.values().map(|s| json!({
                "doc": s.doc,
                "rev": s.rev,
                "fingerprint": rule_fingerprint(&s.doc),
            })).collect::<Vec<_>>()
        }),
    )
}

fn rule_fingerprint(rule: &RuleDoc) -> String {
    fnv_fingerprint(&serde_json::to_value(rule).unwrap_or(Value::Null))
}

fn put_rule(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let rule_value = body.get("doc").cloned().unwrap_or_else(|| body.clone());
    let doc: RuleDoc = match serde_json::from_value(rule_value) {
        Ok(d) => d,
        Err(e) => return Response::error(400, "invalid_rule", &e.to_string()),
    };
    let expected_rev = body
        .get("rev")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u64;
    let state = store.snapshot();
    if let Err(r) = registry.compile(&state, &doc.from.id, doc.from.version) {
        return r;
    }
    if let Err(r) = registry.compile(&state, &doc.to.id, doc.to.version) {
        return r;
    }
    match store.upsert_rule(doc, expected_rev, idem_key(req).as_deref()) {
        Ok((status, mut body)) => {
            body["ok"] = json!(true);
            Response::json(status, &body)
        }
        Err(err) => store_error(err),
    }
}

fn migrate_dry(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let input = match input_ref(store, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let rule_ref = body
        .get("rule")
        .ok_or_else(|| Response::error(400, "missing_rule", "rule ref required"))
        .unwrap();
    let rule_id = rule_ref.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let rule_version = rule_ref.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let data = match parse_hex(&input.hex) {
        Ok(v) => v,
        Err(e) => return Response::error(400, "invalid_hex", &e),
    };
    let state = store.snapshot();
    let rule = match state.rule(rule_id, rule_version) {
        Some(r) => r.clone(),
        None => return Response::error(404, "rule_not_found", &format!("{rule_id}@v{rule_version}")),
    };
    let from = match registry.compile(&state, &rule.from.id, rule.from.version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let to = match registry.compile(&state, &rule.to.id, rule.to.version) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ctx = MigrationCtx { rule: &rule, from: &from, to: &to };
    match dry_run(&ctx, &data) {
        Ok(result) => {
            let mut value = dry_to_json(&result);
            value["ok"] = json!(true);
            value["rule_fingerprint"] = json!(rule_fingerprint(&rule));
            Response::json(200, &value)
        }
        Err(errs) => Response::json(
            422,
            &json!({
                "ok": false,
                "error": "migration_failed",
                "errors": errs.iter().map(|e| json!({"code": e.code, "message": e.message})).collect::<Vec<_>>()
            }),
        ),
    }
}

fn list_plans(store: &Store) -> Response {
    let state = store.snapshot();
    Response::json(
        200,
        &json!({
            "ok": true,
            "plans": state.plans.values().cloned().collect::<Vec<_>>()
        }),
    )
}

fn save_plan(store: &Store, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let plan_value = body.get("plan").cloned().unwrap_or_else(|| body.clone());
    let mut plan: PlanDoc = match serde_json::from_value(plan_value) {
        Ok(p) => p,
        Err(e) => return Response::error(400, "invalid_plan", &e.to_string()),
    };
    if plan.state != PlanState::Draft {
        return Response::error(
            409,
            "illegal_transition",
            "use the publish endpoint to freeze a draft plan",
        );
    }
    plan.batches = Vec::new();
    let expected_rev = body.get("rev").and_then(|v| v.as_u64()).unwrap_or(0) as u64;
    match store.save_plan(plan, expected_rev, idem_key(req).as_deref()) {
        Ok((status, body)) => Response::json(status, &json!({ "ok": true, "plan": body })),
        Err(err) => store_error(err),
    }
}

fn load_migration<'a>(
    registry: &'a mut Registry,
    state: &'a State,
    rule_ref: &Ref,
) -> Result<(Compiled, RuleDoc, Compiled, String), Response> {
    let rule = state
        .rule(&rule_ref.id, rule_ref.version)
        .cloned()
        .ok_or_else(|| Response::error(404, "rule_not_found", &rule_ref.id))?;
    let from = registry.compile(state, &rule.from.id, rule.from.version)?;
    let to = registry.compile(state, &rule.to.id, rule.to.version)?;
    let fingerprint = rule_fingerprint(&rule);
    Ok((from, rule, to, fingerprint))
}

fn publish_plan(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut plan: PlanDoc = match serde_json::from_value(body.get("plan").cloned().unwrap_or(body.clone())) {
        Ok(p) => p,
        Err(e) => return Response::error(400, "invalid_plan", &e.to_string()),
    };
    let expected_rev = body.get("rev").and_then(|v| v.as_u64()).unwrap_or(0) as u64;
    if plan.state != PlanState::Draft {
        return Response::error(409, "illegal_transition", "only draft plans can be published");
    }
    let sample_ids: Vec<String> = body
        .get("sample_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let state = store.snapshot();
    let rule_ref = plan.rule.clone();
    let (from, rule, to, rule_fp) = match load_migration(registry, &state, &rule_ref) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut validation = Vec::new();
    let mut failures = Vec::new();
    for sid in &sample_ids {
        let Some(sample) = state.samples.get(sid) else {
            failures.push(json!({ "sample_id": sid, "error": "sample_not_found" }));
            continue;
        };
        let Ok(data) = parse_hex(&sample.hex) else {
            failures.push(json!({ "sample_id": sid, "error": "invalid_hex" }));
            continue;
        };
        let ctx = MigrationCtx { rule: &rule, from: &from, to: &to };
        match dry_run(&ctx, &data) {
            Ok(result) => {
                if let Err(diff) = accepted_losses_cover(&result, &plan.accepted_losses, &rule) {
                    failures.push(json!({ "sample_id": sid, "error": "loss_not_accepted", "details": diff }));
                }
                validation.push(json!({
                    "sample_id": sid,
                    "equivalence": match result.equivalence {
                        crate::migration::Equivalence::Strict => "strict",
                        crate::migration::Equivalence::Semantic => "semantic",
                        crate::migration::Equivalence::Lossy => "lossy",
                    }
                }));
            }
            Err(errs) => failures.push(json!({
                "sample_id": sid,
                "error": "dry_run_failed",
                "details": errs.iter().map(|e| json!({"code": e.code, "message": e.message})).collect::<Vec<_>>()
            })),
        }
    }
    if !failures.is_empty() {
        return Response::json(
            422,
            &json!({ "ok": false, "error": "publish_rejected", "failures": failures, "validation": validation }),
        );
    }
    plan.state = PlanState::Published;
    plan.batches = Vec::new();
    plan.fingerprints = BTreeMap::from([
        ("rule".to_string(), rule_fp),
        (format!("format:{}@v{}", from.doc.id, from.doc.version), from.fingerprint.clone()),
        (format!("format:{}@v{}", to.doc.id, to.doc.version), to.fingerprint.clone()),
    ]);
    match store.save_plan(plan, expected_rev, idem_key(req).as_deref()) {
        Ok((status, body)) => Response::json(status, &json!({
            "ok": true, "plan": body, "validation": validation
        })),
        Err(err) => store_error(err),
    }
}

fn run_batch(store: &Store, registry: &mut Registry, req: &Request) -> Response {
    let body = match json_body(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let plan_id = body.get("plan_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let sample_ids: Vec<String> = body
        .get("sample_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let state = store.snapshot();
    let Some(existing_plan) = state.plans.get(&plan_id).cloned() else {
        return Response::error(404, "plan_not_found", &plan_id);
    };
    if existing_plan.state != PlanState::Published {
        return Response::error(
            409,
            "illegal_transition",
            "batch requires a published frozen plan",
        );
    }
    let (from, rule, to, rule_fp) = match load_migration(registry, &state, &existing_plan.rule) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if existing_plan.fingerprints.get("rule") != Some(&rule_fp)
        || existing_plan.fingerprints.get(&format!("format:{}@v{}", from.doc.id, from.doc.version))
            != Some(&from.fingerprint)
        || existing_plan.fingerprints.get(&format!("format:{}@v{}", to.doc.id, to.doc.version))
            != Some(&to.fingerprint)
    {
        return Response::error(
            409,
            "fingerprint_drift",
            "a definition changed after the plan was frozen",
        );
    }

    let mut outputs = BTreeMap::new();
    let mut equivalences: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for sid in &sample_ids {
        let Some(sample) = state.samples.get(sid) else {
            failures.push(json!({ "sample_id": sid, "error": "sample_not_found" }));
            continue;
        };
        let Ok(data) = parse_hex(&sample.hex) else {
            failures.push(json!({ "sample_id": sid, "error": "invalid_hex" }));
            continue;
        };
        let ctx = MigrationCtx { rule: &rule, from: &from, to: &to };
        match dry_run(&ctx, &data) {
            Ok(result) => {
                if let Err(diff) = accepted_losses_cover(&result, &existing_plan.accepted_losses, &rule) {
                    failures.push(json!({ "sample_id": sid, "error": "loss_not_accepted", "details": diff }));
                    continue;
                }
                outputs.insert(sid.clone(), to_hex(&result.forward_bytes));
                equivalences.insert(
                    match result.equivalence {
                        crate::migration::Equivalence::Strict => "strict",
                        crate::migration::Equivalence::Semantic => "semantic",
                        crate::migration::Equivalence::Lossy => "lossy",
                    }
                    .to_string(),
                );
            }
            Err(errs) => failures.push(json!({
                "sample_id": sid,
                "error": "dry_run_failed",
                "details": errs.iter().map(|e| json!({"code": e.code, "message": e.message})).collect::<Vec<_>>()
            })),
        }
    }
    if !failures.is_empty() {
        return Response::json(
            422,
            &json!({ "ok": false, "error": "batch_aborted_no_outputs", "failures": failures }),
        );
    }
    let equivalence = if equivalences.contains("lossy") {
        "lossy"
    } else if equivalences.contains("semantic") {
        "semantic"
    } else {
        "strict"
    };
    let batch = BatchDoc {
        id: format!("batch-{}", existing_plan.batches.len() + 1),
        sample_ids,
        outputs,
        equivalence: equivalence.to_string(),
    };
    match store.add_batch_idempotent(&plan_id, batch.clone(), idem_key(req).as_deref()) {
        Ok((status, body)) => Response::json(status, &json!({ "ok": true, "batch": body })),
        Err(err) => store_error(err),
    }
}

fn export_bundle(store: &Store) -> Response {
    let state = store.snapshot();
    let formats: Vec<Value> = state
        .formats
        .values()
        .flat_map(|v| v.iter())
        .map(|doc| {
            let compiled = compile(&StateLookup(&state), doc).ok();
            json!({
                "id": doc.id,
                "version": doc.version,
                "description": doc.description,
                "definition": doc,
                "fingerprint": compiled.as_ref().map(|c| c.fingerprint.clone()),
            })
        })
        .collect();
    let rules: Vec<Value> = state
        .rules
        .values()
        .map(|stored| {
            json!({
                "rev": stored.rev,
                "definition": stored.doc,
                "fingerprint": rule_fingerprint(&stored.doc),
            })
        })
        .collect();
    let samples: Vec<Value> = state
        .samples
        .values()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect();
    let plans: Vec<Value> = state.plans.values().map(|p| serde_json::to_value(p).unwrap()).collect();
    let bundle = json!({
        "schema": "bfw-export/1",
        "formats": formats,
        "rules": rules,
        "samples": samples,
        "plans": plans,
    });
    let canonical = crate::util::canonical_json(&bundle);
    let checksum = crate::util::fnv_fingerprint(&bundle);
    let value = json!({
        "ok": true,
        "bundle": bundle,
        "fingerprint": checksum,
        "canonical_sha_note": "fnv1a-64 over canonical compact JSON with sorted object keys",
        "canonical_len": canonical.len(),
    });
    Response::json(200, &value)
}
