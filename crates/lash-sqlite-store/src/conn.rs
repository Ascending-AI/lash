//! The shared async connection wrapper over [`tokio_rusqlite::Connection`].
//!
//! Every module in this crate talks to SQLite through [`SqliteConnection`]:
//! a single cheaply-clonable handle whose database operations all run on the
//! connection's own background thread via [`tokio_rusqlite::Connection::call`].
//!
//! ## Why a wrapper and not raw `tokio_rusqlite::Connection`
//!
//! Three concerns are centralised here so the porter modules stay terse and
//! consistent:
//!
//! * **WAL + busy-timeout setup.** Real SQLite WAL (the entire point of the
//!   rusqlite swap) needs `PRAGMA journal_mode=WAL` plus a generous
//!   `busy_timeout` so contending processes wait instead of failing.
//!   [`SqliteConnection::open`] applies these once on the connection thread,
//!   for a file and for a named `memdb` database alike.
//!
//! * **`IMMEDIATE` write transactions behind a per-database gate.**
//!   rusqlite's `Connection::transaction` opens `BEGIN DEFERRED`, which only
//!   takes the write lock on the first write statement. Every read-then-write
//!   path the crate promises to serialise cross-process (head-revision CAS,
//!   lease fencing, a root's admission) must therefore use
//!   [`SqliteConnection::write`], which opens `BEGIN IMMEDIATE` so the write
//!   lock is acquired up front and a contending writer waits on the busy
//!   timeout instead of reading a stale snapshot. In-process writers never
//!   reach that wait: every connection opened on one database shares a
//!   process-wide gate (FIG-3975), taken on the connection thread before
//!   `BEGIN IMMEDIATE` and released when the transaction ends, so in-process
//!   contention wakes on the gate's release rather than sleeping in SQLite's
//!   busy handler. The gate is never held across an `.await`, so a suspended
//!   caller cannot keep it. `busy_timeout` still stands for writers in other
//!   processes.
//!
//! * **The writer fence (ADR 0115 §2.2).** The first statement of every
//!   write transaction after `BEGIN IMMEDIATE` reads the database's
//!   `lash_compat` row: it re-admits the component stamp and answers the
//!   epoch `F` the transaction runs under, which the closure reads from
//!   [`FencedTx::fleet`]. `F` outside the writable range is the terminal
//!   `WriterFenced`, and the transaction writes nothing. The installer
//!   ([`SqliteConnection::install`]) is the one write that runs before the
//!   row exists, and it arms the fence. [`SqliteConnection::call`] and
//!   [`SqliteConnection::read`] never mutate; the guarded-transaction lint
//!   (`scripts/check-guarded-transactions.py`) keeps every other mutation
//!   inside [`SqliteConnection::write`] or [`SqliteConnection::write_flow`].
//!
//! * **Error mapping.** `conn.call(...)` returns [`tokio_rusqlite::Error`],
//!   which wraps [`rusqlite::Error`]. The helpers flatten that so closures only
//!   ever deal in `rusqlite::Result<T>` and callers receive the inner
//!   `rusqlite::Error` to feed through `sqlite_error` / `process_sqlite_error`.

use lash_core_execution::FleetFormat;
use lash_core_execution::compat::VersionRange;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(feature = "perf-witness")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;
#[cfg(feature = "perf-witness")]
use std::time::Instant;
use tokio_rusqlite::Connection as AsyncConnection;

use crate::SqliteDatabase;
use crate::location::DatabaseTarget;

// Fault points are syntax declarations in the transaction code. With the
// `testing` feature disabled, the invocation and all of its arguments expand
// to nothing, so production transactions carry no injector branch or state.
#[cfg(feature = "testing")]
macro_rules! sim_fault {
    ($injector:expr, $point:ident, $write_transaction_ordinal:expr) => {
        if let Some(injector) = $injector.as_ref() {
            injector.inject(
                crate::testing::SqliteFaultPoint::$point,
                $write_transaction_ordinal,
            )?;
        }
    };
}

#[cfg(not(feature = "testing"))]
macro_rules! sim_fault {
    ($($ignored:tt)*) => {};
}

/// Outcome a write flow returns to decide commit vs rollback while still
/// handing a value back to the caller. Used for paths that compute a result
/// *and* may discover mid-transaction that the work must not be persisted
/// (e.g. a contended root admission, where partially bound rows must be
/// rolled back and the caller told nothing was admitted).
pub(crate) enum TxOutcome<T> {
    Commit(T),
    Rollback(T),
}

/// Busy timeout applied to every connection. Matches the prior store's
/// 15-second window so cross-process writers wait rather than fail fast.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 15_000;

/// Prepared statements each connection keeps cached (FIG-3975). rusqlite's
/// default cache of 16 evicts constantly under this store's statement mix,
/// so nearly every `execute`/`query_row` re-parsed its SQL; the catalog's
/// distinct statements fit comfortably inside 256.
const PREPARED_STATEMENT_CACHE_CAPACITY: usize = 256;

/// The process-wide write gates, one per database identity
/// ([`DatabaseTarget::canonical_name`], so differently-spelled paths and
/// `memdb` names of the same database share one gate). Entries are weak so
/// a closed backend's gate is forgotten; the connections keep it alive.
static WRITE_GATES: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static READ_GATES: LazyLock<Mutex<HashMap<String, Weak<RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One checkpoint worker per file database, shared by all of its connections.
/// The worker holds only a weak reference while asleep, so closing the last
/// store connection also ends the worker.
static CHECKPOINTS: LazyLock<Mutex<HashMap<String, Weak<CheckpointState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct CheckpointState {
    path: PathBuf,
    write_gate: Arc<Mutex<()>>,
    read_gate: Arc<RwLock<()>>,
    threshold_pages: AtomicU64,
    commits: AtomicU64,
}

fn checkpoint_state(
    target: &DatabaseTarget,
    write_gate: &Arc<Mutex<()>>,
    read_gate: &Arc<RwLock<()>>,
    threshold_pages: u32,
) -> Option<Arc<CheckpointState>> {
    let path = target.file_path()?;
    if threshold_pages == 0 {
        return None;
    }
    let mut states = CHECKPOINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = target.canonical_name();
    if let Some(state) = states.get(&key).and_then(Weak::upgrade) {
        state
            .threshold_pages
            .fetch_min(u64::from(threshold_pages), Ordering::Relaxed);
        return Some(state);
    }
    let state = Arc::new(CheckpointState {
        path: path.to_owned(),
        write_gate: Arc::clone(write_gate),
        read_gate: Arc::clone(read_gate),
        threshold_pages: AtomicU64::new(u64::from(threshold_pages)),
        commits: AtomicU64::new(0),
    });
    let weak = Arc::downgrade(&state);
    match std::thread::Builder::new()
        .name("lash-sqlite-checkpoint".to_string())
        .spawn(move || checkpoint_loop(weak))
    {
        Ok(_) => {
            states.insert(key, Arc::downgrade(&state));
            Some(state)
        }
        Err(error) => {
            tracing::error!(%error, "could not start SQLite checkpoint worker");
            None
        }
    }
}

fn checkpoint_loop(state: Weak<CheckpointState>) {
    let mut seen_commits = 0;
    let mut retry = false;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(state) = state.upgrade() else {
            return;
        };
        let commits = state.commits.load(Ordering::Acquire);
        if commits == seen_commits && !retry {
            continue;
        }
        seen_commits = commits;
        retry = checkpoint_if_needed(&state).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %state.path.display(), "SQLite checkpoint retry");
            true
        });
    }
}

/// Return whether a blocked checkpoint should be retried even without a new
/// commit. TRUNCATE is attempted only while the in-process write gate is idle.
fn checkpoint_if_needed(state: &CheckpointState) -> rusqlite::Result<bool> {
    // Stop new in-process readers and let their current statements finish.
    // Writers also take this shared lock before the existing write gate, so
    // obtaining it exclusively makes the write gate idle without a lock cycle.
    let _read_gate = state
        .read_gate
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(_write_gate) = state.write_gate.try_lock() else {
        return Ok(true);
    };
    let connection = Connection::open_with_flags(
        &state.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(20))?;
    let pages: i64 =
        connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))?;
    if pages < state.threshold_pages.load(Ordering::Relaxed) as i64 {
        return Ok(false);
    }
    let busy: i64 =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    Ok(busy != 0)
}

#[cfg(feature = "perf-witness")]
static GATE_TIMING_ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "perf-witness")]
/// Enable opt-in write-gate timing for the current process.
pub fn enable_gate_timings() {
    GATE_TIMING_ENABLED.store(true, Ordering::Relaxed);
}
#[cfg(feature = "perf-witness")]
static GATE_TIMINGS: LazyLock<Mutex<Vec<(u64, u64)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(feature = "perf-witness")]
fn record_gate_timing(wait: Duration, hold: Duration) {
    if GATE_TIMING_ENABLED.load(Ordering::Relaxed) {
        GATE_TIMINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((wait.as_micros() as u64, hold.as_micros() as u64));
    }
}

#[cfg(feature = "perf-witness")]
/// Drain opt-in write-gate wait and hold measurements, in microseconds.
pub fn take_gate_timings() -> Vec<(u64, u64)> {
    std::mem::take(
        &mut GATE_TIMINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// `target`'s shared write gate, created on first open.
fn write_gate(target: &DatabaseTarget) -> Arc<Mutex<()>> {
    let key = target.canonical_name();
    let mut gates = WRITE_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

fn read_gate(target: &DatabaseTarget) -> Arc<RwLock<()>> {
    let key = target.canonical_name();
    let mut gates = READ_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(RwLock::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

/// SQLite synchronous setting selected for a connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SqliteSynchronous {
    /// Do not wait for filesystem synchronization. This minimizes write
    /// latency but gives up SQLite's protection against power-loss corruption;
    /// use only when the deployment accepts that durability trade-off.
    Off,
    /// Synchronize at the normal durability/performance balance. This is the
    /// default and is appropriate for the store's usual local deployment.
    #[default]
    Normal,
    /// Synchronize every committed transaction for the strongest power-loss
    /// durability at the cost of higher write latency; use on deployments
    /// where that durability guarantee outweighs throughput.
    Full,
}

impl SqliteSynchronous {
    pub(crate) fn as_pragma_value(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
        }
    }
}

/// Deployment policy for the SQLite connection owned by a store handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqliteConnectionPolicy {
    /// Read-only connection threads serving catalog and history reads.
    pub read_connections: std::num::NonZeroUsize,
    /// How long a contending SQLite operation waits before returning busy.
    /// Longer waits reduce transient contention failures but can hold up a
    /// caller; the default is 15 seconds, and change it when the deployment's
    /// write-lock duration or request deadline is materially different.
    pub busy_timeout: Duration,
    /// Filesystem synchronization strength. `Normal` is the default; choose
    /// `Full` when power-loss durability matters more than write latency, or
    /// `Off` only when the deployment explicitly accepts weaker durability.
    pub synchronous: SqliteSynchronous,
    /// WAL pages written before SQLite attempts an automatic checkpoint. The
    /// default is SQLite's 1,000-page setting; `0` disables automatic
    /// checkpointing, including this store's background truncation worker.
    /// Lower it to bound WAL growth, or raise it when checkpoint overhead
    /// matters more than reader lag. `0` disables both mechanisms.
    pub wal_autocheckpoint_pages: u32,
    /// SQLite cache-size pragma value, preserving SQLite's sign overload. The
    /// default is SQLite's `-2000` value (approximately 2,000 KiB); negative
    /// values count KiB and positive values count pages, so change it to match
    /// the deployment's memory budget and page size.
    pub cache_size: i32,
}

impl Default for SqliteConnectionPolicy {
    fn default() -> Self {
        Self {
            read_connections: std::num::NonZeroUsize::new(4).unwrap_or(std::num::NonZeroUsize::MIN),
            busy_timeout: Duration::from_millis(BUSY_TIMEOUT_MS as u64),
            synchronous: SqliteSynchronous::Normal,
            wal_autocheckpoint_pages: 1_000,
            cache_size: -2_000,
        }
    }
}

/// SQLite's `SQLITE_TRACE_PROFILE` callback fires once per completed statement, so this is the
/// crate's single chokepoint for statement-shape counters: no porter module carries
/// instrumentation of its own.
/// Only the statement is recorded, not the duration the callback also carries — that clock is
/// quantised to whole milliseconds.
/// The callback and its registration compile out entirely unless `perf-witness` is enabled.
#[cfg_attr(not(feature = "perf-witness"), expect(unused_variables))]
fn install_perf_statement_witness(connection: &Connection) {
    #[cfg(feature = "perf-witness")]
    connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(|event: rusqlite::trace::TraceEvent<'_>| {
            if let rusqlite::trace::TraceEvent::Profile(statement, _) = event {
                lash_core_execution::perf_witness::record_sql_statement(&statement.sql());
            }
        }),
    );
}

/// Switch a file-backed connection into WAL mode, retrying on lock contention.
///
/// SQLite acquires an exclusive lock to convert the rollback journal to WAL and,
/// unlike ordinary writes, does **not** call the registered busy handler while
/// doing so. When many connections open a brand-new database at once they race
/// on that conversion and all but one get `SQLITE_BUSY`/"database is locked".
/// We therefore retry the conversion ourselves with a short backoff until the
/// busy-timeout budget is exhausted.
fn set_wal_journal_mode(c: &Connection, busy_timeout: Duration) -> rusqlite::Result<()> {
    let deadline = std::time::Instant::now() + busy_timeout;
    let mut backoff = std::time::Duration::from_millis(1);
    loop {
        match c.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(err) if is_busy(&err) && std::time::Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

/// True for the transient `SQLITE_BUSY` / `SQLITE_LOCKED` failures that mean
/// "another connection holds the lock right now", which are safe to retry.
fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::DatabaseBusy
                || e.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// A connection's writer fence (ADR 0115 §2.2), shared by its clones.
///
/// The installer arms it with the database the connection writes and the
/// build's writable range for `F`. Until then no write passes it.
#[derive(Debug)]
struct WriterFence {
    armed: OnceLock<ArmedFence>,
    /// The epoch the most recent fence read: what the handle's
    /// `fleet_format()` answers and what payloads encoded before `BEGIN`
    /// are encoded under (ADR 0115 §2.3).
    observed: Mutex<FleetFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArmedFence {
    database: SqliteDatabase,
    writable: VersionRange,
}

impl WriterFence {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: OnceLock::new(),
            observed: Mutex::new(FleetFormat::current()),
        })
    }

    fn observed(&self) -> FleetFormat {
        *self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record the epoch a fence read. A moved `F` replaces the observed
    /// value; an unmoved one keeps it, pin table included.
    fn observe(&self, fleet: FleetFormat) -> FleetFormat {
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if observed.version() != fleet.version() {
            *observed = fleet;
        }
        *observed
    }

    /// Run the fence inside `tx`, the transaction's first statement.
    fn check(&self, tx: &Transaction<'_>) -> rusqlite::Result<FleetFormat> {
        let Some(armed) = self.armed.get() else {
            return Err(crate::sqlite_conversion_error(
                lash_core_execution::StoreError::StorageFailure {
                    backend: crate::SQLITE_BACKEND,
                    message: "write on a SQLite connection whose database no installer admitted"
                        .to_owned(),
                },
            ));
        };
        let fleet = crate::compat::fence(tx, armed.database, armed.writable)?;
        Ok(self.observe(fleet))
    }
}

/// A write transaction past its fence: the transaction, and the epoch `F`
/// the fence read. Everything the transaction encodes for durable storage is
/// encoded under [`fleet`](Self::fleet), so nothing it writes straddles a
/// finalize.
pub(crate) struct FencedTx<'c> {
    tx: Transaction<'c>,
    fleet: FleetFormat,
}

impl FencedTx<'_> {
    /// The epoch this transaction runs under.
    pub(crate) fn fleet(&self) -> FleetFormat {
        self.fleet
    }
}

impl<'c> std::ops::Deref for FencedTx<'c> {
    type Target = Transaction<'c>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

/// Cheaply-clonable async handle to one SQLite database. Cloning shares the
/// same underlying connection thread (tokio-rusqlite reference-counts it), so
/// the `SqliteStore` can keep a single `SqliteConnection` and hand `&self` borrows of
/// it to every module.
#[derive(Clone)]
pub(crate) struct SqliteConnection {
    inner: AsyncConnection,
    /// The gate every in-process writer to this database queues on (FIG-3975);
    /// shared by all connections opened on the same `canonical_name`.
    write_gate: Arc<Mutex<()>>,
    read_gate: Arc<RwLock<()>>,
    checkpoint: Option<Arc<CheckpointState>>,
    fence: Arc<WriterFence>,
    #[cfg(feature = "testing")]
    fault_injector: Option<crate::testing::SqliteFaultInjector>,
}

impl SqliteConnection {
    /// This connection's fault controller, when one was installed at open.
    #[cfg(feature = "testing")]
    pub(crate) fn fault_injector(&self) -> Option<crate::testing::SqliteFaultInjector> {
        self.fault_injector.clone()
    }

    #[cfg(test)]
    pub(crate) async fn close_for_testing(&self) {
        self.inner
            .clone()
            .close()
            .await
            .expect("close SQLite test connection");
    }

    /// Open (or create) `target`, applying WAL + busy-timeout PRAGMAs on the
    /// connection thread.
    pub(crate) async fn open(target: &DatabaseTarget) -> tokio_rusqlite::Result<Self> {
        Self::open_with_policy(target, SqliteConnectionPolicy::default()).await
    }

    pub(crate) async fn open_with_policy(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(
            target,
            policy,
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn open_with_fault_injector(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
        fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(target, policy, fault_injector).await
    }

    /// One open path for every target: a `memdb` database answers the WAL
    /// switch with its own `memory` journal mode, so the file and memory
    /// forms differ only in the name opened.
    async fn open_configured(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        let gate = write_gate(target);
        let reads = read_gate(target);
        let inner = AsyncConnection::open(target.open_name()).await?;
        let pragmas = crate::connection_sql::open_pragmas(policy);
        inner
            .call(move |c| {
                // Install the busy handler through the rusqlite API *before* the
                // WAL conversion so ordinary write contention waits on it.
                c.busy_timeout(policy.busy_timeout)?;
                c.set_prepared_statement_cache_capacity(PREPARED_STATEMENT_CACHE_CAPACITY);
                // The WAL switch is not covered by the busy handler, so it gets
                // its own bounded retry loop (see `set_wal_journal_mode`).
                set_wal_journal_mode(c, policy.busy_timeout)?;
                c.execute_batch(&pragmas)?;
                install_perf_statement_witness(c);
                Ok(())
            })
            .await?;
        Ok(Self {
            inner,
            checkpoint: checkpoint_state(target, &gate, &reads, policy.wal_autocheckpoint_pages),
            write_gate: gate,
            read_gate: reads,
            fence: WriterFence::new(),
            #[cfg(feature = "testing")]
            fault_injector,
        })
    }

    /// Used by the export/resume call sites that must never mutate the source database.
    pub(crate) async fn open_readonly(target: &DatabaseTarget) -> tokio_rusqlite::Result<Self> {
        let inner = AsyncConnection::open_with_flags(
            target.read_only_uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .await?;
        inner
            .call(move |c| {
                c.busy_timeout(crate::connection_sql::READ_ONLY_BUSY_TIMEOUT)?;
                c.set_prepared_statement_cache_capacity(PREPARED_STATEMENT_CACHE_CAPACITY);
                c.execute_batch(crate::connection_sql::READ_ONLY_PRAGMAS)?;
                install_perf_statement_witness(c);
                Ok(())
            })
            .await?;
        Ok(Self {
            inner,
            write_gate: write_gate(target),
            read_gate: read_gate(target),
            checkpoint: None,
            fence: WriterFence::new(),
            #[cfg(feature = "testing")]
            fault_injector: None,
        })
    }

    /// The closure returns `rusqlite::Result<T>`; this method flattens tokio-rusqlite's
    /// wrapper so callers handle a single `rusqlite::Error`.
    /// Use for single statements, read queries, and `execute_batch`.
    pub(crate) async fn call<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let read_gate = Arc::clone(&self.read_gate);
        flatten(
            self.inner
                .call(move |c| {
                    let _read_gate = read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    Ok(f(c))
                })
                .await,
        )
    }

    /// Run `f` inside a `BEGIN DEFERRED` read transaction, ending it with a
    /// rollback because nothing is written.
    ///
    /// Statements run through [`call`](Self::call) are in autocommit, so each
    /// one takes its own snapshot and another connection's commit can land
    /// between two of them. Any read that follows a row into a child table must
    /// use this instead: one snapshot for the whole read is what makes "a
    /// parent row is visible only together with its children" true for the
    /// reader as well as for the writer. Single-statement reads may stay on
    /// `call`.
    pub(crate) async fn read<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        let read_gate = Arc::clone(&self.read_gate);
        flatten(
            self.inner
                .call(move |c| {
                    let _read_gate = read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
                    let value = f(&tx)?;
                    tx.rollback()?;
                    Ok(Ok(value))
                })
                .await,
        )
    }

    /// The epoch `F` the most recent fence on this connection read (ADR 0115
    /// §2.3), or the installer's admitted `F` before any write.
    pub(crate) fn fleet(&self) -> FleetFormat {
        self.fence.observed()
    }

    /// Stand this connection's writers up on `fleet`, pin table included,
    /// until a fence reads another epoch.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn observe_fleet_for_testing(&self, fleet: FleetFormat) {
        *self
            .fence
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fleet;
    }

    /// The installer: the one write transaction that runs before the
    /// database's `lash_compat` row exists, so it is not fenced. `f` admits
    /// or provisions the database's stamp and answers the admitted `F`,
    /// inside the same `BEGIN IMMEDIATE`; on success the fence is armed with
    /// `database` and `writable`, and every later write on this connection or
    /// its clones passes it.
    pub(crate) async fn install<F>(
        &self,
        database: SqliteDatabase,
        writable: VersionRange,
        f: F,
    ) -> rusqlite::Result<()>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<FleetFormat> + Send + 'static,
    {
        let write_gate = Arc::clone(&self.write_gate);
        let read_gate = Arc::clone(&self.read_gate);
        let fleet = flatten(
            self.inner
                .call(move |c| {
                    let _read_gate = read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _write_gate = write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let fleet = f(&tx)?;
                    tx.commit()?;
                    Ok(Ok(fleet))
                })
                .await,
        )?;
        let armed = ArmedFence { database, writable };
        if *self.fence.armed.get_or_init(|| armed) != armed {
            return Err(crate::compat::malformed(
                database,
                "the connection is already armed for another database or range",
            ));
        }
        self.fence.observe(fleet);
        Ok(())
    }

    /// Run `f` inside a fenced `BEGIN IMMEDIATE` transaction on the connection
    /// thread, committing on `Ok` and rolling back (via drop) on `Err`. The
    /// write lock is acquired up front, and the writer fence is the
    /// transaction's first statement. Use this for every read-then-write path.
    ///
    /// The database's in-process write gate is taken on the connection thread
    /// and held until the transaction ends (FIG-3975): writers in this process
    /// wait on it instead of contending through `busy_timeout`, which remains
    /// for other processes' writers. It is never held across an `.await`, so a
    /// caller whose future is suspended mid-call (a Restate handler) cannot
    /// keep it: the transaction runs to its end on the connection thread
    /// whether or not the caller is polled again.
    pub(crate) async fn write<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.write_flow(move |tx| f(tx).map(TxOutcome::Commit))
            .await
    }

    /// Like [`write`](Self::write) but the closure decides commit vs rollback
    /// via [`TxOutcome`], in either case still returning a value. Lets a path
    /// keep transactional atomicity when it must abandon partially-applied
    /// writes.
    pub(crate) async fn write_flow<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<TxOutcome<T>> + Send + 'static,
    {
        let write_gate = Arc::clone(&self.write_gate);
        let read_gate = Arc::clone(&self.read_gate);
        let checkpoint = self.checkpoint.clone();
        let fence = Arc::clone(&self.fence);
        #[cfg(feature = "testing")]
        let fault_injector = self.fault_injector.clone();
        flatten(
            self.inner
                .call(move |c| {
                    let _read_gate = read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    #[cfg(feature = "perf-witness")]
                    let waiting_since = Instant::now();
                    let _write_gate = write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    #[cfg(feature = "perf-witness")]
                    let wait = waiting_since.elapsed();
                    #[cfg(feature = "perf-witness")]
                    let holding_since = Instant::now();
                    let result = (|| {
                        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        #[cfg(feature = "testing")]
                        let write_transaction_ordinal = fault_injector
                            .as_ref()
                            .map_or(0, crate::testing::SqliteFaultInjector::begin_write);
                        sim_fault!(fault_injector, AfterBegin, write_transaction_ordinal);
                        let fleet = fence.check(&tx)?;
                        sim_fault!(fault_injector, AfterFence, write_transaction_ordinal);
                        let tx = FencedTx { tx, fleet };
                        let outcome = f(&tx)?;
                        let value = match outcome {
                            TxOutcome::Commit(value) => {
                                sim_fault!(fault_injector, BeforeCommit, write_transaction_ordinal);
                                sim_fault!(fault_injector, CommitIo, write_transaction_ordinal);
                                tx.tx.commit()?;
                                value
                            }
                            TxOutcome::Rollback(value) => {
                                tx.tx.rollback()?;
                                value
                            }
                        };
                        Ok(Ok(value))
                    })();
                    #[cfg(feature = "perf-witness")]
                    let hold = holding_since.elapsed();
                    drop(_write_gate);
                    #[cfg(feature = "perf-witness")]
                    record_gate_timing(wait, hold);
                    if result.is_ok()
                        && let Some(checkpoint) = &checkpoint
                    {
                        checkpoint.commits.fetch_add(1, Ordering::Release);
                    }
                    result
                })
                .await,
        )
    }
}

/// `Connection::execute` through the connection's prepared-statement cache
/// (FIG-3975): rusqlite's own `execute` re-prepares its SQL on every call, so
/// every repeated write statement routes here instead.
pub(crate) fn cached_execute(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<usize> {
    conn.prepare_cached(sql)?.execute(params)
}

/// Collapse a `tokio_rusqlite::Result<rusqlite::Result<T>>` into a single
/// `rusqlite::Result<T>`. tokio-rusqlite carries the closure's `rusqlite::Error`
/// in its `Error::Error` variant; `Error::ConnectionClosed` / `Error::Close` are
/// surfaced as a `rusqlite::Error` so the whole crate maps one error type.
fn flatten<T>(result: tokio_rusqlite::Result<rusqlite::Result<T>>) -> rusqlite::Result<T> {
    match result {
        Ok(inner) => inner,
        Err(tokio_rusqlite::Error::Error(err)) => Err(err),
        Err(other) => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::other(other.to_string()),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A connection to a bare scratch database whose installer laid down only
    /// the durable core's `lash_compat` row, so its writes pass the fence.
    async fn installed(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
    ) -> SqliteConnection {
        let connection = SqliteConnection::open_with_policy(target, policy)
            .await
            .expect("open scratch connection");
        connection
            .install(SqliteDatabase::DurableCore, FleetFormat::writable(), |tx| {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS lash_compat (
                             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                             component TEXT NOT NULL,
                             version INTEGER NOT NULL,
                             min_reader INTEGER NOT NULL,
                             fleet_format INTEGER NOT NULL
                         )",
                )?;
                if crate::compat::read(tx, SqliteDatabase::DurableCore)?.is_none() {
                    crate::compat::provision(tx, SqliteDatabase::DurableCore)?;
                }
                Ok(FleetFormat::current())
            })
            .await
            .expect("install the scratch stamp");
        connection
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[expect(
        clippy::disallowed_methods,
        reason = "test inspects the physical WAL size"
    )]
    async fn checkpoint_truncates_after_a_reader_drains_without_another_write() {
        let dir = tempfile::tempdir().expect("checkpoint test tempdir");
        let path = dir.path().join("core.db");
        let target = DatabaseTarget::File(path.clone());
        let writer = installed(
            &target,
            SqliteConnectionPolicy {
                wal_autocheckpoint_pages: 16,
                ..SqliteConnectionPolicy::default()
            },
        )
        .await;
        writer
            .write(|tx| {
                tx.execute_batch("CREATE TABLE payloads (body BLOB NOT NULL)")?;
                Ok(())
            })
            .await
            .expect("create payload table");

        let reader = Connection::open(&path).expect("open WAL reader");
        reader
            .execute_batch("BEGIN")
            .expect("begin read transaction");
        reader
            .query_row("SELECT COUNT(*) FROM payloads", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("pin the reader snapshot");
        for _ in 0..80 {
            writer
                .write(|tx| {
                    tx.execute("INSERT INTO payloads VALUES (?1)", [vec![7_u8; 4096]])?;
                    Ok(())
                })
                .await
                .expect("commit burst write");
        }
        let mut wal_name = path.into_os_string();
        wal_name.push("-wal");
        let wal_path = std::path::PathBuf::from(wal_name);
        let grown = std::fs::metadata(&wal_path).expect("WAL after burst").len();
        assert!(
            grown > 16 * 4096,
            "WAL did not exceed the threshold: {grown}"
        );

        // Keep short store readers overlapping after the pinned reader leaves.
        // The checkpoint must stop new readers long enough to truncate.
        let keep_reading = Arc::new(AtomicBool::new(true));
        let active_readers = Arc::new(AtomicUsize::new(0));
        let mut reader_tasks = Vec::new();
        for _ in 0..8 {
            let connection = SqliteConnection::open(&target)
                .await
                .expect("open concurrent reader");
            let keep_reading = Arc::clone(&keep_reading);
            let active_readers = Arc::clone(&active_readers);
            reader_tasks.push(tokio::spawn(async move {
                while keep_reading.load(Ordering::Relaxed) {
                    let active_readers = Arc::clone(&active_readers);
                    connection
                        .read(move |tx| {
                            tx.query_row("SELECT COUNT(*) FROM payloads", [], |row| {
                                row.get::<_, i64>(0)
                            })?;
                            active_readers.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(10));
                            active_readers.fetch_sub(1, Ordering::Relaxed);
                            Ok(())
                        })
                        .await
                        .expect("concurrent read");
                }
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while active_readers.load(Ordering::Relaxed) < 4 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("concurrent readers started");

        reader
            .execute_batch("ROLLBACK")
            .expect("release reader snapshot");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if std::fs::metadata(&wal_path).is_ok_and(|metadata| metadata.len() == 0) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("idle WAL should truncate without another write");
        keep_reading.store(false, Ordering::Relaxed);
        for task in reader_tasks {
            task.await.expect("reader task");
        }
    }

    /// The counting busy handler the gate proofs read: every `SQLITE_BUSY`
    /// waits here instead of inside an untracked `busy_timeout`, so a run
    /// can prove no in-process writer ever entered SQLite's sleep path
    /// (FIG-3975). It retries like the default handler so genuinely
    /// contended work — a writer in another process — still completes.
    static BUSY_SLEEPS: AtomicUsize = AtomicUsize::new(0);

    fn counting_busy_handler(previous_invocations: i32) -> bool {
        BUSY_SLEEPS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
        previous_invocations < 5_000
    }

    /// FIG-3975's binding proof: sixteen connections to one database writing
    /// concurrently queue on the shared gate, so none ever contends for
    /// SQLite's write lock — the busy handler's count stays at zero. Without
    /// the gate the same writes sleep in `busy_timeout` and are counted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_process_writers_queue_on_the_gate_without_a_busy_sleep() {
        let dir = tempfile::tempdir().expect("gate test tempdir");
        let target = DatabaseTarget::File(dir.path().join("core.db"));
        let mut connections = Vec::with_capacity(16);
        for _ in 0..16 {
            connections.push(installed(&target, SqliteConnectionPolicy::default()).await);
        }
        connections[0]
            .write(|tx| {
                tx.execute_batch("CREATE TABLE writes_seen (n INTEGER)")?;
                Ok(())
            })
            .await
            .expect("create the contention table");
        for connection in &connections {
            connection
                .call(|c| c.busy_handler(Some(counting_busy_handler)))
                .await
                .expect("install the counting busy handler");
        }
        BUSY_SLEEPS.store(0, Ordering::SeqCst);
        let mut writers = Vec::new();
        for connection in connections {
            writers.push(tokio::spawn(async move {
                for _ in 0..50 {
                    connection
                        .write(|tx| {
                            crate::conn::cached_execute(
                                tx,
                                "INSERT INTO writes_seen VALUES (1)",
                                [],
                            )?;
                            Ok(())
                        })
                        .await
                        .expect("queued write");
                }
            }));
        }
        for writer in writers {
            writer.await.expect("writer task");
        }
        assert_eq!(
            BUSY_SLEEPS.load(Ordering::SeqCst),
            0,
            "an in-process writer slept in SQLite's busy handler"
        );
    }
}
