//! Execution joins inside the existing opt-in SQL-shape witness.
//! Exact parameter keys are private, bounded, and disappear with the collector.
use super::{enabled, lock};
use crate::operational_metrics::sql::{OwnerIdentity, Window};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub const KEY_BYTES_LIMIT: usize = 16 * 1024 * 1024;
pub const KEY_LIMIT: usize = 65_536;
pub const WORK_ROW_LIMIT: usize = 32_768;
pub const VALUE_LIMIT: usize = 65_536;
pub const ACTIVE_LIMIT: usize = 1024;
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Payload dimensions are backend types: SQLite expanded literal/value bytes
/// and PostgreSQL encoded parameter/column bytes cannot be pooled as one unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Sqlite,
    Postgres,
}
impl Backend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}
impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cut {
    pub turn: Option<usize>,
    pub phase: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub statements: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
    pub bound_bytes: u64,
    pub result_bytes: u64,
    pub unknown_bindings: u64,
    pub unqualified_read_identities: u64,
}
impl Counts {
    fn add(&mut self, other: &Self) {
        self.statements += other.statements;
        self.rows_returned += other.rows_returned;
        self.rows_affected += other.rows_affected;
        self.bound_bytes += other.bound_bytes;
        self.result_bytes += other.result_bytes;
        self.unknown_bindings += other.unknown_bindings;
        self.unqualified_read_identities += other.unqualified_read_identities;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    /// SHA-256 of exact SQL text, before shape normalization.
    pub statement: String,
    pub shape: String,
    pub site: String,
    /// Syntactic participation, never a division of physical work by table.
    pub relations: Option<Vec<String>>,
    pub backend: Backend,
    pub started_in: Cut,
    pub finished_in: Option<Cut>,
    pub owner: Option<OwnerIdentity>,
    pub operation: Option<String>,
    pub worker: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    #[serde(flatten)]
    pub key: Key,
    #[serde(flatten)]
    pub counts: Counts,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReadKey {
    pub window: Cut,
    pub site: String,
    pub statement: String,
    /// Collision-free, collector-local equality class, not a hash claim.
    pub parameters: usize,
    pub bind_class: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatedRead {
    #[serde(flatten)]
    pub key: ReadKey,
    pub executions: u64,
    pub extra_executions: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub epoch: u64,
    pub totals: BTreeMap<Backend, Counts>,
    pub rows: Vec<Row>,
    pub active: Vec<Row>,
    pub repeated_reads: Vec<RepeatedRead>,
    pub lost: u64,
    pub admitted_key_bytes: usize,
    pub pending_bindings: usize,
    pub read_bind_classes: BTreeMap<String, u64>,
    pub repeat_qualification: String,
    pub repeat_identities_complete: bool,
    /// Counters conserve across completed and active executions. Unknown
    /// binds, overflow, and missing completions refuse a complete claim.
    pub complete: bool,
}
impl Receipt {
    pub fn statements(&self) -> u64 {
        self.totals.values().map(|c| c.statements).sum()
    }
    pub fn rows_returned(&self) -> u64 {
        self.totals.values().map(|c| c.rows_returned).sum()
    }
    pub fn unknown_bindings(&self) -> u64 {
        self.totals.values().map(|c| c.unknown_bindings).sum()
    }
    pub fn conserves(&self) -> bool {
        let mut sum = BTreeMap::<Backend, Counts>::new();
        for row in self.rows.iter().chain(&self.active) {
            sum.entry(row.key.backend).or_default().add(&row.counts);
        }
        self.lost == 0 && sum == self.totals
    }
}

pub(super) struct State {
    epoch: u64,
    next: u64,
    cut: Cut,
    totals: BTreeMap<Backend, Counts>,
    rows: BTreeMap<Key, Counts>,
    active: BTreeMap<u64, Row>,
    // Full equality, including SQL and type/null/length-framed parameters.
    // No digest alone can fabricate a repeated read.
    parameters: BTreeMap<Vec<u8>, usize>,
    binding_captures: BTreeMap<u64, Option<Vec<u8>>>,
    reads: BTreeMap<ReadKey, u64>,
    key_bytes: usize,
    read_bind_classes: BTreeMap<String, u64>,
    lost: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed) + 1,
            next: 0,
            cut: Cut {
                turn: None,
                phase: "unowned".into(),
            },
            totals: BTreeMap::new(),
            rows: BTreeMap::new(),
            active: BTreeMap::new(),
            parameters: BTreeMap::new(),
            binding_captures: BTreeMap::new(),
            reads: BTreeMap::new(),
            key_bytes: 0,
            lost: 0,
            read_bind_classes: BTreeMap::new(),
        }
    }
}
impl State {
    pub(super) fn receipt(&self) -> Receipt {
        Receipt {
            epoch: self.epoch, totals: self.totals.clone(),
            rows: self.rows.iter().map(|(key, counts)| Row { key: key.clone(), counts: counts.clone() }).collect(),
            active: self.active.values().cloned().collect(),
            repeated_reads: self.reads.iter().filter(|(_, n)| **n > 1).map(|(key, n)| RepeatedRead {
                key: key.clone(), executions: *n, extra_executions: n - 1,
            }).collect(),
            lost: self.lost, admitted_key_bytes: self.key_bytes, pending_bindings: self.binding_captures.len(),
            read_bind_classes: self.read_bind_classes.clone(),
            repeat_identities_complete: self.lost == 0 && self.totals.values().all(|c| c.unqualified_read_identities == 0),
            repeat_qualification: "exact SQL plus parameters and origin; SQLite TEXT exact modulo embedded NUL; SQLite REAL excluded; unsupported encodings excluded".into(),
            complete: self.lost == 0 && self.active.is_empty() && self.totals.values().all(|c| c.unknown_bindings == 0),
        }
    }
    fn retain(&mut self, bytes: usize) -> bool {
        if self.key_bytes.saturating_add(bytes) > KEY_BYTES_LIMIT {
            false
        } else {
            self.key_bytes += bytes;
            true
        }
    }
}

/// Assign process work to a replay window. Background SQL is included in the
/// window population, but absent owner identities remain explicitly unowned.
pub fn set_window(turn: Option<usize>, phase: &str) {
    if !enabled() {
        return;
    }
    if let Some(state) = lock().as_mut() {
        state.work.cut = Cut {
            turn,
            phase: phase.chars().take(128).collect(),
        };
    }
}

/// A token cannot attach a late completion to a successor collector.
#[derive(Clone, Copy, Debug)]
pub struct Execution {
    epoch: u64,
    id: u64,
    read: bool,
}
impl Execution {
    pub fn start(
        backend: Backend,
        sql: &str,
        site: &str,
        relations: Option<Vec<String>>,
        read: bool,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let window = Window::current().map(|w| w.identity());
        let mut state = lock();
        let state = &mut state.as_mut()?.work;
        if state.active.len() == ACTIVE_LIMIT || sql.len() > VALUE_LIMIT || site.len() > 4096 {
            state.lost += 1;
            return None;
        }
        let key = Key {
            statement: format!("{:x}", Sha256::digest(sql.as_bytes())),
            shape: crate::operational_metrics::sql::normalized_shape(sql).to_string(),
            site: site.into(),
            relations,
            backend,
            started_in: state.cut.clone(),
            finished_in: None,
            owner: window.as_ref().and_then(|v| v.0.clone()),
            operation: window.map(|v| v.1),
            worker: format!("{:?}", std::thread::current().id()),
        };
        // Include all retained strings (not only parameter values) in the bound.
        let size = serde_json::to_vec(&key).ok()?.len();
        if !state.retain(size) {
            state.lost += 1;
            return None;
        }
        state.next += 1;
        let id = state.next;
        state.active.insert(
            id,
            Row {
                key,
                counts: Counts {
                    statements: 1,
                    ..Counts::default()
                },
            },
        );
        state.totals.entry(backend).or_default().statements += 1;
        Some(Self {
            epoch: state.epoch,
            id,
            read,
        })
    }
    fn update(self, f: impl FnOnce(&mut State, u64)) {
        if !enabled() {
            return;
        }
        if let Some(state) = lock().as_mut()
            && state.work.epoch == self.epoch
        {
            f(&mut state.work, self.id);
        }
    }
    pub fn row(self, bytes: u64) {
        self.update(|state, id| {
            if let Some(row) = state.active.get_mut(&id) {
                row.counts.rows_returned += 1;
                row.counts.result_bytes += bytes;
                state
                    .totals
                    .entry(row.key.backend)
                    .or_default()
                    .rows_returned += 1;
                state
                    .totals
                    .entry(row.key.backend)
                    .or_default()
                    .result_bytes += bytes;
            }
        });
    }
    /// `identity` includes exact SQL plus backend-typed parameter framing.
    /// None denotes unavailable or oversized data; it never means empty binds.
    pub fn bindings(self, bytes: Option<u64>, identity: Option<Vec<u8>>, bind_class: &str) {
        self.update(|state, id| {
            let Some(row) = state.active.get_mut(&id) else {
                return;
            };
            row.counts.bound_bytes += bytes.unwrap_or_default();
            state.totals.entry(row.key.backend).or_default().bound_bytes +=
                bytes.unwrap_or_default();
            if bytes.is_none() {
                row.counts.unknown_bindings += 1;
                state
                    .totals
                    .entry(row.key.backend)
                    .or_default()
                    .unknown_bindings += 1;
            }
            if !self.read {
                return;
            }
            let bind_class = if bind_class.len() > 128
                || (!state.read_bind_classes.contains_key(bind_class)
                    && state.read_bind_classes.len() >= 32)
            {
                "other_unqualified"
            } else {
                bind_class
            };
            *state
                .read_bind_classes
                .entry(bind_class.into())
                .or_default() += 1;
            if identity.is_none() {
                row.counts.unqualified_read_identities += 1;
                state
                    .totals
                    .entry(row.key.backend)
                    .or_default()
                    .unqualified_read_identities += 1;
            }
            let key = ReadKey {
                window: row.key.started_in.clone(),
                site: row.key.site.clone(),
                statement: row.key.statement.clone(),
                parameters: 0,
                bind_class: bind_class.into(),
            };
            let Some(identity) = identity else {
                return;
            };
            let parameter = if let Some(value) = state.parameters.get(&identity) {
                *value
            } else {
                if state.parameters.len() == KEY_LIMIT
                    || identity.len() > VALUE_LIMIT
                    || !state.retain(identity.len())
                {
                    state.lost += 1;
                    return;
                }
                let value = state.parameters.len();
                state.parameters.insert(identity, value);
                value
            };
            let key = ReadKey {
                parameters: parameter,
                ..key
            };
            if !state.reads.contains_key(&key)
                && (state.reads.len() == KEY_LIMIT
                    || !state.retain(serde_json::to_vec(&key).map_or(VALUE_LIMIT, |v| v.len())))
            {
                state.lost += 1;
                return;
            }
            *state.reads.entry(key).or_default() += 1;
        });
    }
    pub fn affected(self, rows: u64) {
        self.update(|state, id| {
            if let Some(row) = state.active.get_mut(&id) {
                row.counts.rows_affected += rows;
                state
                    .totals
                    .entry(row.key.backend)
                    .or_default()
                    .rows_affected += rows;
            }
        });
    }
    /// A dropped PG stream remains active: it did not observe command completion.
    pub fn finish(self) {
        self.update(|state, id| {
            if let Some(mut row) = state.active.remove(&id) {
                row.key.finished_in = Some(state.cut.clone());
                if state.rows.contains_key(&row.key) || state.rows.len() < WORK_ROW_LIMIT {
                    state.rows.entry(row.key).or_default().add(&row.counts);
                } else {
                    state.lost += 1;
                }
            }
        });
    }
}

/// A query owns only an epoch/token. Pending encoded values live in the same
/// bounded state as execution joins, and collector drop destroys them even
/// when a caller retains a query. Unexecuted queries consume a bounded slot.
#[derive(Clone, Copy)]
pub struct BindingCapture {
    epoch: u64,
    id: u64,
}
impl BindingCapture {
    pub fn new() -> Option<Self> {
        if !enabled() {
            return None;
        }
        let mut guard = lock();
        let state = &mut guard.as_mut()?.work;
        if state.binding_captures.len() >= ACTIVE_LIMIT {
            return None;
        }
        state.next += 1;
        state.binding_captures.insert(state.next, Some(Vec::new()));
        Some(Self {
            epoch: state.epoch,
            id: state.next,
        })
    }
    fn token(self) -> Execution {
        Execution {
            epoch: self.epoch,
            id: self.id,
            read: false,
        }
    }
    pub fn append(self, ty: &str, bytes: &[u8], null: bool, trusted_encoding: bool) {
        self.token().update(|state, id| {
            let Some(Some(existing)) = state.binding_captures.get(&id) else {
                return;
            };
            let length = existing.len();
            let added = ty.len().saturating_add(bytes.len()).saturating_add(17);
            if !trusted_encoding
                || length.saturating_add(added) > VALUE_LIMIT
                || state.key_bytes.saturating_add(added) > KEY_BYTES_LIMIT
            {
                state.binding_captures.insert(id, None);
                state.key_bytes = state.key_bytes.saturating_sub(length);
                return;
            }
            state.key_bytes += added;
            let Some(Some(key)) = state.binding_captures.get_mut(&id) else {
                return;
            };
            key.extend_from_slice(&(ty.len() as u64).to_le_bytes());
            key.extend_from_slice(ty.as_bytes());
            key.push(u8::from(null));
            key.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            key.extend_from_slice(bytes);
        });
    }
    pub fn take(self) -> Option<Vec<u8>> {
        let mut result = None;
        self.token().update(|state, id| {
            result = state.binding_captures.remove(&id).flatten();
            state.key_bytes = state
                .key_bytes
                .saturating_sub(result.as_ref().map_or(0, Vec::len));
        });
        result
    }
}

pub fn origin() -> &'static str {
    super::ORIGIN.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// SQL-COST-JOIN: a cut cannot hide an active statement or assign its
    /// tail to a new owner/window, and a successor cannot accept stale tokens.
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the exclusive process-global witness from parallel store laws"
    )]
    #[test]
    fn window_cuts_preserve_active_cross_window_and_refused_work() {
        const CHILD: &str = "LASH_SQL_WORK_CUT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "perf_witness::sql_shapes::work::tests::window_cuts_preserve_active_cross_window_and_refused_work", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "isolated law failed: {stdout}");
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "law did not execute: {stdout}"
            );
            return;
        }

        let collector = crate::perf_witness::Collector::install_with_sql_shapes().unwrap();
        set_window(None, "session_start");
        let execution =
            Execution::start(Backend::Sqlite, "SELECT 1", "law", Some(vec![]), true).unwrap();
        execution.bindings(Some(0), Some(b"SELECT 1".to_vec()), "integer_null_or_none");
        execution.row(8);
        let cut = collector.snapshot().sql_shapes.unwrap().work;
        assert!(cut.conserves());
        assert!(!cut.complete);
        assert_eq!(cut.active.len(), 1);
        assert!(cut.active[0].key.owner.is_none());
        set_window(Some(1), "turn");
        execution.finish();
        let done = collector.snapshot().sql_shapes.unwrap().work;
        assert!(done.complete && done.conserves());
        assert_eq!(done.rows[0].key.started_in.phase, "session_start");
        assert_eq!(done.rows[0].key.finished_in.as_ref().unwrap().phase, "turn");
        let capture = BindingCapture::new().unwrap();
        capture.append("TEXT", b"private", false, true);
        assert_eq!(
            collector
                .snapshot()
                .sql_shapes
                .unwrap()
                .work
                .pending_bindings,
            1
        );
        drop(collector);
        assert!(
            capture.take().is_none(),
            "query token retained values after collector drop"
        );
        let collector = crate::perf_witness::Collector::install_with_sql_shapes().unwrap();
        execution.row(100);
        execution.finish();
        assert_eq!(
            collector.snapshot().sql_shapes.unwrap().work.statements(),
            0
        );
        {
            lock().as_mut().unwrap().work.key_bytes = KEY_BYTES_LIMIT;
        }
        assert!(Execution::start(Backend::Sqlite, "SELECT 1", "law", Some(vec![]), true).is_none());
        let refused = collector.snapshot().sql_shapes.unwrap().work;
        assert!(!refused.complete && !refused.conserves());
        assert!(refused.lost > 0);
    }
}
