//! SQLite-backed persistence.
//!
//! Every definition, rule and sample is append-only: saves create new rows,
//! originals are never overwritten. Writes use optimistic concurrency via the
//! expected revision; a stale writer receives HTTP 409 with both sides' diff.
//! Batch conversion is all-or-nothing and crash-recovered on startup.

use crate::canonical::{canonical_json, fingerprint};
use crate::migrate::RuleSpec;
use crate::model::FormatSpec;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug)]
pub struct DefRow {
    pub id: String,
    pub name: String,
    pub revision: i64,
    pub spec_json: String,
    pub fingerprint: String,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct SampleRow {
    pub id: String,
    pub name: String,
    pub format_name: String,
    pub format_revision: i64,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct RuleRow {
    pub id: String,
    pub revision: i64,
    pub name: String,
    pub spec_json: String,
    pub fingerprint: String,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct PlanRow {
    pub id: String,
    pub revision: i64,
    pub name: String,
    pub status: String,
    pub rule_id: String,
    pub rule_revision: i64,
    pub fingerprints_json: String,
    pub acceptances_json: String,
    pub dryrun_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct BatchRow {
    pub id: String,
    pub plan_id: String,
    pub status: String,
    pub count: i64,
    pub created_at: String,
}

fn now() -> String {
    // Deterministic-friendly ISO timestamp from the wall clock (not part of exports).
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

pub fn new_id(prefix: &str) -> String {
    use sha2::Digest;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = sha2::Sha256::new();
    h.update(prefix.as_bytes());
    h.update(now().as_bytes());
    h.update(format!("{:?}", std::thread::current().id()).as_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let n: u128 = u128::from_le_bytes(h.finalize()[..16].try_into().unwrap());
    format!("{prefix}_{n:016x}")
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 10_000)?;
        let s = Store { conn: Mutex::new(conn) };
        s.init()?;
        s.recover()?;
        Ok(s)
    }

    /// Test-only access to the underlying connection.
    #[doc(hidden)]
    pub fn conn_lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    /// In-memory store for tests.
    pub fn open_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let s = Store { conn: Mutex::new(conn) };
        s.init()?;
        s.recover()?;
        Ok(s)
    }

    fn init(&self) -> rusqlite::Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS defs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                revision INTEGER NOT NULL,
                spec_json TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(name, revision)
            );
            CREATE TABLE IF NOT EXISTS samples (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                format_name TEXT NOT NULL,
                format_revision INTEGER NOT NULL,
                bytes BLOB NOT NULL,
                sha256 TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS rules (
                id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                name TEXT NOT NULL,
                spec_json TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY(id, revision),
                UNIQUE(name, revision)
            );
            CREATE TABLE IF NOT EXISTS plans (
                id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                rule_id TEXT NOT NULL,
                rule_revision INTEGER NOT NULL,
                fingerprints_json TEXT NOT NULL,
                acceptances_json TEXT NOT NULL,
                dryrun_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS batches (
                id TEXT PRIMARY KEY,
                plan_id TEXT NOT NULL,
                status TEXT NOT NULL,
                count INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS outputs (
                batch_id TEXT NOT NULL,
                sample_id TEXT NOT NULL,
                bytes BLOB NOT NULL,
                sha256 TEXT NOT NULL,
                ord INTEGER NOT NULL,
                PRIMARY KEY(batch_id, sample_id)
            );
            CREATE TABLE IF NOT EXISTS idempotency (
                idem_key TEXT PRIMARY KEY,
                route TEXT NOT NULL,
                status_code INTEGER NOT NULL,
                body BLOB NOT NULL,
                content_type TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                action TEXT NOT NULL,
                detail TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Crash recovery: any batch left 'running' by an abnormal exit is marked
    /// aborted together with its partial outputs (which remain only for
    /// forensics and are never listed as visible batches).
    fn recover(&self) -> rusqlite::Result<usize> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE batches SET status='aborted_crash' WHERE status='running'",
            [],
        )?;
        if n > 0 {
            c.execute(
                "INSERT INTO audit_log(ts, action, detail) VALUES(?1,'crash_recover',?2)",
                params![now(), format!("{n} running batch(es) marked aborted_crash")],
            )?;
        }
        Ok(n)
    }

    pub fn audit(&self, action: &str, detail: &str) {
        let c = self.conn.lock().unwrap();
        let _ = c.execute(
            "INSERT INTO audit_log(ts, action, detail) VALUES(?1,?2,?3)",
            params![now(), action, detail],
        );
    }

    // ----------------------------------------------------------- idempotency
    pub fn idem_get(&self, key: &str) -> Option<(u16, Vec<u8>, String)> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT status_code, body, content_type FROM idempotency WHERE idem_key=?1",
            params![key],
            |r| Ok((r.get::<_, i64>(0)? as u16, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn idem_put(&self, key: &str, route: &str, code: u16, body: &[u8], ct: &str) {
        let c = self.conn.lock().unwrap();
        let _ = c.execute(
            "INSERT OR IGNORE INTO idempotency(idem_key, route, status_code, body, content_type, created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, route, code as i64, body, ct, now()],
        );
    }
}

// ------------------------------------------------------------- definitions

#[derive(Debug)]
pub enum StoreError {
    Conflict(String),
    Sql(rusqlite::Error),
    Other(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(s) | StoreError::Other(s) => f.write_str(s),
            StoreError::Sql(e) => write!(f, "{e}"),
        }
    }
}

/// One differing leaf path between two JSON documents.
fn json_diff_paths(a: &Value, b: &Value, prefix: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            let mut keys: std::collections::BTreeSet<&String> = am.keys().collect();
            keys.extend(bm.keys());
            for k in keys {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match (am.get(k), bm.get(k)) {
                    (Some(x), Some(y)) => json_diff_paths(x, y, &p, out),
                    (Some(_), None) => out.push(format!("{p}: removed")),
                    (None, Some(_)) => out.push(format!("{p}: added")),
                    _ => {}
                }
            }
        }
        _ => {
            if a != b {
                out.push(format!("{prefix}: {a} -> {b}"));
            }
        }
    }
}

pub struct ConflictBody {
    pub code: String,
    pub message: String,
    pub expected_revision: i64,
    pub actual_revision: i64,
    pub incoming: Value,
    pub stored: Value,
    pub diffs: Vec<String>,
}

impl serde::Serialize for ConflictBody {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ConflictBody", 7)?;
        st.serialize_field("code", &self.code)?;
        st.serialize_field("message", &self.message)?;
        st.serialize_field("expected_revision", &self.expected_revision)?;
        st.serialize_field("actual_revision", &self.actual_revision)?;
        st.serialize_field("incoming", &self.incoming)?;
        st.serialize_field("stored", &self.stored)?;
        st.serialize_field("diffs", &self.diffs)?;
        st.end()
    }
}

impl Store {
    pub fn latest_def(&self, name: &str) -> rusqlite::Result<Option<DefRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,name,revision,spec_json,fingerprint,created_at FROM defs
             WHERE name=?1 ORDER BY revision DESC LIMIT 1",
            params![name],
            map_def,
        )
        .optional()
    }

    pub fn def_at(&self, name: &str, revision: i64) -> rusqlite::Result<Option<DefRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,name,revision,spec_json,fingerprint,created_at FROM defs
             WHERE name=?1 AND revision=?2",
            params![name, revision],
            map_def,
        )
        .optional()
    }

    pub fn list_defs(&self) -> rusqlite::Result<Vec<(String, i64, String)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT name, revision, fingerprint FROM defs ORDER BY name, revision")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Insert a new revision. `expected_revision` must equal the current latest;
    /// None means "this must be the first revision".
    pub fn insert_def(
        &self,
        raw_json: &str,
        expected_revision: Option<i64>,
    ) -> Result<DefRow, StoreError> {
        let v: Value = serde_json::from_str(raw_json).map_err(|e| StoreError::Other(format!("invalid JSON: {e}")))?;
        let spec: FormatSpec = serde_json::from_value(v.clone())
            .map_err(|e| StoreError::Other(format!("spec does not match schema: {e}")))?;
        if spec.name.trim().is_empty() {
            return Err(StoreError::Other("format name is empty".into()));
        }
        let canon = canonical_json(&v);
        let fp = fingerprint(&v);
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT MAX(revision) FROM defs WHERE name=?1",
                params![spec.name],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match (current, expected_revision) {
            (None, None) | (None, Some(0)) => {}
            (Some(cur), Some(exp)) if cur == exp => {}
            (Some(cur), _) => {
                let stored: String = tx
                    .query_row(
                        "SELECT spec_json FROM defs WHERE name=?1 AND revision=?2",
                        params![spec.name, cur],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "{}".into());
                let stored_v: Value = serde_json::from_str(&stored).unwrap_or(Value::Null);
                let mut diffs = Vec::new();
                json_diff_paths(&v, &stored_v, "", &mut diffs);
                return Err(StoreError::Conflict(
                    serde_json::to_string(&ConflictBody {
                        code: "revision_conflict".into(),
                        message: format!("stale write: {} is at revision {cur}", spec.name),
                        expected_revision: expected_revision.unwrap_or(-1),
                        actual_revision: cur,
                        incoming: v,
                        stored: stored_v,
                        diffs,
                    })
                    .unwrap(),
                ));
            }
            (None, Some(exp)) => {
                return Err(StoreError::Other(format!(
                    "expected base revision {exp} but no definition exists yet (use base revision 0)"
                )));
            }
        }
        let new_rev = current.map(|x| x + 1).unwrap_or(1);
        let id = new_id("def");
        tx.execute(
            "INSERT INTO defs(id,name,revision,spec_json,fingerprint,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, spec.name, new_rev, canon, fp, now()],
        )?;
        tx.execute(
            "INSERT INTO audit_log(ts, action, detail) VALUES(?1,'def_save',?2)",
            params![now(), format!("{} r{}", spec.name, new_rev)],
        )?;
        tx.commit()?;
        Ok(DefRow {
            id,
            name: spec.name,
            revision: new_rev,
            spec_json: canon,
            fingerprint: fp,
            created_at: now(),
        })
    }
}

fn map_def(r: &rusqlite::Row) -> rusqlite::Result<DefRow> {
    Ok(DefRow {
        id: r.get(0)?,
        name: r.get(1)?,
        revision: r.get(2)?,
        spec_json: r.get(3)?,
        fingerprint: r.get(4)?,
        created_at: r.get(5)?,
    })
}

// ------------------------------------------------------------- samples (immutable)

impl Store {
    pub fn insert_sample(
        &self,
        name: &str,
        format_name: &str,
        format_revision: i64,
        bytes: &[u8],
    ) -> Result<SampleRow, StoreError> {
        if self.def_at(format_name, format_revision)?.is_none() {
            return Err(StoreError::Other(format!(
                "format {format_name} r{format_revision} does not exist"
            )));
        }
        let sha = crate::canonical::fingerprint_bytes(bytes);
        let id = new_id("smp");
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO samples(id,name,format_name,format_revision,bytes,sha256,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, name, format_name, format_revision, bytes, sha, now()],
        )?;
        Ok(SampleRow {
            id,
            name: name.to_string(),
            format_name: format_name.into(),
            format_revision,
            bytes: bytes.to_vec(),
            sha256: sha,
            created_at: now(),
        })
    }

    pub fn get_sample(&self, id: &str) -> rusqlite::Result<Option<SampleRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,name,format_name,format_revision,bytes,sha256,created_at FROM samples WHERE id=?1",
            params![id],
            |r| {
                Ok(SampleRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    format_name: r.get(2)?,
                    format_revision: r.get(3)?,
                    bytes: r.get(4)?,
                    sha256: r.get(5)?,
                    created_at: r.get(6)?,
                })
            },
        )
        .optional()
    }

    pub fn list_samples(&self) -> rusqlite::Result<Vec<(String, String, String, i64, String)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id,name,format_name,format_revision,sha256 FROM samples ORDER BY created_at,id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        rows.collect()
    }
}

// ------------------------------------------------------------- rules

fn map_rule(r: &rusqlite::Row) -> rusqlite::Result<RuleRow> {
    Ok(RuleRow {
        id: r.get(0)?,
        revision: r.get(1)?,
        name: r.get(2)?,
        spec_json: r.get(3)?,
        fingerprint: r.get(4)?,
        created_at: r.get(5)?,
    })
}

impl Store {
    pub fn latest_rule(&self, name: &str) -> rusqlite::Result<Option<RuleRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,revision,name,spec_json,fingerprint,created_at FROM rules
             WHERE name=?1 ORDER BY revision DESC LIMIT 1",
            params![name],
            map_rule,
        )
        .optional()
    }

    pub fn rule_at(&self, name: &str, revision: i64) -> rusqlite::Result<Option<RuleRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,revision,name,spec_json,fingerprint,created_at FROM rules WHERE name=?1 AND revision=?2",
            params![name, revision],
            map_rule,
        )
        .optional()
    }

    pub fn rule_by_id(&self, id: &str, revision: i64) -> rusqlite::Result<Option<RuleRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,revision,name,spec_json,fingerprint,created_at FROM rules WHERE id=?1 AND revision=?2",
            params![id, revision],
            map_rule,
        )
        .optional()
    }

    pub fn list_rules(&self) -> rusqlite::Result<Vec<(String, i64, String)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT name, revision, fingerprint FROM rules ORDER BY name, revision")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    pub fn insert_rule(&self, raw_json: &str, expected_revision: Option<i64>) -> Result<RuleRow, StoreError> {
        let v: Value = serde_json::from_str(raw_json).map_err(|e| StoreError::Other(format!("invalid JSON: {e}")))?;
        let rule: RuleSpec =
            serde_json::from_value(v.clone()).map_err(|e| StoreError::Other(format!("rule schema: {e}")))?;
        let canon = canonical_json(&v);
        let fp = fingerprint(&v);
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let current: Option<i64> = tx
            .query_row("SELECT MAX(revision) FROM rules WHERE name=?1", params![rule.name], |r| r.get(0))
            .optional()?
            .flatten();
        match (current, expected_revision) {
            (None, None) | (None, Some(0)) => {}
            (Some(cur), Some(exp)) if cur == exp => {}
            (Some(cur), _) => {
                let stored: String = tx
                    .query_row(
                        "SELECT spec_json FROM rules WHERE name=?1 AND revision=?2",
                        params![rule.name, cur],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "{}".into());
                let stored_v: Value = serde_json::from_str(&stored).unwrap_or(Value::Null);
                let mut diffs = Vec::new();
                json_diff_paths(&v, &stored_v, "", &mut diffs);
                return Err(StoreError::Conflict(
                    serde_json::to_string(&ConflictBody {
                        code: "revision_conflict".into(),
                        message: format!("stale write: rule {} is at revision {cur}", rule.name),
                        expected_revision: expected_revision.unwrap_or(-1),
                        actual_revision: cur,
                        incoming: v,
                        stored: stored_v,
                        diffs,
                    })
                    .unwrap(),
                ));
            }
            (None, Some(exp)) => {
                return Err(StoreError::Other(format!(
                    "expected base rule revision {exp} but no rule exists yet (use 0)"
                )));
            }
        }
        let new_rev = current.map(|x| x + 1).unwrap_or(1);
        let id = new_id("rule");
        tx.execute(
            "INSERT INTO rules(id,revision,name,spec_json,fingerprint,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, new_rev, rule.name, canon, fp, now()],
        )?;
        tx.commit()?;
        Ok(RuleRow {
            id,
            revision: new_rev,
            name: rule.name,
            spec_json: canon,
            fingerprint: fp,
            created_at: now(),
        })
    }
}

// ------------------------------------------------------------- plans

fn map_plan(r: &rusqlite::Row) -> rusqlite::Result<PlanRow> {
    Ok(PlanRow {
        id: r.get(0)?,
        revision: r.get(1)?,
        name: r.get(2)?,
        status: r.get(3)?,
        rule_id: r.get(4)?,
        rule_revision: r.get(5)?,
        fingerprints_json: r.get(6)?,
        acceptances_json: r.get(7)?,
        dryrun_json: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

const PLAN_STATES: &[&str] = &["draft", "frozen", "published", "retired"];

impl Store {
    pub fn create_plan(
        &self,
        name: &str,
        rule_name: &str,
        rule_revision: i64,
        dryrun_json: &str,
        fingerprints_json: &str,
    ) -> Result<PlanRow, StoreError> {
        let rule = self
            .rule_at(rule_name, rule_revision)?
            .ok_or_else(|| StoreError::Other(format!("rule {rule_name} r{rule_revision} missing")))?;
        let id = new_id("plan");
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO plans(id,revision,name,status,rule_id,rule_revision,fingerprints_json,
                 acceptances_json,dryrun_json,created_at,updated_at)
             VALUES(?1,1,?2,'draft',?3,?4,?5,'[]',?6,?7,?7)",
            params![id, name, rule.id, rule.revision, fingerprints_json, dryrun_json, now()],
        )?;
        drop(c);
        self.get_plan(&id)?.ok_or_else(|| StoreError::Other("plan vanished".into()))
    }

    pub fn get_plan(&self, id: &str) -> rusqlite::Result<Option<PlanRow>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT id,revision,name,status,rule_id,rule_revision,fingerprints_json,acceptances_json,
                    dryrun_json,created_at,updated_at FROM plans WHERE id=?1",
            params![id],
            map_plan,
        )
        .optional()
    }

    pub fn list_plans(&self) -> rusqlite::Result<Vec<PlanRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id,revision,name,status,rule_id,rule_revision,fingerprints_json,acceptances_json,
                    dryrun_json,created_at,updated_at FROM plans ORDER BY created_at,id",
        )?;
        let rows = stmt.query_map([], map_plan)?;
        rows.collect()
    }

    /// Optimistic transition with legal-state-machine enforcement.
    pub fn transition_plan(
        &self,
        id: &str,
        expected_revision: i64,
        new_status: &str,
        acceptances_json: Option<&str>,
        dryrun_json: Option<&str>,
    ) -> Result<PlanRow, StoreError> {
        if !PLAN_STATES.contains(&new_status) {
            return Err(StoreError::Other(format!("unknown plan state {new_status}")));
        }
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let (cur_rev, cur_status, rule_id, rule_revision): (i64, String, String, i64) = tx
            .query_row(
                "SELECT revision, status, rule_id, rule_revision FROM plans WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::Other("plan not found".into()))?;
        if cur_rev != expected_revision {
            return Err(StoreError::Conflict(
                serde_json::json!({
                    "code":"revision_conflict",
                    "message":format!("plan revision {cur_rev} != expected {expected_revision}"),
                    "expected_revision": expected_revision,
                    "actual_revision": cur_rev,
                })
                .to_string(),
            ));
        }
        let legal = matches!(
            (cur_status.as_str(), new_status),
            ("draft", "frozen") | ("frozen", "published") | ("frozen", "draft") | ("published", "retired")
        );
        if !legal {
            return Err(StoreError::Other(format!(
                "illegal state transition {cur_status} -> {new_status}"
            )));
        }
        // Freezing requires every lossy item to be accepted & bound to current rule revision.
        if new_status == "frozen" {
            let dry: Value = if let Some(d) = dryrun_json {
                serde_json::from_str(d).unwrap_or(Value::Null)
            } else {
                let d: String = tx.query_row("SELECT dryrun_json FROM plans WHERE id=?1", params![id], |r| r.get(0))?;
                serde_json::from_str(&d).unwrap_or(Value::Null)
            };
            let acc: Value = acceptances_json
                .map(|s| serde_json::from_str(s).unwrap_or(Value::Null))
                .unwrap_or(Value::Array(Vec::new()));
            let lossy: Vec<String> = dry
                .get("samples")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .flat_map(|s| s.get("reverse_items").and_then(|i| i.as_array()).cloned().unwrap_or_default())
                        .filter(|i| i.get("tier").and_then(|t| t.as_str()) == Some("lossy"))
                        .map(|i| i.get("target").and_then(|t| t.as_str()).unwrap_or("").to_string())
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default()
                .into_iter()
                .collect();
            let accepted: std::collections::BTreeSet<String> = acc
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("path").and_then(|p| p.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let missing: Vec<&String> = lossy.iter().filter(|p| !accepted.contains(p.as_str())).collect();
            if !missing.is_empty() {
                return Err(StoreError::Other(format!(
                    "cannot freeze: lossy items lack acceptance binding: {missing:?}"
                )));
            }
            // Freeze definition fingerprints SERVER-SIDE from the bound rule
            // revision (never trusting client JSON).
            let rule_json: String = tx.query_row(
                "SELECT spec_json FROM rules WHERE id=?1 AND revision=?2",
                params![rule_id, rule_revision],
                |r| r.get(0),
            )?;
            let rule_v: Value = serde_json::from_str(&rule_json).unwrap_or(Value::Null);
            let from_name = rule_v.get("from_format").and_then(|v| v.as_str()).unwrap_or("");
            let from_rev = rule_v.get("from_revision").and_then(|v| v.as_i64()).unwrap_or(0);
            let to_name = rule_v.get("to_format").and_then(|v| v.as_str()).unwrap_or("");
            let to_rev = rule_v.get("to_revision").and_then(|v| v.as_i64()).unwrap_or(0);
            let fp_of = |n: &str, rv: i64| -> rusqlite::Result<String> {
                tx.query_row(
                    "SELECT fingerprint FROM defs WHERE name=?1 AND revision=?2",
                    params![n, rv],
                    |r| r.get(0),
                )
            };
            let frozen = serde_json::json!({
                "frozen": true,
                "rule_id": rule_id,
                "rule_revision": rule_revision,
                "rule_fingerprint": crate::canonical::fingerprint(&rule_v),
                "from_format": {"name": from_name, "revision": from_rev,
                                "fingerprint": fp_of(from_name, from_rev)?},
                "to_format": {"name": to_name, "revision": to_rev,
                              "fingerprint": fp_of(to_name, to_rev)?}
            });
            tx.execute(
                "UPDATE plans SET fingerprints_json=?2 WHERE id=?1",
                params![id, canonical_json(&frozen)],
            )?;
        }
        tx.execute(
            "UPDATE plans SET revision=revision+1, status=?2,
                 acceptances_json=COALESCE(?3, acceptances_json),
                 dryrun_json=COALESCE(?4, dryrun_json),
                 updated_at=?5 WHERE id=?1",
            params![id, new_status, acceptances_json, dryrun_json, now()],
        )?;
        tx.commit()?;
        drop(c);
        self.get_plan(id)?.ok_or_else(|| StoreError::Other("plan vanished".into()))
    }
}

// ------------------------------------------------------------- batches & export

pub struct ConvertedFile {
    pub sample_id: String,
    pub bytes: Vec<u8>,
}

impl Store {
    /// All-or-nothing batch. The caller converts *every* sample successfully
    /// before calling this; inside one transaction the running marker and all
    /// outputs are committed together so a crash never leaves a visible partial
    /// batch.
    pub fn commit_batch(&self, plan_id: &str, files: Vec<ConvertedFile>) -> Result<BatchRow, StoreError> {
        let plan = self.get_plan(plan_id)?.ok_or_else(|| StoreError::Other("plan missing".into()))?;
        if plan.status != "published" {
            return Err(StoreError::Other(format!(
                "plan must be published before batch conversion (currently {})",
                plan.status
            )));
        }
        let id = new_id("batch");
        let count = files.len() as i64;
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO batches(id,plan_id,status,count,created_at) VALUES(?1,?2,'completed',?3,?4)",
            params![id, plan_id, count, now()],
        )?;
        for (i, f) in files.iter().enumerate() {
            let sha = crate::canonical::fingerprint_bytes(&f.bytes);
            tx.execute(
                "INSERT INTO outputs(batch_id,sample_id,bytes,sha256,ord) VALUES(?1,?2,?3,?4,?5)",
                params![id, f.sample_id, f.bytes, sha, i as i64],
            )?;
        }
        tx.execute(
            "INSERT INTO audit_log(ts, action, detail) VALUES(?1,'batch_commit',?2)",
            params![now(), format!("{id} ({count} files)")],
        )?;
        tx.commit()?;
        Ok(BatchRow { id, plan_id: plan_id.into(), status: "completed".into(), count, created_at: now() })
    }

    pub fn list_batches(&self, include_aborted: bool) -> rusqlite::Result<Vec<BatchRow>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = if include_aborted {
            c.prepare(
                "SELECT id,plan_id,status,count,created_at FROM batches
                 WHERE status='completed' OR status='aborted_crash' ORDER BY created_at,id",
            )?
        } else {
            c.prepare(
                "SELECT id,plan_id,status,count,created_at FROM batches
                 WHERE status='completed' ORDER BY created_at,id",
            )?
        };
        let rows = stmt.query_map([], |r| {
            Ok(BatchRow {
                id: r.get(0)?,
                plan_id: r.get(1)?,
                status: r.get(2)?,
                count: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn batch_outputs(&self, batch_id: &str) -> rusqlite::Result<Vec<(String, Vec<u8>, String)>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT sample_id, bytes, sha256 FROM outputs WHERE batch_id=?1 ORDER BY ord,sample_id",
        )?;
        let rows = stmt.query_map(params![batch_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Deterministic export of every durable artifact as a USTAR archive.
    /// No timestamps or random ids enter the entry contents, so the same
    /// database content yields byte-identical archives.
    pub fn export_tar(&self) -> rusqlite::Result<Vec<u8>> {
        let c = self.conn.lock().unwrap();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();

        let mut defs = c.prepare(
            "SELECT name,revision,spec_json,fingerprint FROM defs ORDER BY name,revision",
        )?;
        for row in defs.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })? {
            let (name, rev, json, fp) = row?;
            files.push((format!("defs/{name}/r{rev}.json"), json.into_bytes()));
            files.push((format!("defs/{name}/r{rev}.fingerprint"), fp.into_bytes()));
        }

        let mut rules = c.prepare("SELECT name,revision,spec_json,fingerprint FROM rules ORDER BY name,revision")?;
        for row in rules.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })? {
            let (name, rev, json, fp) = row?;
            files.push((format!("rules/{name}/r{rev}.json"), json.into_bytes()));
            files.push((format!("rules/{name}/r{rev}.fingerprint"), fp.into_bytes()));
        }

        let mut smps = c.prepare(
            "SELECT s.id,s.name,s.format_name,s.format_revision,s.sha256 FROM samples s
             ORDER BY s.format_name,s.format_revision,s.id",
        )?;
        for row in smps.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })? {
            let (id, name, fmt, rev, sha) = row?;
            let meta = serde_json::json!({
                "id": id, "name": name, "format": fmt, "revision": rev, "sha256": sha
            });
            files.push((format!("samples/{id}.meta.json"), serde_json::to_vec_pretty(&meta).unwrap()));
        }
        // sample bytes are exported under content-addressed names to be deterministic
        let mut blobs = c.prepare("SELECT DISTINCT bytes, sha256 FROM samples ORDER BY sha256")?;
        for row in blobs.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)))? {
            let (bytes, sha) = row?;
            files.push((format!("samples/blobs/{sha}.bin"), bytes));
        }

        let mut plans = c.prepare(
            "SELECT id,name,status,rule_id,rule_revision,fingerprints_json,acceptances_json,dryrun_json,revision
             FROM plans ORDER BY id",
        )?;
        for row in plans.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "status": r.get::<_, String>(2)?,
                "rule_id": r.get::<_, String>(3)?,
                "rule_revision": r.get::<_, i64>(4)?,
                "revision": r.get::<_, i64>(8)?,
                "fingerprints": serde_json::from_str::<Value>(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                "acceptances": serde_json::from_str::<Value>(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
                "dryrun": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or(Value::Null),
            }))
        })? {
            let v = row?;
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("plan").to_string();
            files.push((format!("plans/{id}.json"), canonical_json(&v).into_bytes()));
        }

        let mut batches = c.prepare(
            "SELECT b.id,b.plan_id,b.status,b.count,o.sample_id,o.bytes,o.sha256,o.ord
             FROM batches b LEFT JOIN outputs o ON o.batch_id=b.id
             WHERE b.status='completed' ORDER BY b.id,o.ord",
        )?;
        for row in batches
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<Vec<u8>>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
        {
            let (bid, plan, status, count, sid, bytes, sha) = row?;
            if let (Some(sid), Some(bytes), Some(sha)) = (sid, bytes, sha) {
                files.push((format!("batches/{bid}/{sid}.bin"), bytes.clone()));
                files.push((
                    format!("batches/{bid}/{sid}.sha256"),
                    format!("{sha}  {sid}.bin\n").into_bytes(),
                ));
            }
            let meta = serde_json::json!({"id":bid,"plan":plan,"status":status,"count":count});
            files.push((format!("batches/{bid}/meta.json"), canonical_json(&meta).into_bytes()));
        }

        // Manifest last; its hash authenticates the deterministic export.
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut manifest = Vec::new();
        for (path, data) in &files {
            let sha = crate::canonical::fingerprint_bytes(data);
            manifest.extend_from_slice(format!("{sha}  {path}\n").as_bytes());
        }
        files.push(("MANIFEST.sha256".into(), manifest));
        Ok(crate::tar::write_ustar_owned(files))
    }
}
