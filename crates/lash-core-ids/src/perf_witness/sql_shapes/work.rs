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

/// How observation ended, not whether the SQL command succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// The executor returned/exhausted, or SQLite emitted its profile callback.
    Finished,
    /// The PostgreSQL executor yielded an error.
    Error,
    /// The executor guard was dropped before either terminal observation.
    Dropped,
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
    fn subtract(&mut self, other: &Self) {
        self.statements -= other.statements;
        self.rows_returned -= other.rows_returned;
        self.rows_affected -= other.rows_affected;
        self.bound_bytes -= other.bound_bytes;
        self.result_bytes -= other.result_bytes;
        self.unknown_bindings -= other.unknown_bindings;
        self.unqualified_read_identities -= other.unqualified_read_identities;
    }
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
    /// Observer termination cut; this alone does not prove command completion.
    pub finished_in: Option<Cut>,
    pub disposition: Option<Disposition>,
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
    /// Cumulative descriptor construction, including already drained windows.
    pub worker_label_formats: u64,
    pub worker_descriptor_bytes: usize,
    pub relation_analyses: u64,
    pub statement_descriptor_bytes: usize,
    pub totals: BTreeMap<Backend, Counts>,
    pub rows: Vec<Row>,
    pub active: Vec<Row>,
    pub repeated_reads: Vec<RepeatedRead>,
    pub lost: u64,
    /// Peak resident key admissions, excluding already exported populations.
    pub admitted_key_bytes: usize,
    pub pending_bindings: usize,
    pub read_bind_classes: BTreeMap<String, u64>,
    pub repeat_qualification: String,
    pub repeat_identities_complete: bool,
    /// Counters conserve across completed and active executions. Unknown
    /// binds, overflow, and missing terminal observations refuse a complete claim.
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

// Metadata is shared by executions and equality groups, but exact SQL remains
// the cache key: a digest is never used to establish parameter equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Statement {
    digest: String,
    shape: String,
    relations: Option<Vec<String>>,
}
impl Statement {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + 128
            + self.digest.len()
            + self.shape.len()
            + self.relations.as_ref().map_or(0, |r| {
                r.iter()
                    .map(|s| std::mem::size_of::<String>() + s.len())
                    .sum::<usize>()
            })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct JoinKey {
    statement: std::sync::Arc<Statement>,
    site: std::sync::Arc<str>,
    backend: Backend,
    started_in: Cut,
    finished_in: Option<Cut>,
    disposition: Option<Disposition>,
    owner: Option<OwnerIdentity>,
    operation: Option<String>,
    worker: std::sync::Arc<str>,
}
impl JoinKey {
    fn exported(&self) -> Key {
        Key {
            statement: self.statement.digest.clone(),
            shape: self.statement.shape.clone(),
            site: self.site.to_string(),
            relations: self.statement.relations.clone(),
            backend: self.backend,
            started_in: self.started_in.clone(),
            finished_in: self.finished_in.clone(),
            disposition: self.disposition,
            owner: self.owner.clone(),
            operation: self.operation.clone(),
            worker: self.worker.to_string(),
        }
    }
    fn bytes(&self) -> usize {
        // Include container slots as well as all execution-owned strings.
        std::mem::size_of::<(Self, Counts)>()
            + 128
            + self.started_in.phase.len()
            + self.finished_in.as_ref().map_or(0, |c| c.phase.len())
            + self.owner.as_ref().map_or(0, |o| o.actor.len())
            + self.operation.as_ref().map_or(0, String::len)
            + self.worker.len()
    }
}
struct Active {
    key: JoinKey,
    counts: Counts,
}
impl Active {
    fn exported(&self) -> Row {
        Row {
            key: self.key.exported(),
            counts: self.counts.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ReadIdentity {
    window: Cut,
    site: std::sync::Arc<str>,
    statement: std::sync::Arc<Statement>,
    parameters: usize,
    bind_class: String,
}
impl ReadIdentity {
    fn exported(&self, executions: u64) -> RepeatedRead {
        RepeatedRead {
            key: ReadKey {
                window: self.window.clone(),
                site: self.site.to_string(),
                statement: self.statement.digest.clone(),
                parameters: self.parameters,
                bind_class: self.bind_class.clone(),
            },
            executions,
            extra_executions: executions - 1,
        }
    }
    fn bytes(&self) -> usize {
        std::mem::size_of::<(Self, u64)>() + 128 + self.window.phase.len() + self.bind_class.len()
    }
}

type DescriptorSlot = std::sync::Arc<std::sync::OnceLock<Option<std::sync::Arc<Statement>>>>;

pub(super) struct State {
    epoch: u64,
    next: u64,
    cut: Cut,
    totals: BTreeMap<Backend, Counts>,
    rows: BTreeMap<JoinKey, Counts>,
    active: BTreeMap<u64, Active>,
    sites: std::collections::BTreeSet<std::sync::Arc<str>>,
    next_parameter: usize,
    peak_key_bytes: usize,
    // Full equality, including SQL and type/null/length-framed parameters.
    // No digest alone can fabricate a repeated read.
    parameters: BTreeMap<Vec<u8>, usize>,
    binding_captures: BTreeMap<u64, Option<Vec<u8>>>,
    reads: BTreeMap<ReadIdentity, u64>,
    key_bytes: usize,
    read_bind_classes: BTreeMap<String, u64>,
    lost: u64,
    worker_label_formats: u64,
    worker_descriptor_bytes: usize,
    relation_analyses: u64,
    statement_descriptor_bytes: usize,
    observed_statements: BTreeMap<Backend, BTreeMap<String, DescriptorSlot>>,
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
            sites: std::collections::BTreeSet::new(),
            next_parameter: 0,
            peak_key_bytes: 0,
            parameters: BTreeMap::new(),
            binding_captures: BTreeMap::new(),
            reads: BTreeMap::new(),
            key_bytes: 0,
            lost: 0,
            read_bind_classes: BTreeMap::new(),
            worker_label_formats: 0,
            worker_descriptor_bytes: 0,
            relation_analyses: 0,
            statement_descriptor_bytes: 0,
            observed_statements: BTreeMap::new(),
        }
    }
}
impl State {
    pub(super) fn receipt(&self) -> Receipt {
        Receipt {
            worker_label_formats: self.worker_label_formats, worker_descriptor_bytes: self.worker_descriptor_bytes,
            relation_analyses: self.relation_analyses, statement_descriptor_bytes: self.statement_descriptor_bytes,
            epoch: self.epoch, totals: self.totals.clone(),
            rows: self.rows.iter().map(|(key, counts)| Row { key: key.exported(), counts: counts.clone() }).collect(),
            active: self.active.values().map(Active::exported).collect(),
            repeated_reads: self.reads.iter().filter(|(_, n)| **n > 1).map(|(key, n)| key.exported(*n)).collect(),
            lost: self.lost, admitted_key_bytes: self.peak_key_bytes, pending_bindings: self.binding_captures.len(),
            read_bind_classes: self.read_bind_classes.clone(),
            repeat_identities_complete: self.lost == 0 && self.totals.values().all(|c| c.unqualified_read_identities == 0),
            repeat_qualification: "exact SQL plus parameters and origin; SQLite TEXT exact modulo embedded NUL; SQLite REAL excluded; unsupported encodings excluded".into(),
            complete: self.lost == 0 && self.active.is_empty() && self.totals.values().all(|c| c.unknown_bindings == 0),
        }
    }
    fn descriptor_slot(&mut self, backend: Backend, sql: &str) -> Option<DescriptorSlot> {
        if let Some(found) = self
            .observed_statements
            .get(&backend)
            .and_then(|statements| statements.get(sql))
        {
            return Some(std::sync::Arc::clone(found));
        }
        let bytes = sql.len() + 128 + std::mem::size_of::<DescriptorSlot>();
        if self
            .observed_statements
            .values()
            .map(BTreeMap::len)
            .sum::<usize>()
            == KEY_LIMIT
            || self
                .key_bytes
                .saturating_add(self.statement_descriptor_bytes)
                .saturating_add(bytes)
                > KEY_BYTES_LIMIT
        {
            self.lost += 1;
            return None;
        }
        self.statement_descriptor_bytes += bytes;
        self.peak_key_bytes = self
            .peak_key_bytes
            .max(self.key_bytes + self.statement_descriptor_bytes);
        let slot = std::sync::Arc::new(std::sync::OnceLock::new());
        self.observed_statements
            .entry(backend)
            .or_default()
            .insert(sql.to_owned(), std::sync::Arc::clone(&slot));
        Some(slot)
    }

    fn site(&mut self, site: &str) -> Option<std::sync::Arc<str>> {
        if let Some(found) = self.sites.get(site) {
            return Some(std::sync::Arc::clone(found));
        }
        if self.sites.len() >= KEY_LIMIT || !self.retain(site.len() + 128) {
            self.lost += 1;
            return None;
        }
        let site: std::sync::Arc<str> = site.into();
        self.sites.insert(std::sync::Arc::clone(&site));
        Some(site)
    }
    fn resident_bytes(&self) -> usize {
        self.sites
            .iter()
            .map(|site| site.len() + 128)
            .sum::<usize>()
            + self.rows.keys().map(JoinKey::bytes).sum::<usize>()
            + self
                .active
                .values()
                .map(|row| row.key.bytes())
                .sum::<usize>()
            + self.reads.keys().map(ReadIdentity::bytes).sum::<usize>()
            + self
                .parameters
                .keys()
                .map(|key| key.len() + 128)
                .sum::<usize>()
            + self
                .binding_captures
                .values()
                .map(|key| 128 + key.as_ref().map_or(0, Vec::len))
                .sum::<usize>()
    }
    fn drain_completed(&mut self) -> Receipt {
        let mut drained = Receipt {
            epoch: self.epoch,
            worker_label_formats: self.worker_label_formats,
            worker_descriptor_bytes: self.worker_descriptor_bytes,
            relation_analyses: self.relation_analyses,
            statement_descriptor_bytes: self.statement_descriptor_bytes,
            complete: true,
            repeat_identities_complete: true,
            admitted_key_bytes: self.peak_key_bytes,
            ..Receipt::default()
        };
        for (key, counts) in std::mem::take(&mut self.rows) {
            drained.totals.entry(key.backend).or_default().add(&counts);
            self.totals
                .entry(key.backend)
                .or_default()
                .subtract(&counts);
            drained.rows.push(Row {
                key: key.exported(),
                counts,
            });
        }
        self.totals.retain(|_, counts| *counts != Counts::default());
        // An execution can capture its bindings or finish after the window
        // changes. Retain its entire read population until that tail ends.
        let mut retained = BTreeMap::new();
        for (key, executions) in std::mem::take(&mut self.reads) {
            if key.window == self.cut
                || self
                    .active
                    .values()
                    .any(|row| row.key.started_in == key.window)
            {
                retained.insert(key, executions);
            } else if executions > 1 {
                drained.repeated_reads.push(key.exported(executions));
            }
        }
        self.reads = retained;
        let parameters: std::collections::BTreeSet<_> =
            self.reads.keys().map(|key| key.parameters).collect();
        self.parameters.retain(|_, id| parameters.contains(id));
        self.sites
            .retain(|site| std::sync::Arc::strong_count(site) > 1);
        self.key_bytes = self.resident_bytes();
        drained.complete = drained.unknown_bindings() == 0;
        drained.repeat_identities_complete = drained
            .totals
            .values()
            .all(|c| c.unqualified_read_identities == 0);
        drained
    }
    fn retain(&mut self, bytes: usize) -> bool {
        if self
            .key_bytes
            .saturating_add(self.statement_descriptor_bytes)
            .saturating_add(bytes)
            > KEY_BYTES_LIMIT
        {
            false
        } else {
            self.key_bytes += bytes;
            self.peak_key_bytes = self
                .peak_key_bytes
                .max(self.key_bytes + self.statement_descriptor_bytes);
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

/// Export terminal observations and closed read windows, releasing their
/// resident admissions. Active executions, their read windows and pending
/// bindings survive. Callers must assemble these chunks with the final
/// snapshot before comparing against cumulative independent witnesses.
pub fn drain_completed() -> Option<Receipt> {
    if !enabled() {
        return None;
    }
    Some(lock().as_mut()?.work.drain_completed())
}

// One immutable label per observed thread/epoch. A thread retains one slot,
// replacing its previous epoch; no mutable execution facts live here.
thread_local! {
    static WORKER: std::cell::RefCell<Option<(u64, std::sync::Arc<str>)>> = const { std::cell::RefCell::new(None) };
}
fn worker(state: &mut State) -> Option<std::sync::Arc<str>> {
    WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((epoch, label)) = &*slot
            && *epoch == state.epoch
        {
            return Some(std::sync::Arc::clone(label));
        }
        if state.worker_label_formats >= KEY_LIMIT as u64 {
            state.lost += 1;
            return None;
        }
        let label: std::sync::Arc<str> = format!("{:?}", std::thread::current().id()).into();
        state.worker_label_formats += 1;
        state.worker_descriptor_bytes +=
            label.len() + std::mem::size_of::<(u64, std::sync::Arc<str>)>();
        *slot = Some((state.epoch, std::sync::Arc::clone(&label)));
        Some(label)
    })
}

/// A token cannot attach a late completion to a successor collector.
#[derive(Clone, Copy, Debug)]
pub struct Execution {
    epoch: u64,
    id: u64,
    read: bool,
}
impl Execution {
    /// Observed executors supply syntactic relation analysis lazily. Exact SQL
    /// and backend select immutable metadata within this collector epoch.
    pub fn start(
        backend: Backend,
        sql: &str,
        site: &str,
        relations: impl FnOnce() -> Option<Vec<String>>,
        read: bool,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let (epoch, slot) = {
            let mut guard = lock();
            let state = &mut guard.as_mut()?.work;
            if sql.len() > VALUE_LIMIT || site.len() > 4096 {
                state.lost += 1;
                return None;
            }
            (state.epoch, state.descriptor_slot(backend, sql)?)
        };
        let statement = slot
            .get_or_init(|| {
                {
                    let mut guard = lock();
                    let state = &mut guard.as_mut()?.work;
                    if state.epoch != epoch {
                        return None;
                    }
                    state.relation_analyses += 1;
                }
                // One initializer per admitted SQL/backend descriptor. Parsing
                // and hashing run outside the collector lock, including races.
                let statement = std::sync::Arc::new(Statement {
                    digest: format!("{:x}", Sha256::digest(sql.as_bytes())),
                    shape: crate::operational_metrics::sql::normalized_shape(sql).to_string(),
                    relations: relations(),
                });
                let mut guard = lock();
                let state = &mut guard.as_mut()?.work;
                if state.epoch != epoch {
                    return None;
                }
                let bytes = statement.bytes();
                if state
                    .key_bytes
                    .saturating_add(state.statement_descriptor_bytes)
                    .saturating_add(bytes)
                    > KEY_BYTES_LIMIT
                {
                    return None;
                }
                state.statement_descriptor_bytes += bytes;
                state.peak_key_bytes = state
                    .peak_key_bytes
                    .max(state.key_bytes + state.statement_descriptor_bytes);
                Some(statement)
            })
            .clone();
        let Some(statement) = statement else {
            let mut guard = lock();
            if let Some(state) = guard.as_mut()
                && state.work.epoch == epoch
            {
                state.work.lost += 1;
            }
            return None;
        };
        Self::start_inner(backend, sql, site, read, epoch, statement)
    }

    fn start_inner(
        backend: Backend,
        sql: &str,
        site: &str,
        read: bool,
        epoch: u64,
        statement: std::sync::Arc<Statement>,
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
        if epoch != state.epoch {
            return None;
        }
        let site = state.site(site)?;
        let key = JoinKey {
            statement,
            site,
            backend,
            started_in: state.cut.clone(),
            finished_in: None,
            disposition: None,
            owner: window.as_ref().and_then(|v| v.0.clone()),
            operation: window.map(|v| v.1),
            worker: worker(state)?,
        };
        if !state.retain(key.bytes()) {
            state.lost += 1;
            return None;
        }
        state.next += 1;
        let id = state.next;
        state.active.insert(
            id,
            Active {
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
            let key = ReadIdentity {
                window: row.key.started_in.clone(),
                site: std::sync::Arc::clone(&row.key.site),
                statement: std::sync::Arc::clone(&row.key.statement),
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
                    || !state.retain(identity.len() + 128)
                {
                    state.lost += 1;
                    return;
                }
                let value = state.next_parameter;
                state.next_parameter += 1;
                state.parameters.insert(identity, value);
                value
            };
            let key = ReadIdentity {
                parameters: parameter,
                ..key
            };
            if !state.reads.contains_key(&key)
                && (state.reads.len() == KEY_LIMIT || !state.retain(key.bytes()))
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
    /// Observe executor exhaustion/return or a SQLite profile callback.
    pub fn finish(self) {
        self.end(Disposition::Finished);
    }
    /// Observe an error yielded by the executor, without inventing completion.
    pub fn failed(self) {
        self.end(Disposition::Error);
    }
    /// Close an abandoned observation, retaining only work already observed.
    pub fn dropped(self) {
        self.end(Disposition::Dropped);
    }
    fn end(self, disposition: Disposition) {
        self.update(|state, id| {
            if let Some(mut row) = state.active.remove(&id) {
                state.key_bytes -= row.key.bytes();
                row.key.finished_in = Some(state.cut.clone());
                row.key.disposition = Some(disposition);
                if let Some(counts) = state.rows.get_mut(&row.key) {
                    counts.add(&row.counts);
                } else if state.rows.len() < WORK_ROW_LIMIT && state.retain(row.key.bytes()) {
                    state.rows.insert(row.key, row.counts);
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
        if state.binding_captures.len() >= ACTIVE_LIMIT || !state.retain(128) {
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
            state.peak_key_bytes = state.peak_key_bytes.max(state.key_bytes);
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
            if let Some(captured) = state.binding_captures.remove(&id) {
                state.key_bytes -= 128 + captured.as_ref().map_or(0, Vec::len);
                result = captured;
            }
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
    /// SQL-WORK-RESIDENT: repeated executions admit resident keys once,
    /// rather than exhausting the budget with invocation-lifetime traffic.
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the process-global witness"
    )]
    #[test]
    fn closed_windows_release_dynamic_keys_and_retain_epoch_descriptors() {
        const CHILD: &str = "LASH_SQL_RESIDENT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "perf_witness::sql_shapes::work::tests::closed_windows_release_dynamic_keys_and_retain_epoch_descriptors", "--nocapture"])
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
        for _ in 0..100_000 {
            if let Some(execution) = Execution::start(
                Backend::Sqlite,
                "SELECT 1",
                "resident-law",
                || Some(vec![]),
                false,
            ) {
                execution.finish();
            }
        }
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(
            work.complete && work.conserves(),
            "resident admissions refused repeated work: lost={}",
            work.lost
        );
        assert_eq!(work.statements(), 100_000);
        assert_eq!(work.rows.len(), 1);
        assert!(work.admitted_key_bytes < KEY_BYTES_LIMIT);
        set_window(Some(0), "turn");
        assert_eq!(drain_completed().unwrap().statements(), 100_000);
        let mut previous_parameter = None;
        // More than 16 MiB of distinct identity traffic across closed turns
        // must fit when no single resident population exceeds the bound.
        for turn in 1..=4 {
            set_window(Some(turn), "turn");
            for parameter in 0_usize..2500 {
                let mut identity = vec![0; 2048];
                identity[..8].copy_from_slice(&parameter.to_le_bytes());
                for _ in 0..2 {
                    let execution = Execution::start(
                        Backend::Sqlite,
                        "SELECT ?",
                        "resident-law",
                        || Some(vec![]),
                        true,
                    )
                    .unwrap();
                    execution.bindings(Some(8), Some(identity.clone()), "integer");
                    execution.finish();
                }
            }
            set_window(Some(turn + 1), "maintenance");
            let drained = drain_completed().unwrap();
            assert!(drained.complete && drained.conserves());
            assert_eq!(drained.statements(), 5000);
            assert_eq!(drained.repeated_reads.len(), 2500);
            let first_parameter = drained
                .repeated_reads
                .iter()
                .map(|r| r.key.parameters)
                .min()
                .unwrap();
            if let Some(previous) = previous_parameter {
                assert!(
                    first_parameter > previous,
                    "released equality IDs were reused"
                );
            }
            previous_parameter = drained
                .repeated_reads
                .iter()
                .map(|r| r.key.parameters)
                .max();
            let guard = lock();
            let state = &guard.as_ref().unwrap().work;
            assert_eq!(state.key_bytes, 0);
            assert_eq!(state.relation_analyses, 2);
            assert!(state.statement_descriptor_bytes > 0);
            assert!(state.statement_descriptor_bytes < KEY_BYTES_LIMIT);
            assert!(state.parameters.is_empty() && state.sites.is_empty());
        }
        assert!(collector.snapshot().sql_shapes.unwrap().work.complete);
    }

    /// Descriptor identity belongs to the collector epoch, including late tokens.
    #[expect(
        clippy::disallowed_methods,
        reason = "exclusive global witness needs an isolated process"
    )]
    #[test]
    fn immutable_descriptors_belong_to_one_backend_and_collector_epoch() {
        const CHILD: &str = "LASH_WORKER_DESCRIPTOR_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "perf_witness::sql_shapes::work::tests::immutable_descriptors_belong_to_one_backend_and_collector_epoch"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let mut previous_epoch = 0;
        let mut stale = None;
        for _ in 0..2 {
            let collector = crate::perf_witness::Collector::install_with_sql_shapes().unwrap();
            if let Some(stale) = stale {
                let stale: Execution = stale;
                stale.finish();
            }
            for _ in 0..10 {
                let e = Execution::start(Backend::Sqlite, "SELECT 1", "law", || Some(vec![]), true)
                    .unwrap();
                e.bindings(Some(0), Some(vec![]), "none");
                e.finish();
                stale = Some(e);
            }
            let barrier = std::sync::Barrier::new(4);
            std::thread::scope(|scope| {
                for _ in 0..3 {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let e =
                            Execution::start(Backend::Postgres, "SELECT 1", "law", || None, true)
                                .unwrap();
                        e.bindings(Some(0), Some(vec![]), "none");
                        e.finish();
                    });
                }
                barrier.wait();
            });
            let work = collector.snapshot().sql_shapes.unwrap().work;
            assert!(work.epoch > previous_epoch);
            previous_epoch = work.epoch;
            assert_eq!(work.worker_label_formats, 4);
            assert_eq!(work.relation_analyses, 2);
            assert_eq!(work.statements(), 13);
            assert!(
                work.rows
                    .iter()
                    .filter(|row| row.key.backend == Backend::Postgres)
                    .all(|row| row.key.relations.is_none())
            );
            assert!(work.complete && work.conserves());
        }
    }

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
            Execution::start(Backend::Sqlite, "SELECT 1", "law", || Some(vec![]), true).unwrap();
        execution.bindings(Some(0), Some(b"SELECT 1".to_vec()), "integer_null_or_none");
        execution.row(8);
        let cut = collector.snapshot().sql_shapes.unwrap().work;
        assert!(cut.conserves());
        assert!(!cut.complete);
        assert_eq!(cut.active.len(), 1);
        assert!(cut.active[0].key.owner.is_none());
        let tail =
            Execution::start(Backend::Sqlite, "SELECT 1", "law", || Some(vec![]), true).unwrap();
        set_window(Some(1), "turn");
        let drained = drain_completed().unwrap();
        assert!(drained.conserves());
        assert_eq!(drained.statements(), 0);
        tail.bindings(Some(0), Some(b"SELECT 1".to_vec()), "integer_null_or_none");
        execution.finish();
        tail.finish();
        let done = collector.snapshot().sql_shapes.unwrap().work;
        assert!(done.complete && done.conserves());
        assert_eq!(done.rows[0].key.started_in.phase, "session_start");
        assert_eq!(done.rows[0].key.finished_in.as_ref().unwrap().phase, "turn");
        let capture = BindingCapture::new().unwrap();
        capture.append("TEXT", b"private", false, true);
        set_window(Some(2), "turn");
        let drained = drain_completed().unwrap();
        assert!(drained.complete && drained.conserves());
        assert_eq!(drained.statements(), 2);
        assert_eq!(drained.repeated_reads.len(), 1);
        assert_eq!(drained.repeated_reads[0].executions, 2);
        assert_eq!(
            collector.snapshot().sql_shapes.unwrap().work.statements(),
            0
        );
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
        assert!(
            Execution::start(Backend::Sqlite, "SELECT 1", "law", || Some(vec![]), true).is_none()
        );
        let refused = collector.snapshot().sql_shapes.unwrap().work;
        assert!(!refused.complete && !refused.conserves());
        assert!(refused.lost > 0);
    }
}
