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

    fn format_for(&self, version: u32) -> FleetFormat {
        if version == self.state.opened.version() {
            self.state.opened
        } else {
            FleetFormat::from_version(version)
        }
    }

    fn observe(&self, version: u32) {
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
    Ok(GuardedTx { tx, fleet })
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
) -> Result<GuardedTx<'c>, StoreError> {
    let fence = WriterFence::new(FleetFormat::writable(), FleetFormat::current());
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
    Ok(GuardedTx { tx, fleet })
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
