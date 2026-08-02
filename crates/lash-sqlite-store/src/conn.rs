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
//!   `busy_timeout` so contending processes wait instead of failing. [`open`]
//!   and [`open_in_memory`] apply these once on the connection thread.
//!
//! * **`IMMEDIATE` write transactions.** rusqlite's `Connection::transaction`
//!   opens `BEGIN DEFERRED`, which only takes the write lock on the first write
//!   statement. Every read-then-write path the crate promises to serialise
//!   cross-process (head-revision CAS, lease fencing, the queued-work claim)
//!   must therefore use [`SqliteConnection::write`], which opens
//!   `BEGIN IMMEDIATE` so the write lock is acquired up front and a contending
//!   writer waits on the busy timeout instead of reading a stale snapshot.
//!
//! * **Error mapping.** `conn.call(...)` returns [`tokio_rusqlite::Error`],
//!   which wraps [`rusqlite::Error`]. The helpers flatten that so closures only
//!   ever deal in `rusqlite::Result<T>` and callers receive the inner
//!   `rusqlite::Error` to feed through `sqlite_error` / `process_sqlite_error`.

use rusqlite::{Connection, Transaction, TransactionBehavior};
use tokio_rusqlite::Connection as AsyncConnection;

/// Outcome a write flow returns to decide commit vs rollback while still
/// handing a value back to the caller. Used for paths that compute a result
/// *and* may discover mid-transaction that the work must not be persisted
/// (e.g. a contended queued-work claim, where partially claimed rows must be
/// rolled back and the caller told nothing was claimed).
pub(crate) enum TxOutcome<T> {
    Commit(T),
    Rollback(T),
}

/// Busy timeout applied to every connection. Matches the prior store's
/// 15-second window so cross-process writers wait rather than fail fast.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 15_000;

/// PRAGMAs applied on the connection thread immediately after open. WAL is the
/// reason this crate exists: it uses the `-wal`/`-shm` sidecars and supports
/// multi-process readers + a single writer, which the prior store's single-file mvcc mode
/// did not give us across processes.
fn open_pragmas() -> String {
    // The `journal_mode=WAL` conversion is applied separately via
    // [`set_wal_journal_mode`] because SQLite does *not* invoke the busy handler
    // for `journal_mode` changes, so concurrent first-openers must retry it by
    // hand.
    format!(
        "PRAGMA busy_timeout={BUSY_TIMEOUT_MS};\
         PRAGMA synchronous=NORMAL;\
         PRAGMA foreign_keys=ON;"
    )
}

/// Switch a file-backed connection into WAL mode, retrying on lock contention.
///
/// SQLite acquires an exclusive lock to convert the rollback journal to WAL and,
/// unlike ordinary writes, does **not** call the registered busy handler while
/// doing so. When many connections open a brand-new database at once they race
/// on that conversion and all but one get `SQLITE_BUSY`/"database is locked".
/// We therefore retry the conversion ourselves with a short backoff until the
/// busy-timeout budget is exhausted.
fn set_wal_journal_mode(c: &Connection) -> rusqlite::Result<()> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64);
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
    #[cfg(feature = "testing")]
    fault_injector: Option<crate::testing::SqliteFaultInjector>,
}

impl SqliteConnection {
    /// Open (or create) a file-backed database, applying WAL + busy-timeout
    /// PRAGMAs on the connection thread.
    pub(crate) async fn open(path: &std::path::Path) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(
            path,
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn open_with_fault_injector(
        path: &std::path::Path,
        fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_configured(path, fault_injector).await
    }

    async fn open_configured(
        path: &std::path::Path,
        #[cfg(feature = "testing")] fault_injector: Option<crate::testing::SqliteFaultInjector>,
    ) -> tokio_rusqlite::Result<Self> {
        let inner = AsyncConnection::open(path).await?;
        let pragmas = open_pragmas();
        inner
            .call(move |c| {
                // Install the busy handler through the rusqlite API *before* the
                // WAL conversion so ordinary write contention waits on it.
                c.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
                // The WAL switch is not covered by the busy handler, so it gets
                // its own bounded retry loop (see `set_wal_journal_mode`).
                set_wal_journal_mode(c)?;
                c.execute_batch(&pragmas)?;
                Ok(())
            })
            .await?;
        Ok(Self {
            inner,
            #[cfg(feature = "testing")]
            fault_injector,
        })
    }

    /// Open a private in-memory database (used by `Store::memory` and the test
    /// suites). WAL is skipped because `:memory:` does not support it.
    pub(crate) async fn open_in_memory() -> tokio_rusqlite::Result<Self> {
        let inner = AsyncConnection::open_in_memory().await?;
        // `:memory:` databases cannot use WAL, so only the tuning pragmas apply.
        let pragmas = open_pragmas();
        inner
            .call(move |c| {
                c.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
                c.execute_batch(&pragmas)?;
                Ok(())
            })
            .await?;
        Ok(Self {
            inner,
            #[cfg(feature = "testing")]
            fault_injector: None,
        })
    }

    /// Open a file-backed database read-only. Used by the export/resume call
    /// sites that must never mutate the source database.
    pub(crate) async fn open_readonly(path: &std::path::Path) -> tokio_rusqlite::Result<Self> {
        let path = path.to_path_buf();
        let inner = AsyncConnection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .await?;
        inner
            .call(move |c| {
                c.busy_timeout(std::time::Duration::from_secs(1))?;
                c.execute_batch("PRAGMA cache_size = -500;")?;
                Ok(())
            })
            .await?;
        Ok(Self {
            inner,
            #[cfg(feature = "testing")]
            fault_injector: None,
        })
    }

    /// Run `f` against the raw `rusqlite::Connection` on its own thread. The
    /// closure returns `rusqlite::Result<T>`; this method flattens
    /// tokio-rusqlite's wrapper so callers handle a single `rusqlite::Error`.
    /// Use for single statements, read queries, and `execute_batch`.
    pub(crate) async fn call<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        flatten(self.inner.call(move |c| Ok(f(c))).await)
    }

    /// Run `f` inside a `BEGIN IMMEDIATE` transaction on the connection thread,
    /// committing on `Ok` and rolling back (via drop) on `Err`. The write lock
    /// is acquired up front. Use this for every read-then-write path.
    pub(crate) async fn write<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        #[cfg(feature = "testing")]
        let fault_injector = self.fault_injector.clone();
        flatten(
            self.inner
                .call(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    #[cfg(feature = "testing")]
                    let write_transaction_ordinal = fault_injector
                        .as_ref()
                        .map_or(0, crate::testing::SqliteFaultInjector::begin_write);
                    #[cfg(feature = "testing")]
                    if let Some(injector) = fault_injector.as_ref() {
                        injector.inject(
                            crate::testing::SqliteFaultPoint::AfterBegin,
                            write_transaction_ordinal,
                        )?;
                    }
                    let value = f(&tx)?;
                    #[cfg(feature = "testing")]
                    if let Some(injector) = fault_injector.as_ref() {
                        injector.inject(
                            crate::testing::SqliteFaultPoint::BeforeCommit,
                            write_transaction_ordinal,
                        )?;
                        injector.inject(
                            crate::testing::SqliteFaultPoint::CommitIo,
                            write_transaction_ordinal,
                        )?;
                    }
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
        #[cfg(feature = "testing")]
        let fault_injector = self.fault_injector.clone();
        flatten(
            self.inner
                .call(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    #[cfg(feature = "testing")]
                    let write_transaction_ordinal = fault_injector
                        .as_ref()
                        .map_or(0, crate::testing::SqliteFaultInjector::begin_write);
                    #[cfg(feature = "testing")]
                    if let Some(injector) = fault_injector.as_ref() {
                        injector.inject(
                            crate::testing::SqliteFaultPoint::AfterBegin,
                            write_transaction_ordinal,
                        )?;
                    }
                    let outcome = f(&tx)?;
                    let value = match outcome {
                        TxOutcome::Commit(value) => {
                            #[cfg(feature = "testing")]
                            if let Some(injector) = fault_injector.as_ref() {
                                injector.inject(
                                    crate::testing::SqliteFaultPoint::BeforeCommit,
                                    write_transaction_ordinal,
                                )?;
                                injector.inject(
                                    crate::testing::SqliteFaultPoint::CommitIo,
                                    write_transaction_ordinal,
                                )?;
                            }
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
