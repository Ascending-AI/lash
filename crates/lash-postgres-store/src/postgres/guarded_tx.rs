//! The guarded transaction entry: the PostgreSQL writer fence (ADR 0115
//! §2.2–2.4).
//!
//! Every mutating transaction this store runs begins here, and nowhere else
//! (`scripts/check-guarded-transactions.py` holds the entry total). The fence
//! is the transaction's first statement, ahead of the session advisory lock
//! and every row lock:
//!
//! ```sql
//! SELECT format_version FROM lash_fleet_format WHERE singleton FOR SHARE;
//! ```
//!
//! Finalize reads the same row `FOR UPDATE` and then moves it, in one
//! transaction that takes no other row lock. So a writer holding the share
//! lock makes finalize wait and commits under the old `F`, and a writer whose
//! share lock waits behind finalize re-reads the row once finalize commits and
//! is fenced before it writes anything. The two never deadlock: a writer holds
//! no other lock while it waits on `F`, and finalize takes no lock a writer
//! holds.
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

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use lash_core_execution::compat::{CompatRefusal, ComponentId, VersionRange};
use lash_core_execution::store::plugin_writers::{
    PluginPublication, PluginWriterRanges, PluginWriterRegistration,
};
use lash_core_execution::{FleetFormat, StoreError};
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Transaction};

use crate::session_sql::session_sql;
use crate::store_sqlx_error;

/// How many times [`guarded`] runs a transaction that meets contention before
/// it surfaces [`StoreError::Contended`] to the caller's own retry.
const CONTENDED_ATTEMPTS: u32 = 4;

/// The first pause between contended attempts; each later one doubles it.
const CONTENDED_BACKOFF: Duration = Duration::from_millis(5);

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
    #[cfg(any(test, feature = "testing"))]
    after_fence: std::sync::Mutex<Option<crate::testing::AfterFence>>,
}

impl WriterFence {
    /// The fence of a storage that admitted `opened` against `writable` at
    /// open.
    pub(crate) fn new(writable: VersionRange, opened: FleetFormat) -> Self {
        Self {
            state: Arc::new(FenceState {
                writable,
                observed: AtomicU32::new(opened.version()),
                opened,
                #[cfg(any(test, feature = "testing"))]
                after_fence: std::sync::Mutex::new(None),
            }),
        }
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
        Self::new(self.state.writable, opened)
    }

    /// Install the `AfterFence` seam every transaction of this storage passes.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn install_after_fence(&self, seam: crate::testing::AfterFence) {
        use lash_sansio::sync::MutexExt;
        *self.state.after_fence.lock_recover() = Some(seam);
    }

    #[cfg(any(test, feature = "testing"))]
    fn after_fence(&self) -> Option<crate::testing::AfterFence> {
        use lash_sansio::sync::MutexExt;
        self.state.after_fence.lock_recover().clone()
    }

    /// Run the fence as `tx`'s first statement and admit the epoch it reads.
    async fn admit(&self, tx: &mut Transaction<'_, Postgres>) -> Result<FleetFormat, StoreError> {
        match self.read(tx).await? {
            Some(recorded) => self.accept(recorded).await,
            None => Err(missing_fence_row("the fleet-format row is absent")),
        }
    }

    /// The fence statement: the recorded epoch under a share lock, `None`
    /// when the row is absent.
    async fn read(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<u32>, StoreError> {
        let read = sqlx::query_scalar::<_, i32>(session_sql().fleet_format.select_for_fence.sql())
            .fetch_optional(&mut **tx)
            .await;
        match read {
            Ok(Some(recorded)) => u32::try_from(recorded).map(Some).map_err(|_| {
                missing_fence_row(&format!(
                    "lash_fleet_format.format_version is not an epoch: {recorded}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(error) => {
                let error = store_sqlx_error(error);
                #[cfg(any(test, feature = "testing"))]
                if matches!(error, StoreError::Contended)
                    && let Some(seam) = self.after_fence()
                {
                    seam.record_contended();
                }
                Err(error)
            }
        }
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

/// Every recorded writer range, read on `connection`. The caller holds the
/// fleet-format row's lock, or reads for inspection only.
pub(crate) async fn read_plugin_writers(
    connection: &mut PgConnection,
) -> Result<PluginWriterRanges, StoreError> {
    let rows: Vec<(String, i32, i32)> =
        sqlx::query_as(session_sql().fleet_plugin_writers.select_all.sql())
            .fetch_all(connection)
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
    /// The share lock the fence took on the fleet-format row is what makes
    /// the read hold: finalize moves a range only under that row's update
    /// lock, so no recorded range changes until this transaction ends. A
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
            let rows: Vec<(String, i32, i32)> =
                sqlx::query_as(session_sql().fleet_plugin_writers.select_named.sql())
                    .bind(&plugins)
                    .fetch_all(&mut *self.tx)
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
                provisioned +=
                    sqlx::query(session_sql().fleet_plugin_writers.insert_if_absent.sql())
                        .bind(plugin)
                        .bind(min)
                        .bind(max)
                        .execute(&mut *self.tx)
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
            sqlx::query(session_sql().fleet_plugin_writers.insert_if_absent.sql())
                .bind(&plugin)
                .bind(min)
                .bind(max)
                .execute(&mut *self.tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        // Read again: a plugin another transaction provisioned first keeps
        // the range that transaction recorded.
        read_plugin_writers(&mut self.tx).await
    }

    pub(crate) async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }

    pub(crate) async fn rollback(self) -> Result<(), sqlx::Error> {
        self.tx.rollback().await
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

/// `BEGIN`, then the fence as the transaction's first statement.
///
/// `acquire` is the pool, or a connection the caller already checked out.
pub(crate) async fn begin_guarded<'c, A>(
    acquire: A,
    fence: &WriterFence,
) -> Result<GuardedTx<'c>, StoreError>
where
    A: Acquire<'c, Database = Postgres>,
{
    let mut tx = acquire.begin().await.map_err(store_sqlx_error)?;
    let fleet = fence.admit(&mut tx).await?;
    Ok(GuardedTx {
        tx,
        fleet,
        finalized: fleet.version() == fence.state.writable.max(),
    })
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
    connection: &'c mut PgConnection,
    fence: &WriterFence,
) -> Result<GuardedTx<'c>, StoreError> {
    let mut tx = Acquire::begin(connection).await.map_err(store_sqlx_error)?;
    let recordable: bool = sqlx::query_scalar(session_sql().fleet_format.select_is_present.sql())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let recorded = if recordable {
        fence.read(&mut tx).await?
    } else {
        None
    };
    let fleet = match recorded {
        Some(recorded) => fence.accept(recorded).await?,
        None => FleetFormat::current(),
    };
    Ok(GuardedTx {
        tx,
        fleet,
        finalized: fleet.version() == fence.state.writable.max(),
    })
}

/// The fleet-format row locked for an update: a move of `F`'s side of the
/// fence (§2.2), which tests stand up for a newer fleet.
///
/// `BEGIN`, then the row read `FOR UPDATE` as the transaction's only lock.
/// It waits behind every writer holding the row `FOR SHARE`, and every
/// writer that fences after it waits until it commits and then reads what it
/// wrote. The recorded epoch is admitted against the fence's writable range
/// like any writer's: a build that a newer release fenced out cannot move it.
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
        self.tx.commit().await.map_err(store_sqlx_error)
    }
}

/// Lock the fleet-format row for an update ([`FleetRowTx`]).
#[cfg(any(test, feature = "testing"))]
pub(crate) async fn begin_fleet_row(
    pool: &PgPool,
    fence: &WriterFence,
) -> Result<FleetRowTx, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    let row: Option<i32> = sqlx::query_scalar(session_sql().fleet_format.select_for_update.sql())
        .fetch_optional(&mut *tx)
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

/// `body` in a guarded transaction, committed, and retried per §2.4: an
/// attempt that meets contention anywhere, the fence included, rolls back and
/// runs again from a fresh `BEGIN`, so the retry reads `F` afresh. After
/// [`CONTENDED_ATTEMPTS`] the contention is the caller's.
///
/// The transaction is typed at `'a`, the lifetime of what `body` borrows, so
/// the body's future may hold both.
pub(crate) async fn guarded<'a, T, F>(
    pool: &PgPool,
    fence: &WriterFence,
    mut body: F,
) -> Result<T, StoreError>
where
    F: for<'t> FnMut(&'t mut GuardedTx<'a>) -> GuardedBody<'t, T>,
{
    let mut backoff = CONTENDED_BACKOFF;
    let mut attempt = 1;
    loop {
        let outcome = match begin_guarded(pool, fence).await {
            Ok(tx) => {
                let mut tx: GuardedTx<'a> = tx;
                match body(&mut tx).await {
                    Ok(value) => tx.commit().await.map_err(store_sqlx_error).map(|()| value),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match outcome {
            Err(StoreError::Contended) if attempt < CONTENDED_ATTEMPTS => {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                attempt += 1;
            }
            outcome => return outcome,
        }
    }
}

#[cfg(test)]
#[path = "guarded_tx_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plugin_writer_tests.rs"]
mod plugin_writer_tests;
