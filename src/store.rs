use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::migration::{AcceptedLoss, RuleDoc};
use crate::model::FormatDoc;
use crate::util::crc32_ieee;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleDoc {
    pub id: String,
    pub rev: u64,
    pub format: crate::model::Ref,
    pub name: String,
    pub hex: String,
    pub note: String,
    pub derived_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDoc {
    pub id: String,
    pub rev: u64,
    pub rule: crate::model::Ref,
    pub state: PlanState,
    #[serde(default)]
    pub accepted_losses: Vec<AcceptedLoss>,
    #[serde(default)]
    pub fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    pub batches: Vec<BatchDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDoc {
    pub id: String,
    pub sample_ids: Vec<String>,
    pub outputs: BTreeMap<String, String>,
    pub equivalence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub seq: u64,
    pub formats: BTreeMap<String, Vec<FormatDoc>>,
    pub rules: BTreeMap<String, StoredRule>,
    pub samples: BTreeMap<String, SampleDoc>,
    pub plans: BTreeMap<String, PlanDoc>,
    pub idempotency: BTreeMap<String, IdempotentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotentRecord {
    pub status: u16,
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    FormatUpserted { doc: FormatDoc },
    RuleUpserted { doc: RuleDoc, rev: u64 },
    SampleUpserted { doc: SampleDoc },
    PlanUpserted { doc: PlanDoc },
    IdempotencyRecorded { key: String, status: u16, body: Value },
}

impl State {
    pub fn format(&self, id: &str, version: u32) -> Option<&FormatDoc> {
        self.formats.get(id).and_then(|v| v.iter().find(|d| d.version == version))
    }
    pub fn latest_format(&self, id: &str) -> Option<&FormatDoc> {
        self.formats.get(id).and_then(|v| v.last())
    }
    pub fn rule(&self, id: &str, version: u32) -> Option<&RuleDoc> {
        self.rules.get(id).filter(|s| s.doc.version == version).map(|s| &s.doc)
    }
    pub fn latest_rule(&self, id: &str) -> Option<&RuleDoc> {
        self.rules.get(id).map(|s| &s.doc)
    }

    fn apply(&mut self, event: Event) {
        self.seq += 1;
        match event {
            Event::FormatUpserted { doc } => {
                let entry = self.formats.entry(doc.id.clone()).or_default();
                if let Some(existing) = entry.iter_mut().find(|d| d.version == doc.version) {
                    *existing = doc;
                } else {
                    entry.push(doc);
                    entry.sort_by_key(|d| d.version);
                }
            }
            Event::RuleUpserted { doc, rev } => {
                self.rules.insert(doc.id.clone(), StoredRule { doc, rev });
            }
            Event::SampleUpserted { doc } => {
                self.samples.insert(doc.id.clone(), doc);
            }
            Event::PlanUpserted { doc } => {
                self.plans.insert(doc.id.clone(), doc);
            }
            Event::IdempotencyRecorded { key, status, body } => {
                self.idempotency.insert(key, IdempotentRecord { status, body });
            }
        }
    }
}

pub struct Store {
    state: Mutex<State>,
    dir: PathBuf,
}

#[derive(Debug)]
pub struct StoreError {
    pub status: u16,
    pub body: Value,
}

impl StoreError {
    pub fn conflict(message: &str, diff: Value) -> Self {
        StoreError {
            status: 409,
            body: serde_json::json!({ "error": "conflict", "message": message, "diff": diff }),
        }
    }
    pub fn bad(message: &str) -> Self {
        StoreError {
            status: 400,
            body: serde_json::json!({ "error": "bad_request", "message": message }),
        }
    }
    pub fn not_found(message: &str) -> Self {
        StoreError {
            status: 404,
            body: serde_json::json!({ "error": "not_found", "message": message }),
        }
    }
    pub fn unprocessable(message: &str, details: Value) -> Self {
        StoreError {
            status: 422,
            body: serde_json::json!({ "error": "unprocessable", "message": message, "details": details }),
        }
    }
    pub fn illegal(message: &str) -> Self {
        StoreError {
            status: 409,
            body: serde_json::json!({ "error": "illegal_transition", "message": message }),
        }
    }
}

fn wal_path(dir: &Path) -> PathBuf {
    dir.join("wal.log")
}
fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join("snapshot.json")
}
fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

const SNAPSHOT_EVERY: u64 = 50;

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Store> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let state = Self::recover(&dir)?;
        Ok(Store {
            state: Mutex::new(state),
            dir,
        })
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn recover(dir: &Path) -> std::io::Result<State> {
        let mut state = if snapshot_path(dir).exists() {
            let bytes = fs::read(snapshot_path(dir))?;
            serde_json::from_slice(&bytes).unwrap_or_default()
        } else {
            State::default()
        };
        let path = wal_path(dir);
        if path.exists() {
            let bytes = fs::read(&path)?;
            let mut pos = 0usize;
            while pos + 12 <= bytes.len() {
                let magic = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
                let crc = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());
                if magic != 0x5741_4c31 {
                    break;
                }
                if pos + 12 + len > bytes.len() {
                    break;
                }
                let payload = &bytes[pos + 12..pos + 12 + len];
                if crc32_ieee(payload) as u32 != crc {
                    break;
                }
                match serde_json::from_slice::<Event>(payload) {
                    Ok(event) => state.apply(event),
                    Err(_) => break,
                }
                pos += 12 + len;
            }
        }
        Ok(state)
    }
}

impl Store {
    fn append(&self, state: &mut State, event: Event) -> std::io::Result<()> {
        let payload = serde_json::to_vec(&event).expect("event serializes");
        let crc = crc32_ieee(&payload) as u32;
        let mut frame = Vec::with_capacity(12 + payload.len());
        frame.extend_from_slice(&0x5741_4c31u32.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&payload);

        let path = wal_path(&self.dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(&frame)?;
        file.sync_data()?;
        state.apply(event);

        if state.seq % SNAPSHOT_EVERY == 0 {
            self.write_snapshot(state)?;
        }
        Ok(())
    }

    fn write_snapshot(&self, state: &State) -> std::io::Result<()> {
        let target = snapshot_path(&self.dir);
        let tmp = tmp_path(&target);
        let bytes = serde_json::to_vec(state).expect("state serializes");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &target)?;
        let wal = wal_path(&self.dir);
        let tmp_wal = tmp_path(&wal);
        fs::write(&tmp_wal, b"")?;
        fs::rename(&tmp_wal, &wal)?;
        Ok(())
    }

    pub fn idempotent(
        &self,
        key: Option<&str>,
        action: impl FnOnce(&mut State) -> Result<(u16, Value, Vec<Event>), StoreError>,
    ) -> Result<(u16, Value), StoreError> {
        let mut state = self.state.lock().unwrap();
        if let Some(key) = key {
            if let Some(record) = state.idempotency.get(key) {
                return Ok((record.status, record.body.clone()));
            }
        }
        let (status, body, events) = action(&mut state)?;
        for event in events {
            self.append(&mut state, event).map_err(|e| {
                StoreError::bad(&format!("persistence failure: {e}"))
            })?;
        }
        if let Some(key) = key {
            let event = Event::IdempotencyRecorded {
                key: key.to_string(),
                status,
                body: body.clone(),
            };
            self.append(&mut state, event).map_err(|e| {
                StoreError::bad(&format!("persistence failure: {e}"))
            })?;
        }
        Ok((status, body))
    }
}

fn value_diff(server: &Value, client: &Value, prefix: &str, out: &mut Vec<Value>) {
    match (server, client) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<String> =
                a.keys().chain(b.keys()).cloned().collect();
            for key in keys {
                let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                match (a.get(&key), b.get(&key)) {
                    (Some(x), Some(y)) if x != y => value_diff(x, y, &path, out),
                    (Some(x), None) => out.push(serde_json::json!({
                        "path": path, "server": x, "client": null, "kind": "server_only"
                    })),
                    (None, Some(y)) => out.push(serde_json::json!({
                        "path": path, "server": null, "client": y, "kind": "client_only"
                    })),
                    _ => {}
                }
            }
        }
        (a, b) if a != b => out.push(serde_json::json!({
            "path": prefix, "server": a, "client": b, "kind": "value"
        })),
        _ => {}
    }
}

impl Store {
    pub fn upsert_format(
        &self,
        doc: FormatDoc,
        idempotency_key: Option<&str>,
    ) -> Result<(u16, Value), StoreError> {
        self.idempotent(idempotency_key, |state| {
            let existing_same_version = state.format(&doc.id, doc.version).cloned();
            if let Some(existing) = existing_same_version {
                let a = serde_json::to_value(&existing).unwrap();
                let b = serde_json::to_value(&doc).unwrap();
                if a != b {
                    let mut diffs = Vec::new();
                    value_diff(&a, &b, "", &mut diffs);
                    return Err(StoreError::conflict(
                        "format version is immutable; create a new version",
                        serde_json::json!({ "fields": diffs }),
                    ));
                }
                return Ok((200, serde_json::to_value(&existing).unwrap(), vec![]));
            }
            let body = serde_json::to_value(&doc).unwrap();
            Ok((201, body, vec![Event::FormatUpserted { doc }]))
        })
    }

    pub fn upsert_rule(
        &self,
        doc: RuleDoc,
        expected_rev: u64,
        idempotency_key: Option<&str>,
    ) -> Result<(u16, Value), StoreError> {
        self.idempotent(idempotency_key, |state| {
            let existing = state.latest_rule(&doc.id).cloned();
            let current_rev = state.rules.get(&doc.id).map(|s| s.rev).unwrap_or(0);
            if expected_rev != current_rev {
                let mut diffs = Vec::new();
                if let Some(server_doc) = &existing {
                    value_diff(
                        &serde_json::to_value(server_doc).unwrap(),
                        &serde_json::to_value(&doc).unwrap(),
                        "",
                        &mut diffs,
                    );
                }
                return Err(StoreError::conflict(
                    "stale rule revision; merge the server version and retry",
                    serde_json::json!({
                        "server_rev": current_rev,
                        "client_rev": expected_rev,
                        "server": existing,
                        "fields": diffs
                    }),
                ));
            }
            if let Some(server_doc) = &existing {
                if doc.version <= server_doc.version {
                    return Err(StoreError::bad(
                        "new rule version must be greater than the stored version",
                    ));
                }
            }
            let rev = current_rev + 1;
            let body = serde_json::json!({ "doc": doc, "rev": rev });
            Ok((201, body, vec![Event::RuleUpserted { doc, rev }]))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRule {
    pub doc: RuleDoc,
    pub rev: u64,
}

impl Store {
    pub fn upsert_sample(
        &self,
        mut doc: SampleDoc,
        idempotency_key: Option<&str>,
    ) -> Result<(u16, Value), StoreError> {
        self.idempotent(idempotency_key, |state| {
            if let Some(existing) = state.samples.get(&doc.id) {
                if existing.hex == doc.hex
                    && existing.format == doc.format
                    && existing.name == doc.name
                {
                    return Ok((200, serde_json::to_value(existing).unwrap(), vec![]));
                }
                doc.rev = existing.rev + 1;
            }
            let body = serde_json::to_value(&doc).unwrap();
            Ok((201, body, vec![Event::SampleUpserted { doc }]))
        })
    }

    pub fn save_plan(
        &self,
        mut plan: PlanDoc,
        expected_rev: u64,
        idempotency_key: Option<&str>,
    ) -> Result<(u16, Value), StoreError> {
        self.idempotent(idempotency_key, |state| {
            let current_rev = state.plans.get(&plan.id).map(|p| p.rev).unwrap_or(0);
            if expected_rev != current_rev {
                let server = state.plans.get(&plan.id).cloned();
                let mut diffs = Vec::new();
                if let Some(server_plan) = &server {
                    value_diff(
                        &serde_json::to_value(server_plan).unwrap(),
                        &serde_json::to_value(&plan).unwrap(),
                        "",
                        &mut diffs,
                    );
                }
                return Err(StoreError::conflict(
                    "stale plan revision",
                    serde_json::json!({
                        "server_rev": current_rev,
                        "client_rev": expected_rev,
                        "server": server,
                        "fields": diffs
                    }),
                ));
            }
            plan.rev = current_rev + 1;
            let body = serde_json::to_value(&plan).unwrap();
            Ok((200, body, vec![Event::PlanUpserted { doc: plan }]))
        })
    }

    pub fn add_batch(&self, plan_id: &str, batch: BatchDoc) -> Result<(u16, Value), StoreError> {
        let mut state = self.state.lock().unwrap();
        let plan = state
            .plans
            .get_mut(plan_id)
            .ok_or_else(|| StoreError::not_found("plan not found"))?;
        if plan.state != PlanState::Published {
            return Err(StoreError::illegal(
                "batches can only be created from a published, frozen plan",
            ));
        }
        plan.batches.push(batch.clone());
        let snapshot = plan.clone();
        let event = Event::PlanUpserted { doc: snapshot };
        self.append(&mut state, event)
            .map_err(|e| StoreError::bad(&format!("persistence failure: {e}")))?;
        Ok((201, serde_json::to_value(&batch).unwrap()))
    }

    pub fn add_batch_idempotent(
        &self,
        plan_id: &str,
        batch: BatchDoc,
        key: Option<&str>,
    ) -> Result<(u16, Value), StoreError> {
        self.idempotent(key, |state| {
            let plan = state
                .plans
                .get_mut(plan_id)
                .ok_or_else(|| StoreError::not_found("plan not found"))?;
            if plan.state != PlanState::Published {
                return Err(StoreError::illegal(
                    "batches can only be created from a published, frozen plan",
                ));
            }
            plan.batches.push(batch.clone());
            let snapshot = plan.clone();
            Ok((
                201,
                serde_json::to_value(&batch).unwrap(),
                vec![Event::PlanUpserted { doc: snapshot }],
            ))
        })
    }
}
