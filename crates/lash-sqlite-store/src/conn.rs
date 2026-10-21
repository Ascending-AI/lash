//! The shared async connection wrapper over SQLite.
//!
//! Every module in this crate talks to SQLite through [`SqliteConnection`]:
//! a single cheaply-clonable handle whose database operations all run on the
//! connection's own background thread. The last handle drains and joins it.
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
//!   lease fencing, a run's admission) must therefore use
//!   [`SqliteConnection::write`], which opens `BEGIN IMMEDIATE` so the write
//!   lock is acquired up front and a contending writer waits on the busy
//!   timeout instead of reading a stale snapshot. In-process writers never
//!   reach that wait: every connection opened on one database shares a
//!   process-wide gate (FIG-3975), taken on the connection thread before
//!   `BEGIN IMMEDIATE` and released when the transaction ends, so in-process
//!   contention wakes on the gate's release rather than sleeping in SQLite's
//!   busy handler. A deployment is one database file (ADR 0132 §12), so one
//!   gate orders every writer of every table in it. The gate is never held
//!   across an `.await`, so a suspended caller cannot keep it.
//!   `busy_timeout` still stands for writers in other processes.
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

use lash_core_store::compat::VersionRange;
use lash_core_store::store::FleetFormat;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;
mod batch;
mod checkpoint;
mod worker;
use checkpoint::checkpoint_state;
use worker::Connection as AsyncConnection;

use crate::location::DatabaseTarget;

use crate::observed_sql::open_connection;

/// Outcome a write flow returns to decide commit vs rollback while still
/// handing a value back to the caller. Used for paths that compute a result
/// *and* may discover mid-transaction that the work must not be persisted
/// (e.g. a contended run admission, where partially bound rows must be
/// rolled back and the caller told nothing was admitted).
pub(crate) enum TxOutcome<T> {
    Commit(T),
    Rollback(T),
}

/// Busy timeout applied to every connection. Matches the prior store's
/// 15-second window so cross-process writers wait rather than fail fast.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 15_000;

/// The process-wide write gates, one per database identity
/// ([`DatabaseTarget::canonical_name`], so differently-spelled paths and
/// `memdb` names of the same database share one gate). Entries are weak so
/// a closed backend's gate is forgotten; the connections keep it alive.
static WRITE_GATES: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static READ_GATES: LazyLock<Mutex<HashMap<String, Weak<RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

/// SQLite's `synchronous` mode for a store's connections: what a commit
/// waits for before it answers, and so what a power loss or operating-system
/// crash can take back.
///
/// The host states it for every file store; there is no default
/// (D-DEFAULTS2). The store runs in WAL mode, where the modes differ as
/// SQLite documents (<https://www.sqlite.org/pragma.html#pragma_synchronous>,
/// <https://www.sqlite.org/wal.html>). An application crash alone loses
/// nothing in any mode. Exactly-once effect admission survives power loss
/// only under [`Self::Full`], assuming the storage honors sync requests.
/// `Normal` and `Off` explicitly select weaker contracts: losing an
/// acknowledged `Once` start can allow its body to execute again after recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteSynchronous {
    /// Never wait for the filesystem. Fastest writes, but a power loss or
    /// operating-system crash can corrupt the database, not only lose recent
    /// commits. For stores whose loss is acceptable; no exactly-once admission
    /// guarantee across power loss.
    Off,
    /// Sync the WAL at checkpoints, not at each commit. The database stays
    /// consistent through a power loss, but the commits since the last
    /// checkpoint can be rolled back: a turn lash answered as committed may
    /// be gone after the machine restarts. Exactly-once admission across power
    /// loss is not guaranteed, even if the tool already performed its effect.
    Normal,
    /// Sync the WAL at every commit. A commit that answered survives a power
    /// loss when the storage honors sync requests. This is the mode required
    /// for exactly-once effect admission across power loss; each write
    /// transaction waits for the WAL sync before returning.
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
    /// What a commit waits for, and so what a power loss can take back: see
    /// [`SqliteSynchronous`]. Stated by the host; it has no default.
    pub synchronous: SqliteSynchronous,
    /// WAL pages a file store's commits leave before its background worker
    /// checkpoints and truncates the WAL, off the committing thread. The
    /// default is SQLite's 1,000-page autocheckpoint setting; `0` disables
    /// automatic checkpointing. Lower it to bound WAL growth, or raise it
    /// when checkpoint overhead matters more than reader lag.
    pub wal_autocheckpoint_pages: u32,
    /// SQLite cache-size pragma value, preserving SQLite's sign overload. The
    /// default is SQLite's `-2000` value (approximately 2,000 KiB); negative
    /// values count KiB and positive values count pages, so change it to match
    /// the deployment's memory budget and page size.
    pub cache_size: i32,
    /// Coordination, readonly readers and working batches.
    pub operational: crate::SqliteOperationalSettings,
}

impl SqliteConnectionPolicy {
    /// Standard production: four readers, 15 s busy timeout, 1000-page WAL
    /// checkpoint and -2000 KiB cache, plus standard operational settings.
    /// WAL/cache follow SQLite conventions; no universal workload measurement
    /// backs the reader count or busy budget. The host states durability.
    pub fn standard(synchronous: SqliteSynchronous) -> Self {
        Self {
            read_connections: std::num::NonZeroUsize::new(4).unwrap_or(std::num::NonZeroUsize::MIN),
            busy_timeout: Duration::from_millis(BUSY_TIMEOUT_MS as u64),
            synchronous,
            wal_autocheckpoint_pages: 1_000,
            cache_size: -2_000,
            operational: crate::SqliteOperationalSettings::standard(),
        }
    }
}

impl SqliteConnectionPolicy {
    /// Development uses one reader, a 2 s busy budget and the smaller
    /// operational development capacities; WAL and cache remain standard.
    /// These capacity reductions are unmeasured. The host states durability.
    pub fn development(synchronous: SqliteSynchronous) -> Self {
        Self {
            read_connections: std::num::NonZeroUsize::MIN,
            busy_timeout: Duration::from_secs(2),
            operational: crate::SqliteOperationalSettings::development(),
            ..Self::standard(synchronous)
        }
    }
}

/// Count physical SQL in the caller's opt-in operation window. PROFILE's
/// quantized clock is ignored; work is the difference from STMT to PROFILE.
fn install_perf_statement_witness(connection: &Connection) {
    if cfg!(feature = "perf-witness") {
        crate::observed_sql::enable(connection);
    }
}

/// Switch a file-backed connection into WAL mode, retrying on lock contention.
///
/// SQLite acquires an exclusive lock to convert the rollback journal to WAL and,
/// unlike ordinary writes, does **not** call the registered busy handler while
/// doing so. When many connections open a brand-new database at once they race
/// on that conversion and all but one get `SQLITE_BUSY`/"database is locked".
/// We therefore retry the conversion ourselves with a short backoff until the
/// busy-timeout budget is exhausted.
fn set_wal_journal_mode(
    c: &Connection,
    busy_timeout: Duration,
    policy: crate::SqliteOperationalSettings,
) -> rusqlite::Result<()> {
    let deadline = std::time::Instant::now() + busy_timeout;
    let mut backoff = policy.wal_retry_initial;
    loop {
        match c.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(err) if is_busy(&err) && std::time::Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2).min(policy.wal_retry_max);
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
/// The installer arms it with the build's writable range for `F`. Until then
/// no write passes it.
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
                lash_core_store::store::StoreError::StorageFailure {
                    backend: crate::SQLITE_BACKEND,
                    message: "write on a SQLite connection whose database no installer admitted"
                        .to_owned(),
                },
            ));
        };
        let fleet = crate::compat::fence(tx, armed.writable)?;
        Ok(self.observe(fleet))
    }

    /// The build's writable range for `F`, once an installer armed the fence.
    fn writable(&self) -> Option<VersionRange> {
        self.armed.get().map(|armed| armed.writable)
    }
}

/// A write transaction past its fence: the transaction, and the epoch `F`
/// the fence read. Everything the transaction encodes for durable storage is
/// encoded under [`fleet`](Self::fleet), so nothing it writes straddles a
/// finalize.
pub(crate) struct FencedTx<'c> {
    pub(crate) operational: crate::SqliteOperationalSettings,
    pub(crate) configs: Arc<lash_core_store::store_backend_support::ConfigCache>,
    tx: Transaction<'c>,
    fleet: FleetFormat,
    /// The build's writable range for `F`: the fleet is finalized at this
    /// build's epoch when `fleet` is its top.
    writable: Option<VersionRange>,
}

impl FencedTx<'_> {
    /// The epoch this transaction runs under.
    pub(crate) fn fleet(&self) -> FleetFormat {
        self.fleet
    }

    /// Admit the plugin namespaces this transaction publishes against the
    /// fleet record's writer ranges (FIG-4746), before the transaction writes
    /// any of them. The reserved lock the transaction holds keeps a finalize
    /// from moving a range until it ends. A refusal is typed, and the
    /// caller's rollback leaves nothing published.
    pub(crate) fn admit_plugin_writers(
        &self,
        publication: &lash_core_store::store::plugin_writers::PluginPublication,
    ) -> rusqlite::Result<()> {
        crate::compat::admit_plugin_writers(&self.tx, publication)
    }

    /// Whether the fleet epoch is this build's own: no older build writes
    /// beside this transaction.
    pub(crate) fn finalized(&self) -> bool {
        self.writable
            .is_some_and(|writable| self.fleet.version() == writable.max())
    }
}

impl<'c> std::ops::Deref for FencedTx<'c> {
    type Target = Transaction<'c>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

/// Cheaply-clonable async handle to one SQLite database. Cloning shares the
/// same underlying connection thread, which the last handle joins, so
/// the `SqliteStore` can keep a single `SqliteConnection` and hand `&self` borrows of
/// it to every module.
#[derive(Clone)]
pub(crate) struct SqliteConnection {
    pub(crate) operational: crate::SqliteOperationalSettings,
    inner: AsyncConnection,
    #[cfg(feature = "perf-witness")]
    reader: bool,
    /// The gate every in-process writer to this connection's database queues
    /// on (FIG-3975), shared by all connections that open it.
    write_gate: Arc<Mutex<()>>,
    read_gate: Arc<RwLock<()>>,
    fence: Arc<WriterFence>,
    /// What this connection's queued writes share on its thread.
    writer: Arc<batch::Writer>,
    /// Where a producer commit on this connection hands the actor it woke,
    /// after the commit: the store set's [`WakeSink`](lash_durable::runner::WakeSink).
    wakes: lash_durable::runner::WakeSink,
    #[cfg(feature = "testing")]
    pauses: Option<crate::testing::SqlitePauses>,
    #[cfg(feature = "testing")]
    inline_calls: bool,
}

impl SqliteConnection {
    /// Committed configuration shared by this writer and its store's readers.
    pub(crate) fn configs(&self) -> &Arc<lash_core_store::store_backend_support::ConfigCache> {
        &self.writer.configs
    }

    /// Install this file's transaction-local wake capture after its schema exists.
    pub(crate) async fn install_node_wakes(
        &self,
        writer: crate::durable::NodeWakeWriter,
    ) -> rusqlite::Result<()> {
        let capture = writer.capture();
        self.call(move |c| {
            capture.register(c)?;
            c.execute_batch(crate::durable::NODE_WAKE_CAPTURE)
        })
        .await?;
        *self
            .writer
            .node_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
        Ok(())
    }

    /// The wake sink this connection's producer commits deliver to.
    pub(crate) fn wakes(&self) -> &lash_durable::runner::WakeSink {
        &self.wakes
    }

    /// Hand `woken`, which a transaction on this connection committed, to
    /// the nodes attached to the store set.
    pub(crate) fn woke(&self, woken: Option<&lash_durable::Woken>) {
        if let Some(woken) = woken {
            self.wakes.wake(woken);
        }
    }

    /// This connection's pause controller, when one was installed at open.
    #[cfg(feature = "testing")]
    pub(crate) fn pauses(&self) -> Option<crate::testing::SqlitePauses> {
        self.pauses.clone()
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
    pub(crate) async fn open_with_policy(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(
            target,
            policy,
            #[cfg(feature = "testing")]
            crate::testing::ConnectionHooks::default(),
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn open_with_hooks(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
        hooks: crate::testing::ConnectionHooks,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(target, policy, hooks).await
    }

    /// This connection, running each call to its answer before its caller
    /// goes on when `inline_calls` is set.
    #[cfg(feature = "testing")]
    pub(crate) fn with_inline_calls(mut self, inline_calls: bool) -> Self {
        self.inline_calls = inline_calls;
        self
    }

    /// One open path for every target: a `memdb` database answers the WAL
    /// switch with its own `memory` journal mode, so the file and memory
    /// forms differ only in the name opened.
    async fn open_configured(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
        #[cfg(feature = "testing")] hooks: crate::testing::ConnectionHooks,
    ) -> tokio_rusqlite::Result<Self> {
        policy.operational.validate()?;
        Self::check_release_configured(
            target,
            crate::release_stamp::BUILD_RELEASE,
            policy.operational,
        )
        .await?;
        let gate = write_gate(target);
        let reads = read_gate(target);
        let path = target.open_name();
        #[cfg(feature = "perf-witness")]
        lash_core_ids::perf_witness::startup::record(
            lash_core_ids::perf_witness::startup::Phase::StoreOpenStarted,
        );
        let inner =
            AsyncConnection::start(move || open_connection(path, rusqlite::OpenFlags::default()))
                .await?;
        #[cfg(feature = "perf-witness")]
        lash_core_ids::perf_witness::startup::record(
            lash_core_ids::perf_witness::startup::Phase::StoreOpened,
        );
        let pragmas = crate::connection_sql::open_pragmas(policy);
        let setup_gate = Arc::clone(&gate);
        let checkpoint = checkpoint_state(
            target,
            &gate,
            &reads,
            policy.wal_autocheckpoint_pages,
            policy.operational,
        );
        inner
            .call(move |c| {
                // Journal setup must queue with schema installation and other
                // in-process writers, just like BEGIN IMMEDIATE. SQLite's WAL
                // transition can otherwise race their schema/header locks.
                #[cfg(feature = "perf-witness")]
                let mut setup_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.setup_gate");
                let _setup_gate = setup_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut setup_timing {
                    timing.start_service();
                    timing.complete();
                }
                // Install the busy handler through the rusqlite API *before* the
                // WAL conversion so ordinary write contention waits on it.
                c.busy_timeout(policy.busy_timeout)?;
                c.set_prepared_statement_cache_capacity(
                    policy.operational.statement_cache_capacity,
                );
                // The WAL switch is not covered by the busy handler, so it gets
                // its own bounded retry loop (see `set_wal_journal_mode`).
                set_wal_journal_mode(c, policy.busy_timeout, policy.operational)?;
                c.execute_batch(&pragmas)?;
                // The file's checkpoint worker replaces SQLite's own
                // autocheckpoint, so no commit checkpoints on this thread.
                if let Some(checkpoint) = &checkpoint {
                    checkpoint::report_commits(c, checkpoint);
                }
                install_perf_statement_witness(c);
                Ok(())
            })
            .await?;
        #[cfg(feature = "perf-witness")]
        lash_core_ids::perf_witness::startup::record(
            lash_core_ids::perf_witness::startup::Phase::StoreSetupFinished,
        );
        let fence = WriterFence::new();
        Ok(Self {
            operational: policy.operational,
            inner,
            #[cfg(feature = "perf-witness")]
            reader: false,
            writer: Arc::new(batch::Writer {
                configs: Arc::default(),
                operational: policy.operational,
                write_gate: Arc::clone(&gate),
                read_gate: Arc::clone(&reads),
                serializes_readers: matches!(target, DatabaseTarget::Memory(_)),
                fence: Arc::clone(&fence),
                node_wakes: Mutex::new(None),
                #[cfg(feature = "testing")]
                pauses: hooks.pauses.clone(),
            }),
            write_gate: gate,
            read_gate: reads,
            fence,
            wakes: lash_durable::runner::WakeSink::default(),
            #[cfg(feature = "testing")]
            pauses: hooks.pauses,
            #[cfg(feature = "testing")]
            inline_calls: hooks.inline_calls,
        })
    }

    #[cfg(test)]
    pub(crate) async fn open_readonly(target: &DatabaseTarget) -> tokio_rusqlite::Result<Self> {
        Self::open_readonly_configured(target, crate::SqliteOperationalSettings::standard()).await
    }

    pub(crate) async fn open_readonly_configured(
        target: &DatabaseTarget,
        operational: crate::SqliteOperationalSettings,
    ) -> tokio_rusqlite::Result<Self> {
        operational.validate()?;

        let gate = write_gate(target);
        let setup_gate = Arc::clone(&gate);
        let uri = target.read_only_uri();
        let inner = AsyncConnection::start(move || {
            open_connection(
                uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
        })
        .await?;
        inner
            .call(move |c| {
                // Even cache_size reads the database schema. A first opener
                // must wait for an in-process installer or WAL transition
                // before spending the read-only SQLite busy budget.
                #[cfg(feature = "perf-witness")]
                let mut setup_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.setup_gate");
                let _setup_gate = setup_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut setup_timing {
                    timing.start_service();
                    timing.complete();
                }
                c.busy_timeout(operational.readonly_busy_timeout)?;
                c.set_prepared_statement_cache_capacity(operational.statement_cache_capacity);
                c.pragma_update(None, "cache_size", operational.readonly_cache_size)?;
                install_perf_statement_witness(c);
                Ok(())
            })
            .await?;
        let reads = read_gate(target);
        let fence = WriterFence::new();
        Ok(Self {
            operational,
            inner,
            #[cfg(feature = "perf-witness")]
            reader: true,
            writer: Arc::new(batch::Writer {
                configs: Arc::default(),
                operational,
                write_gate: Arc::clone(&gate),
                read_gate: Arc::clone(&reads),
                serializes_readers: matches!(target, DatabaseTarget::Memory(_)),
                fence: Arc::clone(&fence),
                node_wakes: Mutex::new(None),
                #[cfg(feature = "testing")]
                pauses: None,
            }),
            write_gate: gate,
            read_gate: reads,
            fence,
            wakes: lash_durable::runner::WakeSink::default(),
            #[cfg(feature = "testing")]
            pauses: None,
            #[cfg(feature = "testing")]
            inline_calls: false,
        })
    }

    /// Refuse pre-release file stores before journal setup, recovery or backup
    /// can change their bytes. The installer checks again in its transaction.
    #[cfg(test)]
    pub(crate) async fn check_release_before_open(
        target: &DatabaseTarget,
        build_release: &str,
    ) -> tokio_rusqlite::Result<()> {
        Self::check_release_configured(
            target,
            build_release,
            crate::SqliteOperationalSettings::standard(),
        )
        .await
    }

    pub(crate) async fn check_release_configured(
        target: &DatabaseTarget,
        build_release: &str,
        policy: crate::SqliteOperationalSettings,
    ) -> tokio_rusqlite::Result<()> {
        // A named SQLite memory database belongs to this process's build.
        if !matches!(target, DatabaseTarget::File(_)) || !target.exists() {
            return Ok(());
        }
        let connection = Self::open_readonly_configured(target, policy).await?;
        let build_release = build_release.to_owned();
        let result = connection.call(move |c| {
            let writing_release = crate::release_stamp::read_release(c);
            if let Some(refusal) = lash_core_store::compat::CompatRefusal::pre_release(
                crate::schema::COMPONENT.as_str(), writing_release.as_deref(), &build_release,
            ) {
                tracing::warn!(writing_release = ?writing_release, build_release, refusal = %refusal, "lash SQLite release gate refused the database");
                Err(crate::sqlite_conversion_error(lash_core_store::store::StoreError::Incompatible { refusal }))
            } else {
                Ok(())
            }
        }).await;
        let closed = connection.inner.close().await;
        result.map_err(tokio_rusqlite::Error::Error)?;
        closed?;
        Ok(())
    }

    /// Read a migration stamp without provisioning or changing the database.
    pub(crate) async fn migration_stamp(
        target: &DatabaseTarget,
        busy_timeout: Duration,
        operational: crate::SqliteOperationalSettings,
    ) -> rusqlite::Result<Option<lash_core_store::compat::CompatStamp>> {
        let connection = flatten(
            Self::open_readonly_configured(target, operational)
                .await
                .map(Ok),
        )?;
        let result = connection
            .call(move |c| {
                c.busy_timeout(busy_timeout)?;
                crate::compat::read(c).map(|row| row.map(|(stamp, _)| stamp))
            })
            .await;
        let closed = flatten(connection.inner.close().await.map(Ok));
        let stamp = result?;
        closed?;
        Ok(stamp)
    }

    /// Recover a hot journal and checkpoint committed pages before a migration
    /// copies the database. No installer, journal-mode switch or row write runs;
    /// the connection closes before the caller checks whether another owner
    /// still holds the WAL.
    pub(crate) async fn checkpoint_for_migration(
        target: &DatabaseTarget,
        busy_timeout: Duration,
    ) -> rusqlite::Result<()> {
        let uri = target.uri();
        let connection = AsyncConnection::start(move || {
            open_connection(
                uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
        })
        .await?;
        let result = flatten(
            connection
                .call(move |c| {
                    c.busy_timeout(busy_timeout)?;
                    c.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                    let mode: String = c.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                    if mode.eq_ignore_ascii_case("wal") {
                        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                            row.get::<_, i64>(0)
                        })?;
                    }
                    Ok(())
                })
                .await
                .map(Ok),
        );
        let closed = flatten(connection.close().await.map(Ok));
        result?;
        closed
    }

    /// Run `f` on the connection thread; with inline calls, to its answer
    /// before the caller goes on.
    async fn run<T, F>(&self, f: F) -> tokio_rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let window = lash_core_ids::operational_metrics::sql::Window::current();
        #[cfg(feature = "perf-witness")]
        let reader = self.reader;
        let f = move |c: &mut Connection| {
            crate::observed_sql::physical_scope(std::any::type_name::<F>(), || {
                #[cfg(feature = "perf-witness")]
                let _busy = lash_core_ids::perf_witness::latency::Busy::enter(reader);
                let observed = window.is_some() && !cfg!(feature = "perf-witness");
                if observed {
                    crate::observed_sql::enable(c);
                }
                #[cfg(feature = "perf-witness")]
                let writes = crate::observed_sql::WriteWitness::start(c);
                let result =
                    lash_core_ids::operational_metrics::sql::Window::on_worker(window, || f(c));
                #[cfg(feature = "perf-witness")]
                writes.finish(c);
                if observed {
                    c.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
                }
                result
            })
        };
        #[cfg(feature = "testing")]
        if self.inline_calls {
            return self.inner.call_inline(f);
        }
        self.inner.call(f).await
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
            self.run(move |c| {
                #[cfg(feature = "perf-witness")]
                let mut read_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.read_gate");
                let _read_gate = timed_gate(|| {
                    read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                });
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut read_timing {
                    timing.start_service();
                    timing.complete();
                }
                Ok(crate::observed_sql::with_origin(
                    std::any::type_name::<F>(),
                    || f(c),
                ))
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
            self.run(move |c| {
                #[cfg(feature = "perf-witness")]
                let mut read_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.read_gate");
                let _read_gate = timed_gate(|| {
                    read_gate
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                });
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut read_timing {
                    timing.start_service();
                    timing.complete();
                }
                let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
                let value =
                    crate::observed_sql::with_origin(std::any::type_name::<F>(), || f(&tx))?;
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
    /// `writable`, and every later write on this connection or its clones
    /// passes it.
    pub(crate) async fn install<F>(&self, writable: VersionRange, f: F) -> rusqlite::Result<()>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<FleetFormat> + Send + 'static,
    {
        let write_gate = Arc::clone(&self.write_gate);
        let writer = Arc::clone(&self.writer);
        let fleet = flatten(
            self.run(move |c| {
                #[cfg(feature = "perf-witness")]
                let mut read_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.read_gate");
                let _read_gate = timed_gate(|| writer.access());
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut read_timing {
                    timing.start_service();
                    timing.complete();
                }
                #[cfg(feature = "perf-witness")]
                let mut write_timing =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.write_gate");
                let _write_gate = timed_gate(|| {
                    write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                });
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut write_timing {
                    timing.start_service();
                    timing.complete();
                }
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                #[cfg(feature = "perf-witness")]
                lash_core_ids::perf_witness::latency::began_write();
                let fleet = f(&tx)?;
                tx.commit()?;
                Ok(Ok(fleet))
            })
            .await,
        )?;
        let armed = ArmedFence { writable };
        if *self.fence.armed.get_or_init(|| armed) != armed {
            return Err(crate::compat::malformed(
                "the connection is already armed for another range",
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
    /// caller whose future is suspended mid-call cannot
    /// keep it: the transaction runs to its end on the connection thread
    /// whether or not the caller is polled again.
    pub(crate) async fn write<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.write_flow(move |tx| {
            crate::observed_sql::with_origin(std::any::type_name::<F>(), || f(tx))
                .map(TxOutcome::Commit)
        })
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
        #[cfg(feature = "testing")]
        if self.inline_calls {
            let (answer, result) = std::sync::mpsc::sync_channel(1);
            flatten(
                self.inner
                    .write(batch::Member::new(&self.writer, f, move |value| {
                        let _ = answer.send(value);
                    }))
                    .map(Ok),
            )?;
            return result.recv().unwrap_or_else(|_| closed());
        }
        let (answer, result) = tokio::sync::oneshot::channel();
        flatten(
            self.inner
                .write(batch::Member::new(&self.writer, f, move |value| {
                    let _ = answer.send(value);
                }))
                .map(Ok),
        )?;
        result.await.unwrap_or_else(|_| closed())
    }
}

/// A store's read-only connections, taken in turn by its reads and the
/// durable engine's (FIG-6907), so a read does not queue on the writer's
/// thread and hold up the writes behind it. A read sees every write that
/// answered before it started, as a read on the writer does: a commit
/// answers only after its `COMMIT` returned. Other connections commit
/// between two autocommit statements here, so a read of more than one
/// statement takes one snapshot through [`SqliteConnection::read`].
#[derive(Clone)]
pub(crate) struct ReadPool(Arc<ReadPoolInner>);

struct ReadPoolInner {
    readers: Vec<SqliteConnection>,
    next: AtomicU64,
}

impl ReadPool {
    /// `readers`, at least one.
    pub(crate) fn new(readers: Vec<SqliteConnection>) -> Self {
        Self(Arc::new(ReadPoolInner {
            readers,
            next: AtomicU64::new(0),
        }))
    }

    /// The reader the next read runs on.
    pub(crate) fn next(&self) -> &SqliteConnection {
        let index = self.0.next.fetch_add(1, Ordering::Relaxed) as usize;
        &self.0.readers[index % self.0.readers.len()]
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &SqliteConnection> {
        self.0.readers.iter()
    }
}

/// The page threshold `target`'s checkpoint worker runs at, while a writer
/// connection keeps it.
#[cfg(test)]
pub(crate) fn checkpoint_threshold_for_testing(target: &DatabaseTarget) -> Option<u64> {
    checkpoint::state_for_testing(target).map(|state| state.threshold_for_testing())
}

/// The answer of a write whose connection thread ended before answering.
fn closed<T>() -> rusqlite::Result<T> {
    flatten(Err(tokio_rusqlite::Error::ConnectionClosed))
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

fn timed_gate<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(feature = "perf-witness")]
    {
        lash_core_ids::perf_witness::latency::gate(f)
    }
    #[cfg(not(feature = "perf-witness"))]
    {
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// FIG-5933 counts physical write transactions, including rollback, and
    /// never publishes a clock for a refused commit. FIG-5943 excludes writes
    /// on other connections from this law's worker counts.
    #[cfg(feature = "perf-witness")]
    #[tokio::test]
    async fn latency_witness_counts_rollbacks_without_publishing_their_clocks() {
        use lash_core_ids::perf_witness::latency::Collector;
        use lash_core_ids::perf_witness::latency::CommitMark;
        use lash_core_ids::perf_witness::latency::stage;
        let dir = tempfile::tempdir().unwrap();
        let target = DatabaseTarget::File(dir.path().join("witness.db"));
        let conn = installed(
            &target,
            SqliteConnectionPolicy::development(SqliteSynchronous::Normal),
        )
        .await;
        let reader = SqliteConnection::open_readonly_configured(
            &target,
            crate::SqliteOperationalSettings::development(),
        )
        .await
        .unwrap();
        let unrelated = installed(
            &DatabaseTarget::File(dir.path().join("unrelated.db")),
            SqliteConnectionPolicy::development(SqliteSynchronous::Normal),
        )
        .await;
        let collector = Collector::install().unwrap();
        conn.write(|_| {
            stage(|| CommitMark::Accepted {
                input: "committed".to_owned(),
            });
            Ok(())
        })
        .await
        .unwrap();
        conn.write_flow(|_| {
            stage(|| CommitMark::Accepted {
                input: "refused".to_owned(),
            });
            Ok(TxOutcome::Rollback(()))
        })
        .await
        .unwrap();
        unrelated
            .write_flow(|_| Ok(TxOutcome::Rollback(())))
            .await
            .unwrap();
        reader
            .call(|c| c.query_row("SELECT 1", [], |r| r.get::<_, u64>(0)))
            .await
            .unwrap();
        let writer_thread = conn
            .inner
            .call(|_| Ok(format!("{:?}", std::thread::current().id())))
            .await
            .unwrap();
        let reader_thread = reader
            .inner
            .call(|_| Ok(format!("{:?}", std::thread::current().id())))
            .await
            .unwrap();
        // A call answers before its Busy guard drops. Closing each worker
        // drains its final receipt before the snapshot, without a timing wait.
        conn.inner.close().await.unwrap();
        reader.inner.close().await.unwrap();
        unrelated.inner.close().await.unwrap();
        let mut snapshot = collector.snapshot();
        // The collector is process-wide; only these two connections belong
        // to this law. Other tests' workers must not affect its exact counts.
        snapshot
            .workers
            .retain(|_, worker| worker.thread == writer_thread || worker.thread == reader_thread);
        assert!(snapshot.inputs["committed"].accepted.is_some());
        assert!(!snapshot.inputs.contains_key("refused"));
        assert_eq!(
            snapshot
                .workers
                .values()
                .map(|w| w.write_transactions)
                .sum::<u64>(),
            2
        );
        assert!(
            snapshot
                .workers
                .values()
                .any(|w| w.reader && w.busy_ns > 0 && w.write_transactions == 0)
        );
        assert!(
            snapshot
                .workers
                .values()
                .any(|w| !w.reader && w.busy_ns > 0 && w.write_transactions == 2)
        );
    }

    /// SQL-WORK: repeated cached statements count rows and work in each window,
    /// rather than importing earlier executions of the same prepared statement.
    #[tokio::test]
    async fn sql_operation_reports_exact_shapes_rows_and_work() {
        use lash_core_ids::operational_metrics::sql;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = DatabaseTarget::File(dir.path().join("sql.db"));
        let conn = installed(
            &target,
            SqliteConnectionPolicy::standard(SqliteSynchronous::Normal),
        )
        .await;
        let mut first_work = None;
        for size in [2, 8, 2] {
            let (answer, receipt) = sql::collect(format!("known-select/{size}"), "sqlite", conn.call(move |c| {
                let mut statement = c.prepare_cached("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < ?1) SELECT x FROM n")?;
                statement.query_map([size], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()
            })).await;
            assert_eq!(answer.expect("query").len(), size as usize);
            assert_eq!(receipt.statements, 1);
            assert_eq!(receipt.shapes.len(), 1);
            assert!(receipt.shapes.contains_key("with recursive n(x) as (values(?) union all select x+? from n where x < ?) select x from n"));
            assert_eq!(receipt.rows_returned, size as u64);
            assert_eq!(receipt.shapes.values().next().expect("shape").statements, 1);
            let work = receipt.sqlite_work.expect("SQLite work");
            assert!(work.vm_steps > 0);
            if size == 2 {
                if let Some(first) = first_work {
                    assert_eq!(work, first, "cached executions exclude earlier work");
                }
                first_work = Some(work);
            }
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
    }

    #[test]
    fn first_open_waits_for_in_process_schema_installation() {
        let dir = tempfile::tempdir().expect("first-open directory");
        let path = dir.path().join("core.db");
        let target = DatabaseTarget::File(path.clone());
        let gate = write_gate(&target);
        let installing = gate.lock().expect("installer gate");
        let mut installer = Connection::open(&path).expect("installer connection");
        let tx = installer
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .expect("first schema transaction");
        for statements in crate::schema::provisioning_statements() {
            tx.execute_batch(statements).expect("uncommitted schema");
        }
        let (answer, result) = std::sync::mpsc::channel();
        let opener = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("opener runtime");
            let mut options = crate::StoreOptions::standard(crate::SqliteSynchronous::Normal);
            options.connection_policy.busy_timeout = Duration::from_millis(1);
            options.connection_policy.operational.readonly_busy_timeout = Duration::from_millis(1);
            let opened = runtime.block_on(crate::SqliteStore::open_file_with_options_for_testing(
                &path, options,
            ));
            answer.send(opened.map(|_| ())).expect("opener result");
        });
        // An in-process installer owns the gate longer than either SQLite
        // busy budget. Another first open must queue there rather than spend
        // its busy timeout inspecting an uncommitted schema.
        let premature = result.recv_timeout(Duration::from_millis(100));
        let waited = matches!(&premature, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        crate::compat::provision(&tx, FleetFormat::writable()).expect("schema stamp");
        crate::release_stamp::write(&tx).expect("release stamp");
        tx.commit().expect("finish schema installation");
        drop(installing);
        opener.join().expect("opener thread");
        let opened = match premature {
            Ok(opened) => opened,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => result.recv().expect("queued open"),
            Err(error) => panic!("opener disconnected: {error}"),
        };
        opened.expect("concurrent first open waits for the schema installer");
        assert!(
            waited,
            "first open must wait until schema installation commits"
        );
    }

    /// A connection to a bare scratch database whose installer laid down only
    /// its `lash_compat` row, so its writes pass the fence.
    pub(super) async fn installed(
        target: &DatabaseTarget,
        policy: SqliteConnectionPolicy,
    ) -> SqliteConnection {
        let connection = SqliteConnection::open_with_policy(target, policy)
            .await
            .expect("open scratch connection");
        connection
            .install(FleetFormat::writable(), |tx| {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS lash_compat (
                             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                             component TEXT NOT NULL,
                             version INTEGER NOT NULL,
                             min_reader INTEGER NOT NULL,
                             fleet_format INTEGER NOT NULL
                         )",
                )?;
                if crate::compat::read(tx)?.is_none() {
                    crate::compat::provision(tx, FleetFormat::writable())?;
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
                ..SqliteConnectionPolicy::standard(SqliteSynchronous::Normal)
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
            let connection = SqliteConnection::open_with_policy(
                &target,
                SqliteConnectionPolicy::standard(SqliteSynchronous::Normal),
            )
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
            connections.push(
                installed(
                    &target,
                    SqliteConnectionPolicy::standard(SqliteSynchronous::Normal),
                )
                .await,
            );
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
