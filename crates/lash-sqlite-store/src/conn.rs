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
//! * **Error mapping.** `conn.call(...)` returns [`tokio_rusqlite::Error`],
//!   which wraps [`rusqlite::Error`]. The helpers flatten that so closures only
//!   ever deal in `rusqlite::Result<T>` and callers receive the inner
//!   `rusqlite::Error` to feed through `sqlite_error` / `process_sqlite_error`.

use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;
use tokio_rusqlite::Connection as AsyncConnection;

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
    /// checkpointing. Lower it to bound WAL growth, or raise it when
    /// checkpoint overhead matters more than reader lag.
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

/// Cheaply-clonable async handle to one SQLite database. Cloning shares the
/// same underlying connection thread (tokio-rusqlite reference-counts it), so
/// the `Store` can keep a single `SqliteConnection` and hand `&self` borrows of
/// it to every module.
#[derive(Clone)]
pub(crate) struct SqliteConnection {
    inner: AsyncConnection,
    /// The gate every in-process writer to this database queues on (FIG-3975);
    /// shared by all connections opened on the same `canonical_name`.
    write_gate: Arc<Mutex<()>>,
    #[cfg(feature = "testing")]
    fault_injector: Option<crate::testing::SqliteFaultInjector>,
}

impl SqliteConnection {
    /// This connection's fault controller, when one was installed at open.
    #[cfg(feature = "testing")]
    pub(crate) fn fault_injector(&self) -> Option<crate::testing::SqliteFaultInjector> {
        self.fault_injector.clone()
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
            write_gate: gate,
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
        flatten(self.inner.call(move |c| Ok(f(c))).await)
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
        flatten(
            self.inner
                .call(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
                    let value = f(&tx)?;
                    tx.rollback()?;
                    Ok(Ok(value))
                })
                .await,
        )
    }

    /// Run `f` inside a `BEGIN IMMEDIATE` transaction on the connection thread,
    /// committing on `Ok` and rolling back (via drop) on `Err`. The write lock
    /// is acquired up front. Use this for every read-then-write path.
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
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        let write_gate = Arc::clone(&self.write_gate);
        #[cfg(feature = "testing")]
        let fault_injector = self.fault_injector.clone();
        flatten(
            self.inner
                .call(move |c| {
                    let _write_gate = write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    #[cfg(feature = "testing")]
                    let write_transaction_ordinal = fault_injector
                        .as_ref()
                        .map_or(0, crate::testing::SqliteFaultInjector::begin_write);
                    sim_fault!(fault_injector, AfterBegin, write_transaction_ordinal);
                    let value = f(&tx)?;
                    sim_fault!(fault_injector, BeforeCommit, write_transaction_ordinal);
                    sim_fault!(fault_injector, CommitIo, write_transaction_ordinal);
                    tx.commit()?;
                    Ok(Ok(value))
                })
                .await,
        )
    }

    /// Like [`write`](Self::write) but the closure decides commit vs rollback
    /// via [`TxOutcome`], in either case still returning a value. Lets a path
    /// keep transactional atomicity when it must abandon partially-applied
    /// writes.
    pub(crate) async fn write_flow<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<TxOutcome<T>> + Send + 'static,
    {
        let write_gate = Arc::clone(&self.write_gate);
        #[cfg(feature = "testing")]
        let fault_injector = self.fault_injector.clone();
        flatten(
            self.inner
                .call(move |c| {
                    let _write_gate = write_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    #[cfg(feature = "testing")]
                    let write_transaction_ordinal = fault_injector
                        .as_ref()
                        .map_or(0, crate::testing::SqliteFaultInjector::begin_write);
                    sim_fault!(fault_injector, AfterBegin, write_transaction_ordinal);
                    let outcome = f(&tx)?;
                    let value = match outcome {
                        TxOutcome::Commit(value) => {
                            sim_fault!(fault_injector, BeforeCommit, write_transaction_ordinal);
                            sim_fault!(fault_injector, CommitIo, write_transaction_ordinal);
                            tx.commit()?;
                            value
                        }
                        TxOutcome::Rollback(value) => {
                            tx.rollback()?;
                            value
                        }
                    };
                    Ok(Ok(value))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                SqliteConnection::open(&target)
                    .await
                    .expect("open gated connection"),
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
