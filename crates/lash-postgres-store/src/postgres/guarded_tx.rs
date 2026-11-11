//! The guarded transaction entry: the PostgreSQL writer fence (ADR 0115
//! §2.2–2.4).
//!
//! Every mutating transaction this store runs begins here, and nowhere else
//! (`scripts/check-guarded-transactions.py` holds the entry total). The fence
//! is the writer fence's lock, taken shared ahead of the session advisory
//! lock and every row lock, and then a separate statement that reads the
//! recorded epoch:
//!
//! ```sql
//! SELECT pg_advisory_xact_lock_shared(715425, 0);
//! SELECT lash_writer_fence(<the epoch last observed>);
//! ```
//!
//! Finalize takes the same lock exclusive and then moves the row, in one
//! transaction that takes no other lock first. So a writer holding the lock
//! shared makes finalize wait and commits under the old `F`, and a writer
//! whose lock waits behind finalize reads the row once finalize commits and
//! is fenced before it writes anything: under read committed the read takes
//! its snapshot after the lock is held. The two never deadlock: a writer
//! holds no other lock while it waits on the fence, and finalize takes no
//! lock a writer holds. An advisory lock writes nothing: the fence dirties no
//! page, logs no WAL and grows no MultiXact on the fleet row, which every
//! writer of the fleet would otherwise share (FIG-5275).
//!
//! The fence answers one of four ways (§2.4):
//!
//! - `F` inside this build's writable range: the transaction proceeds under
//!   it, and the handle's last observed `F` becomes it;
//! - `F` outside the range: the terminal [`StoreError::WriterFenced`];
//! - the row missing or unreadable: the terminal [`StoreError::Incompatible`],
//!   failing closed;
//! - lock contention: today's retryable [`StoreError::Contended`], and
//!   [`guarded`] retries the whole transaction, fence included.
//!
//! A refused transaction wrote nothing: the fence ran first, and dropping the
//! transaction rolls back the rest.
//!
//! The fence rides the transaction's `BEGIN`: the guard profile
//! ([`TransactionPrelude`]), the lock, the check and `BEGIN` are one simple
//! query, so the lock's wait is bounded by the profile's `lock_timeout`
//! (FIG-5240) and the fence costs no round trip of its own (FIG-7701). The
//! check expects the epoch the fence last observed and raises with the
//! recorded one otherwise, before `BEGIN` runs; an epoch the writer may
//! still write under is observed and the `BEGIN` sent again expecting it.
//! A durable commit reads the epoch in its first statement, with the clock
//! and its other envelope reads ([`Locked`]); a writer on a dedicated
//! connection reads it as its first data statement.
//!
//! A transaction stages the change-feed changes it writes; their feed
//! sequences are assigned after it commits ([`crate::change_feed`]), so no
//! writer takes a feed clock.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use lash_core_store::compat::CompatRefusal;
use lash_core_store::compat::ComponentId;
use lash_core_store::compat::VersionRange;
use lash_core_store::store::FleetFormat;
use lash_core_store::store::StoreError;
use lash_core_store::store::plugin_writers::PluginPublication;
use lash_core_store::store::plugin_writers::PluginWriterRanges;
use lash_core_store::store::plugin_writers::PluginWriterRegistration;
use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::change_feed::TurnChange;
use crate::host::{RetryPolicies, RetryPolicy, TransactionPrelude};
use crate::replayable::{Attempt, Settle, Uncommitted, XactId, commit_reconciled};
use crate::session_sql::session_sql;
use crate::store_sqlx_error;

/// One storage's writer fence: the writable range its build declares and the
/// fleet epoch its fences last observed.
///
/// Every handle a [`PostgresStorage`](crate::PostgresStorage) hands out shares
/// one, so the `F` any of them reports is the newest any of their
/// transactions read.
#[derive(Clone)]
pub(crate) struct WriterFence {
    state: Arc<FenceState>,
}

struct FenceState {
    /// `[F_prev, F_self]` of the build that opened the storage (§2.1).
    writable: VersionRange,
    /// The last `F` a fence observed, or the open's admitted `F` before any
    /// fence ran. It only moves forward: `F` never moves back, and a slow
    /// transaction that read the old epoch must not overwrite a newer one.
    observed: AtomicU32,
    /// The fleet format the storage opened under. It carries the writer-pin
    /// table a test may have stood the store up on; a fence that reads the same
    /// epoch keeps it.
    opened: FleetFormat,
    /// The guard profile every ordinary transaction begins with.
    prelude: TransactionPrelude,
    /// How [`guarded`] retries a contended transaction.
    retry: RetryPolicy,
    #[cfg(any(test, feature = "testing"))]
    after_fence: std::sync::Mutex<Option<crate::testing::AfterFence>>,
    #[cfg(any(test, feature = "testing"))]
    before_turn_commit: std::sync::Mutex<Option<crate::testing::BeforeTurnCommit>>,
}

impl WriterFence {
    /// The fence of a storage that admitted `opened` against `writable` at
    /// open, installing no guards: the migrate runner's and the tests'.
    pub(crate) fn new(writable: VersionRange, opened: FleetFormat) -> Self {
        Self::guarded(
            writable,
            opened,
            TransactionPrelude::inherit(),
            RetryPolicies::default().store,
        )
    }

    /// The fence of a storage whose ordinary transactions begin with
    /// `prelude` and retry contention under `retry`.
    pub(crate) fn guarded(
        writable: VersionRange,
        opened: FleetFormat,
        prelude: TransactionPrelude,
        retry: RetryPolicy,
    ) -> Self {
        Self {
            state: Arc::new(FenceState {
                writable,
                observed: AtomicU32::new(opened.version()),
                opened,
                prelude,
                retry,
                #[cfg(any(test, feature = "testing"))]
                after_fence: std::sync::Mutex::new(None),
                #[cfg(any(test, feature = "testing"))]
                before_turn_commit: std::sync::Mutex::new(None),
            }),
        }
    }

    /// The guard profile ordinary transactions begin with.
    #[cfg(test)]
    pub(crate) fn prelude(&self) -> &TransactionPrelude {
        &self.state.prelude
    }

    /// The last `F` this storage's fences observed: what its writers encode
    /// under before a transaction begins, and what
    /// `FleetFormatStore::fleet_format` answers (§2.3).
    pub(crate) fn fleet(&self) -> FleetFormat {
        self.format_for(self.state.observed.load(Ordering::Acquire))
    }

    /// `[F_prev, F_self]` of the build that opened the storage: the epochs
    /// it writes under, and the one its finalize moves `F` to (`F_self`).
    pub(crate) fn writable(&self) -> VersionRange {
        self.state.writable
    }

    /// The fence of this build as the migrate runner and the operator binary
    /// stand: its own writable range, opened under its newest epoch until a
    /// fence reads the row.
    pub(crate) fn of_this_build() -> Self {
        Self::new(FleetFormat::writable(), FleetFormat::current())
    }

    fn format_for(&self, version: u32) -> FleetFormat {
        if version == self.state.opened.version() {
            self.state.opened
        } else {
            FleetFormat::from_version(version)
        }
    }

    pub(crate) fn observe(&self, version: u32) {
        self.state.observed.fetch_max(version, Ordering::AcqRel);
    }

    /// A fence of its own over the same writable range, standing on `opened`
    /// until a fence reads the row: a test's pin table stays in force while
    /// the recorded epoch is `opened`'s.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn standing_on(&self, opened: FleetFormat) -> Self {
        Self::guarded(
            self.state.writable,
            opened,
            self.state.prelude.clone(),
            self.state.retry,
        )
    }

    /// Install the `AfterFence` seam every transaction of this storage passes.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn install_after_fence(&self, seam: crate::testing::AfterFence) {
        use lash_protocol_types::sync::MutexExt;
        *self.state.after_fence.lock_recover() = Some(seam);
    }

    #[cfg(any(test, feature = "testing"))]
    fn after_fence(&self) -> Option<crate::testing::AfterFence> {
        use lash_protocol_types::sync::MutexExt;
        self.state.after_fence.lock_recover().clone()
    }

    /// Install the `BeforeTurnCommit` seam every runtime commit of this storage
    /// passes once it recorded its turn receipt.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn install_before_turn_commit(&self, seam: crate::testing::BeforeTurnCommit) {
        use lash_protocol_types::sync::MutexExt;
        *self.state.before_turn_commit.lock_recover() = Some(seam);
    }

    #[cfg(any(test, feature = "testing"))]
    fn before_turn_commit(&self) -> Option<crate::testing::BeforeTurnCommit> {
        use lash_protocol_types::sync::MutexExt;
        self.state.before_turn_commit.lock_recover().clone()
    }

    /// The fence's read, its transaction holding the fence lock: the
    /// recorded epoch, `None` when the row is absent.
    async fn read(&self, connection: &mut PgConnection) -> Result<Option<i32>, StoreError> {
        crate::observed_sql::query_scalar::<_, i32>(
            session_sql().fleet_format.select_fleet_format.sql(),
        )
        .fetch_optional(crate::observed_sql::executor(connection))
        .await
        .map_err(|error| self.refused(store_sqlx_error(error)))
    }

    /// `error`, refusing a fence: contention is counted for a test's seam.
    fn refused(&self, error: StoreError) -> StoreError {
        #[cfg(any(test, feature = "testing"))]
        if matches!(error, StoreError::Contended)
            && let Some(seam) = self.after_fence()
        {
            seam.record_contended();
        }
        error
    }

    /// Admit the epoch a fence read, `recorded`: absent or not an epoch
    /// fails closed.
    pub(crate) async fn admit(&self, recorded: Option<i32>) -> Result<FleetFormat, StoreError> {
        let Some(recorded) = recorded else {
            return Err(missing_fence_row("the fleet-format row is absent"));
        };
        let recorded = u32::try_from(recorded).map_err(|_| {
            missing_fence_row(&format!(
                "lash_fleet_format.format_version is not an epoch: {recorded}"
            ))
        })?;
        self.accept(recorded).await
    }

    /// The epoch a fence check read in place of the one it expected: refused
    /// as [`Self::admit`] refuses it, or, inside the writable range, observed
    /// and answered for the next `BEGIN` to expect.
    fn moved(&self, recorded: Option<i32>) -> Result<u32, StoreError> {
        let Some(recorded) = recorded else {
            return Err(missing_fence_row("the fleet-format row is absent"));
        };
        let recorded = u32::try_from(recorded).map_err(|_| {
            missing_fence_row(&format!(
                "lash_fleet_format.format_version is not an epoch: {recorded}"
            ))
        })?;
        FleetFormat::fence(recorded, self.state.writable)?;
        self.observe(recorded);
        Ok(recorded)
    }

    /// Admit the epoch the fence read against the writable range, and record
    /// it as the last observed.
    async fn accept(&self, recorded: u32) -> Result<FleetFormat, StoreError> {
        let fleet = FleetFormat::fence(recorded, self.state.writable)?;
        self.observe(recorded);
        #[cfg(any(test, feature = "testing"))]
        if let Some(seam) = self.after_fence() {
            seam.pass(recorded).await;
        }
        Ok(self.format_for(fleet.version()))
    }
}

/// A fence row that is not there to read, or not a version: the store fails
/// closed (§2.4).
fn missing_fence_row(detail: &str) -> StoreError {
    StoreError::Incompatible {
        refusal: CompatRefusal::MalformedStamp {
            component: ComponentId::POSTGRES.as_str().to_string(),
            detail: format!("writer fence: {detail}"),
            writing_release: None,
        },
    }
}

/// A mutating transaction whose first statement was the writer fence, and the
/// epoch that fence read.
///
/// It derefs to the transaction it guards, so statements run on `&mut **tx`
/// and helpers that take a `&mut Transaction` take `&mut tx`.
pub(crate) struct GuardedTx<'c> {
    tx: Transaction<'c, Postgres>,
    fleet: FleetFormat,
    /// Whether the epoch the fence read is the top of this build's writable
    /// range: the fleet is finalized at this build's epoch.
    finalized: bool,
    /// The transaction's id, once read: what a lost `COMMIT` is reconciled
    /// by.
    xact: Option<XactId>,
    /// Whether the transaction wrote a change of the turn feed.
    #[cfg(any(test, feature = "testing"))]
    wrote_turn_change: bool,
    #[cfg(any(test, feature = "testing"))]
    before_turn_commit: Option<crate::testing::BeforeTurnCommit>,
}

/// A plugin writer range the fleet record does not admit, as the store's
/// typed refusal.
fn plugin_writer_refusal(refusal: CompatRefusal) -> StoreError {
    StoreError::Incompatible { refusal }
}

/// Decode `(plugin, min, max)` rows of the fleet record's writer ranges.
fn plugin_writer_ranges(rows: Vec<(String, i32, i32)>) -> Result<PluginWriterRanges, StoreError> {
    PluginWriterRanges::from_rows(
        rows.into_iter()
            .map(|(plugin, min, max)| (plugin, i64::from(min), i64::from(max))),
    )
    .map_err(plugin_writer_refusal)
}

/// A range's bounds as the columns that record them.
fn plugin_writer_bounds(plugin: &str, range: VersionRange) -> Result<(i32, i32), StoreError> {
    match (i32::try_from(range.min()), i32::try_from(range.max())) {
        (Ok(min), Ok(max)) => Ok((min, max)),
        _ => Err(plugin_writer_refusal(
            CompatRefusal::PluginWriterRangeMalformed {
                plugin: plugin.to_owned(),
                detail: format!("range {range} does not fit the fleet record"),
            },
        )),
    }
}

/// Whether `publication` is admitted against the `recorded` writer ranges
/// as they stand, provisioning none: what a commit planned on an earlier
/// read of them needs. A refusal is the admission's own.
pub(crate) fn admits_without_provisioning(
    recorded: &[(String, i32, i32)],
    publication: &PluginPublication,
) -> Result<bool, StoreError> {
    Ok(plugin_writer_ranges(recorded.to_vec())?
        .admit(publication)
        .map_err(plugin_writer_refusal)?
        .is_empty())
}

/// Every recorded writer range, read on `connection`. The caller holds the
/// fleet-format row's lock, or reads for inspection only.
pub(crate) async fn read_plugin_writers(
    connection: &mut PgConnection,
) -> Result<PluginWriterRanges, StoreError> {
    let rows: Vec<(String, i32, i32)> =
        crate::observed_sql::query_as(session_sql().fleet_plugin_writers.select_all.sql())
            .fetch_all(crate::observed_sql::executor(connection))
            .await
            .map_err(store_sqlx_error)?;
    plugin_writer_ranges(rows)
}

/// `F` moved to another writable epoch between a commit's encoding and its
/// fence (§2.3).
///
/// Internal to the store: the commit rolls back, encodes again under
/// [`FleetMoved::current`] and retries once. It never escapes a store call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FleetMoved {
    pub(crate) current: FleetFormat,
}

impl<'c> GuardedTx<'c> {
    /// `tx`, whose fence admitted `fleet`.
    fn admitted(tx: Transaction<'c, Postgres>, fleet: FleetFormat, fence: &WriterFence) -> Self {
        Self {
            tx,
            fleet,
            finalized: fleet.version() == fence.state.writable.max(),
            xact: None,
            #[cfg(any(test, feature = "testing"))]
            wrote_turn_change: false,
            #[cfg(any(test, feature = "testing"))]
            before_turn_commit: fence.before_turn_commit(),
        }
    }
}

impl GuardedTx<'_> {
    /// The epoch this transaction runs under.
    pub(crate) fn fleet(&self) -> FleetFormat {
        self.fleet
    }

    /// Refuses with [`FleetMoved`] when payloads encoded before `BEGIN` were
    /// encoded under another epoch than the one the fence read.
    pub(crate) fn require_encoded_under(&self, fleet: FleetFormat) -> Result<(), FleetMoved> {
        if fleet.version() == self.fleet.version() {
            Ok(())
        } else {
            Err(FleetMoved {
                current: self.fleet,
            })
        }
    }

    /// Admit the plugin namespaces this transaction publishes against the
    /// fleet record's writer ranges (FIG-4746), before the transaction writes
    /// any of them.
    ///
    /// The fence lock this transaction holds shared is what makes the read
    /// hold: finalize moves a range only under that lock held exclusive, so
    /// no recorded range changes until this transaction ends. A
    /// plugin the record does not name is provisioned here when it publishes
    /// its first format; two transactions that provision it at once agree on
    /// the row the first one committed. A refusal is typed, and dropping the
    /// transaction leaves nothing published.
    pub(crate) async fn admit_plugin_writers(
        &mut self,
        publication: &PluginPublication,
    ) -> Result<(), StoreError> {
        if publication.is_empty() {
            return Ok(());
        }
        let plugins: Vec<String> = publication
            .plugins()
            .into_iter()
            .map(str::to_owned)
            .collect();
        loop {
            let rows: Vec<(String, i32, i32)> = crate::observed_sql::query_as(
                session_sql().fleet_plugin_writers.select_named.sql(),
            )
            .bind(&plugins)
            .fetch_all(crate::observed_sql::executor(&mut *self.tx))
            .await
            .map_err(store_sqlx_error)?;
            let seeded = plugin_writer_ranges(rows)?
                .admit(publication)
                .map_err(plugin_writer_refusal)?;
            if seeded.is_empty() {
                return Ok(());
            }
            let mut provisioned = 0;
            for (plugin, range) in &seeded {
                let (min, max) = plugin_writer_bounds(plugin, *range)?;
                provisioned += crate::observed_sql::query(
                    session_sql().fleet_plugin_writers.insert_if_absent.sql(),
                )
                .bind(plugin)
                .bind(min)
                .bind(max)
                .execute(crate::observed_sql::executor(&mut *self.tx))
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
            }
            // Every row this transaction inserted is the one it was admitted
            // against. A row another transaction committed first is read and
            // admitted again.
            if provisioned == seeded.len() as u64 {
                return Ok(());
            }
        }
    }

    /// Provision a writer range for every plugin of `registrations` the
    /// fleet record does not name, and answer the recorded ranges.
    pub(crate) async fn provision_plugin_writers(
        &mut self,
        registrations: &[PluginWriterRegistration],
    ) -> Result<PluginWriterRanges, StoreError> {
        let recorded = read_plugin_writers(&mut self.tx).await?;
        for (plugin, range) in recorded.provisioned(registrations, self.finalized) {
            let (min, max) = plugin_writer_bounds(&plugin, range)?;
            crate::observed_sql::query(session_sql().fleet_plugin_writers.insert_if_absent.sql())
                .bind(&plugin)
                .bind(min)
                .bind(max)
                .execute(crate::observed_sql::executor(&mut *self.tx))
                .await
                .map_err(store_sqlx_error)?;
        }
        // Read again: a plugin another transaction provisioned first keeps
        // the range that transaction recorded.
        read_plugin_writers(&mut self.tx).await
    }

    /// Pass the `BeforeTurnCommit` seam, right before `COMMIT`, when the
    /// transaction wrote a turn change.
    async fn pass_before_turn_commit(&self) {
        #[cfg(any(test, feature = "testing"))]
        if self.wrote_turn_change
            && let Some(seam) = &self.before_turn_commit
        {
            seam.pass().await;
        }
    }

    /// Write a change of the turn feed, staged: it is sequenced after this
    /// transaction commits.
    pub(crate) async fn stage_turn_change(
        &mut self,
        change: TurnChange,
    ) -> Result<(), sqlx::Error> {
        crate::change_feed::stage_turn_change(&mut self.tx, change).await?;
        #[cfg(any(test, feature = "testing"))]
        {
            self.wrote_turn_change = true;
        }
        Ok(())
    }

    /// A statement of this transaction staged a change of the turn feed.
    pub(crate) fn staged_turn_change(&mut self) {
        #[cfg(any(test, feature = "testing"))]
        {
            self.wrote_turn_change = true;
        }
    }

    /// `COMMIT`.
    pub(crate) async fn commit(self) -> Result<(), sqlx::Error> {
        self.pass_before_turn_commit().await;
        crate::observed_sql::control("COMMIT", self.tx.commit()).await
    }

    /// Record the transaction's id, read with another statement: the
    /// commit then needs no read of its own.
    pub(crate) fn note_xact(&mut self, xact: XactId) {
        self.xact = Some(xact);
    }

    /// `COMMIT`, reconciling a lost answer from the transaction's recorded
    /// outcome ([`commit_reconciled`]). The transaction's id is read first
    /// when it is not yet known.
    pub(crate) async fn commit_reconciled(
        mut self,
        settle: &Settle<'_>,
    ) -> Result<(), Uncommitted> {
        let xact = match self.xact.take() {
            Some(xact) => xact,
            None => XactId::of(&mut self.tx)
                .await
                .map_err(Uncommitted::RolledBack)?,
        };
        self.pass_before_turn_commit().await;
        commit_reconciled(self.tx, &xact, settle).await
    }

    pub(crate) async fn rollback(self) -> Result<(), sqlx::Error> {
        crate::observed_sql::control("ROLLBACK", self.tx.rollback()).await
    }
}

impl<'c> Deref for GuardedTx<'c> {
    type Target = Transaction<'c, Postgres>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl DerefMut for GuardedTx<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}

/// An owned PostgreSQL session detached from any pool. Cancellation closes
/// the session, including its session-scoped migration or sweep locks.
pub(crate) struct DedicatedConnection {
    connection: PgConnection,
}

impl DedicatedConnection {
    pub(crate) fn from_detached(connection: PgConnection) -> Self {
        Self { connection }
    }

    pub(crate) async fn transaction(
        &mut self,
        statement: Option<String>,
    ) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        Transaction::begin(&mut self.connection, statement.map(Into::into)).await
    }

    pub(crate) async fn close(self) -> Result<(), sqlx::Error> {
        sqlx::Connection::close(self.connection).await
    }
}

impl Deref for DedicatedConnection {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        &self.connection
    }
}

impl DerefMut for DedicatedConnection {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.connection
    }
}

/// A savepoint borrows an already acknowledged transaction; it is not a
/// pooled BEGIN and cannot return an untracked session to a pool.
pub(crate) async fn savepoint<'t, 'c>(
    tx: &'t mut Transaction<'c, Postgres>,
) -> Result<Transaction<'t, Postgres>, sqlx::Error> {
    sqlx::Acquire::begin(tx).await
}

/// Where a guarded transaction begins: the pool, or a connection the caller
/// already checked out.
pub(crate) enum GuardedEntry<'c, 'p> {
    /// A checkout of its own, which the transaction owns: it outlives the
    /// borrow of the pool.
    Pool(&'p PgPool),
    /// A connection the caller checked out of `pool`: a `BEGIN` refused
    /// because `F` moved checks out another from the same pool.
    Pooled(PoolConnection<Postgres>, &'p PgPool),
    Connection(&'c mut DedicatedConnection),
}

impl<'p> From<&'p PgPool> for GuardedEntry<'_, 'p> {
    fn from(pool: &'p PgPool) -> Self {
        Self::Pool(pool)
    }
}

impl<'c> From<&'c mut DedicatedConnection> for GuardedEntry<'c, 'c> {
    fn from(connection: &'c mut DedicatedConnection) -> Self {
        Self::Connection(connection)
    }
}

impl<'p> From<(PoolConnection<Postgres>, &'p PgPool)> for GuardedEntry<'_, 'p> {
    fn from((connection, pool): (PoolConnection<Postgres>, &'p PgPool)) -> Self {
        Self::Pooled(connection, pool)
    }
}

/// SQLx records transaction depth only after BEGIN acknowledges. Keep the
/// pooled connection owned until that acknowledgement even if its caller
/// cancels during the fence wait. The completed transaction then drops with
/// a tracked rollback before the pool can hand it to another reader.
pub(crate) async fn begin_pooled(
    connection: PoolConnection<Postgres>,
    statement: String,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let begun =
        tokio::spawn(async move { Transaction::begin(connection, Some(statement.into())).await });
    match begun.await {
        Ok(result) => result,
        // Preserve a direct BEGIN's unwind: the activation's panic budget,
        // rather than the transient store retry, owns a driver panic.
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(sqlx::Error::Protocol(format!(
            "joining pooled BEGIN: {error}"
        ))),
    }
}

/// The fence's check, which a guarded writer's `BEGIN` runs after the fence
/// lock ([`crate::host::TransactionPrelude::fenced_expecting`]).
pub(crate) const WRITER_FENCE_FUNCTION: &str = "lash_writer_fence";

/// The SQLSTATE the fence's check raises when the fleet row does not record
/// the epoch the writer expected; its detail is the recorded epoch, empty
/// when the row is absent.
const FENCE_UNEXPECTED: &str = "LF001";

/// What a fenced `BEGIN` that expected an epoch answers.
enum Fenced<T> {
    /// The row recorded the expected epoch: the transaction is open under it.
    Admitted(T),
    /// The row recorded another epoch, or none; nothing was begun.
    Unexpected(Option<i32>),
}

/// `error` as the fence check's refusal, when it is one: the epoch it read.
fn fence_unexpected(error: &sqlx::Error) -> Option<Option<i32>> {
    let database = error.as_database_error()?;
    if database.code().as_deref() != Some(FENCE_UNEXPECTED) {
        return None;
    }
    let detail = database
        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()?
        .detail()
        .unwrap_or_default();
    if detail.is_empty() {
        Some(None)
    } else {
        // The function renders an INTEGER column: a detail that is not one
        // is no epoch, which the admission refuses closed.
        Some(Some(detail.parse().unwrap_or(-1)))
    }
}

/// Begin on `pool`, or on `connection` when given, with `statement`, a
/// fenced `BEGIN` expecting an epoch.
async fn begin_expecting(
    pool: &PgPool,
    connection: Option<PoolConnection<Postgres>>,
    statement: String,
) -> Result<Fenced<Transaction<'static, Postgres>>, sqlx::Error> {
    let shape = statement.clone();
    let begun = crate::observed_sql::control(&shape, async {
        let connection = match connection {
            Some(connection) => connection,
            None => pool.acquire().await?,
        };
        begin_pooled(connection, statement).await
    })
    .await;
    match begun {
        Ok(tx) => Ok(Fenced::Admitted(tx)),
        Err(error) => match fence_unexpected(&error) {
            Some(recorded) => Ok(Fenced::Unexpected(recorded)),
            None => Err(error),
        },
    }
}

/// A writer's transaction past its fenced `BEGIN`: it holds the fence lock
/// shared, and the epoch the fence admits is the one its next statement
/// reads. A durable commit reads it with its envelope's other reads, in one
/// statement ([`Self::admit`]).
pub(crate) struct Locked<'c> {
    tx: Transaction<'c, Postgres>,
}

impl<'c> Locked<'c> {
    /// The connection the epoch is read on.
    pub(crate) fn connection(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Admit `recorded`, the epoch this transaction read, under `fence`.
    pub(crate) async fn admit(
        self,
        fence: &WriterFence,
        recorded: Option<i32>,
    ) -> Result<GuardedTx<'c>, StoreError> {
        let fleet = fence.admit(recorded).await?;
        Ok(GuardedTx::admitted(self.tx, fleet, fence))
    }
}

/// `BEGIN` with the fence's guard profile, its lock and its check, in one
/// simple query.
///
/// The check expects the epoch the fence last observed, the one the fleet
/// almost always still records. When the row records another, nothing was
/// begun: an epoch outside the writable range refuses as the fence does, an
/// absent row fails closed, and an epoch inside it is observed and the
/// `BEGIN` runs again expecting it, so the transaction proceeds under the
/// epoch its own fence read, as one that read the row with a statement of its
/// own does.
///
/// A dedicated connection begins under the lock and reads the epoch as its
/// first data statement: a refused attempt cannot hand its borrow to the
/// next.
pub(crate) async fn begin_guarded<'c, 'p>(
    entry: impl Into<GuardedEntry<'c, 'p>>,
    fence: &WriterFence,
) -> Result<GuardedTx<'c>, StoreError> {
    let (pool, mut connection) = match entry.into() {
        GuardedEntry::Pool(pool) => (pool, None),
        GuardedEntry::Pooled(connection, pool) => (pool, Some(connection)),
        GuardedEntry::Connection(connection) => {
            let statement = fence.state.prelude.fenced().to_owned();
            let tx = crate::observed_sql::control(&statement.clone(), async {
                connection.transaction(Some(statement)).await
            })
            .await
            .map_err(|error| fence.refused(store_sqlx_error(error)))?;
            let mut locked = Locked { tx };
            let recorded = fence.read(locked.connection()).await?;
            return locked.admit(fence, recorded).await;
        }
    };
    let mut expected = fence.state.observed.load(Ordering::Acquire);
    loop {
        let statement = fence.state.prelude.fenced_expecting(expected);
        match begin_expecting(pool, connection.take(), statement)
            .await
            .map_err(|error| fence.refused(store_sqlx_error(error)))?
        {
            Fenced::Admitted(tx) => {
                let fleet = fence.accept(expected).await?;
                return Ok(GuardedTx::admitted(tx, fleet, fence));
            }
            Fenced::Unexpected(recorded) => expected = fence.moved(recorded)?,
        }
    }
}

/// Durable entry with checkout observed separately from the transaction:
/// `BEGIN` with `prelude`'s guards and the fence lock. The caller reads the
/// epoch ([`Locked::admit`]).
pub(crate) async fn begin_durable(
    pool: &PgPool,
    fence: &WriterFence,
    prelude: &TransactionPrelude,
) -> Result<Locked<'static>, StoreError> {
    let connection = crate::observed_sql::checkout(pool)
        .await
        .map_err(store_sqlx_error)?;
    crate::observed_sql::transaction_started();
    let begin = begin_pooled(connection, prelude.fenced().to_owned());
    // Observed under its own text: the fence lock's wait counts as a lock
    // wait.
    let tx = crate::observed_sql::control(prelude.fenced(), begin)
        .await
        .map_err(|error| fence.refused(store_sqlx_error(error)))?;
    Ok(Locked { tx })
}

/// A schema migration's transaction entry: `BEGIN`, then the fence, on the
/// connection that holds the schema advisory lock.
///
/// The migrations ledger is fenced like every other mutation, so a stale
/// build's `migrate` refuses once a newer release has finalized. A catalog
/// that cannot record `F` yet has nothing to fence on: a bootstrap runs
/// before `lash_fleet_format` exists, and the row stays unrecorded until the
/// first open. No release can have finalized either, so the step proceeds
/// under this build's own epoch.
pub(crate) async fn begin_migration<'c>(
    connection: &'c mut DedicatedConnection,
    fence: &WriterFence,
) -> Result<GuardedTx<'c>, StoreError> {
    let mut tx = crate::observed_sql::control("BEGIN", connection.transaction(None))
        .await
        .map_err(store_sqlx_error)?;
    let recordable: bool =
        crate::observed_sql::query_scalar(session_sql().fleet_format.select_is_present.sql())
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(store_sqlx_error)?;
    let recorded = if recordable {
        crate::observed_sql::query(
            crate::connection_sql::connection_sql()
                .lock_xact_fleet_fence_shared
                .sql(),
        )
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(|error| fence.refused(store_sqlx_error(error)))?;
        fence.read(&mut tx).await?
    } else {
        None
    };
    // A catalog that records no `F` is the one migration seeds: unlike a
    // writer's fence, an absent row is not a refusal here.
    let fleet = match recorded {
        Some(recorded) => fence.admit(Some(recorded)).await?,
        None => FleetFormat::current(),
    };
    Ok(GuardedTx::admitted(tx, fleet, fence))
}

/// The fleet-format row under the fence lock held exclusive: a move of
/// `F`'s side of the fence (§2.2), which tests stand up for a newer fleet.
///
/// `BEGIN`, then the fence lock exclusive as the transaction's only lock,
/// then the row read. It waits behind every writer holding the lock shared,
/// and every writer that fences after it waits until it commits and then
/// reads what it wrote. The recorded epoch is admitted against the fence's
/// writable range like any writer's: a build that a newer release fenced out
/// cannot move it.
#[cfg(any(test, feature = "testing"))]
pub(crate) struct FleetRowTx {
    tx: Transaction<'static, Postgres>,
    /// The epoch the row records.
    pub(crate) recorded: u32,
}

#[cfg(any(test, feature = "testing"))]
impl FleetRowTx {
    pub(crate) fn connection(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub(crate) async fn commit(self) -> Result<(), StoreError> {
        crate::observed_sql::control("COMMIT", self.tx.commit())
            .await
            .map_err(store_sqlx_error)
    }
}

/// Lock the fleet-format row for an update ([`FleetRowTx`]).
#[cfg(any(test, feature = "testing"))]
pub(crate) async fn begin_fleet_row(
    pool: &PgPool,
    fence: &WriterFence,
) -> Result<FleetRowTx, StoreError> {
    let mut tx = crate::host::begin_unfenced(pool)
        .await
        .map_err(store_sqlx_error)?;
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_fleet_fence
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)?;
    let row: Option<i32> =
        crate::observed_sql::query_scalar(session_sql().fleet_format.select_fleet_format.sql())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(store_sqlx_error)?;
    let Some(recorded) = row else {
        return Err(missing_fence_row("the fleet-format row is absent"));
    };
    let recorded = u32::try_from(recorded).map_err(|_| {
        missing_fence_row(&format!(
            "lash_fleet_format.format_version is not an epoch: {recorded}"
        ))
    })?;
    FleetFormat::fence(recorded, fence.state.writable)?;
    fence.observe(recorded);
    Ok(FleetRowTx { tx, recorded })
}

/// The body [`guarded`] runs inside each attempt's transaction.
pub(crate) type GuardedBody<'t, T> =
    Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 't>>;

/// `body` in a guarded transaction, committed, and retried per §2.4 under
/// the storage's store retry policy ([`RetryPolicy::run`]): an attempt that
/// meets contention anywhere, the fence included, rolls back and runs again
/// from a fresh `BEGIN`, so the retry reads `F` afresh. A `COMMIT` whose
/// answer was lost is reconciled, never run again blind. After the policy's
/// attempts the contention is the caller's, and the whole loop, pauses
/// included, runs within the ordinary profile's operation deadline.
///
/// The transaction is typed at `'a`, the lifetime of what `body` borrows, so
/// the body's future may hold both.
pub(crate) async fn guarded<'a, T, F>(
    pool: &PgPool,
    fence: &WriterFence,
    body: F,
) -> Result<T, StoreError>
where
    T: Send,
    F: for<'t> Fn(&'t mut GuardedTx<'a>) -> GuardedBody<'t, T> + Send + Sync,
{
    let prelude = &fence.state.prelude;
    let retry = &fence.state.retry;
    let deadline = prelude
        .deadline()
        .map(|deadline| tokio::time::Instant::now() + deadline);
    let body = &body;
    let attempts = retry.run(deadline, Attempt::is_aborted, || async move {
        let mut tx: GuardedTx<'a> = begin_guarded(pool, fence).await.map_err(attempt)?;
        let value = match body(&mut tx).await {
            Ok(value) => value,
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(attempt(error));
            }
        };
        let settle = Settle {
            pool,
            retry,
            deadline,
            #[cfg(any(test, feature = "testing"))]
            fault: None,
        };
        match tx.commit_reconciled(&settle).await {
            Ok(()) => Ok(value),
            Err(Uncommitted::RolledBack(error)) => Err(attempt(store_sqlx_error(error))),
            Err(Uncommitted::Lost(error)) => Err(Attempt::Aborted(StoreError::StorageFailure {
                code: crate::support::storage_code(&error),
                backend: crate::POSTGRES_BACKEND,
                message: format!("the connection was lost at COMMIT, which rolled back: {error}"),
            })),
            Err(Uncommitted::Unknown { message, code }) => {
                Err(Attempt::Failed(StoreError::StorageFailure {
                    code,
                    backend: crate::POSTGRES_BACKEND,
                    message,
                }))
            }
        }
    });
    prelude
        .bounded(attempts)
        .await
        .unwrap_or_else(|deadline| Err(Attempt::Failed(operation_deadline_exceeded(deadline))))
        .map_err(Attempt::into_error)
}

/// `attempt`, one registry write from its guarded `BEGIN` through its
/// `COMMIT`, run again while the database rolls it back for contention,
/// under the storage's store retry policy and within the ordinary profile's
/// operation deadline, as [`guarded`] runs its body (FIG-5855). The caller
/// fixes the write's inputs before the first attempt, so every attempt is
/// the identical write, and an attempt rolls a failed transaction back
/// before it answers. A `COMMIT` whose answer was lost is not contention:
/// it is never run again. After the policy's attempts the contention is the
/// caller's.
pub(crate) async fn retry_contended<T, F, Fut>(
    fence: &WriterFence,
    attempt: F,
) -> Result<T, lash_execution_contracts::plugin_error::PluginError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, lash_execution_contracts::plugin_error::PluginError>>,
{
    let deadline = fence
        .state
        .prelude
        .deadline()
        .map(|deadline| tokio::time::Instant::now() + deadline);
    fence
        .state
        .retry
        .run(
            deadline,
            |error: &lash_execution_contracts::plugin_error::PluginError| {
                matches!(
                    error,
                    lash_execution_contracts::plugin_error::PluginError::StoreUnavailable {
                        fault: lash_core_store::store::StoreFault::Contended,
                    }
                )
            },
            attempt,
        )
        .await
}

/// A store refusal as an attempt's failure: contention rolled the attempt
/// back, and anything else is the operation's answer.
fn attempt(error: StoreError) -> Attempt<StoreError> {
    if matches!(error, StoreError::Contended) {
        Attempt::Aborted(error)
    } else {
        Attempt::Failed(error)
    }
}

/// A store operation that did not finish within its whole-operation
/// deadline: whether its last attempt committed is unknown.
pub(crate) fn operation_deadline_exceeded(deadline: std::time::Duration) -> StoreError {
    StoreError::StorageFailure {
        code: None,
        backend: crate::POSTGRES_BACKEND,
        message: format!(
            "operation did not finish within its {} ms deadline",
            deadline.as_millis()
        ),
    }
}

#[cfg(test)]
#[path = "guarded_tx_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plugin_writer_tests.rs"]
mod plugin_writer_tests;
