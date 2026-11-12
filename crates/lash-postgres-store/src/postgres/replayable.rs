//! The one retry owner of a replayable PostgreSQL transaction (FIG-5242).
//!
//! A replayable transaction is a complete database write whose inputs are
//! fixed before it begins: the same payload under the same epoch, with no
//! tool or model call inside it. A clock an attempt reads itself is the one
//! exception; only the committed attempt's timestamp persists.
//! [`RetryPolicy::run`] runs it again from a
//! fresh `BEGIN` when the database rolled it back for contention, so every
//! attempt passes the writer fence and its ownership fence afresh, and an
//! attempt fenced out stops the loop. Each pause comes after the failed
//! transaction is gone, and no pause runs past the operation's deadline.
//! One operation has one retry owner: nothing here wraps another.
//!
//! A `COMMIT` whose answer never came back may have committed. It is not a
//! known rollback, so it is never run again on that error alone:
//! [`commit_reconciled`] reads the transaction's recorded outcome by its id
//! (`pg_xact_status`) on a fresh connection. A commit that landed answers
//! once; one that rolled back is a known rollback, and only then runs again.

use std::future::Future;
use std::time::Duration;

use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::time::Instant;

use crate::connection_sql::connection_sql;
use crate::host::RetryPolicy;

tokio::task_local! {
    /// What the running attempt has done toward its seal (FIG-7700).
    static ATTEMPT: std::cell::Cell<SealRecord>;
}

/// One attempt's seal: whether it has taken its artifact locks, and
/// whether its last contention was a wait that expired after them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SealRecord {
    sealed: bool,
    expired_after_seal: bool,
}

/// The message prefix the schema raises when a lock wait inside one of
/// its sealing functions expires after that function's seal
/// ([`crate::advisory_lock`]).
pub(crate) const SEALED_WAIT_EXPIRED: &str = "a lock wait after the artifact locks expired";

/// The refusal the schema's advisory helper raises for an advisory lock a
/// sealed transaction did not plan ([`crate::advisory_lock`]).
pub(crate) const UNPLANNED_ADVISORY_REFUSED: &str =
    "an advisory lock this transaction needs after its artifact locks is held elsewhere";

/// Record that the running attempt's sealing statement returned: its
/// artifact locks are held and its `lock_timeout` is the sealed bound.
/// Called by each Rust site that runs a sealing statement, after it
/// succeeds; outside a retry loop it records nothing.
pub(crate) fn note_sealed() {
    let _ = ATTEMPT.try_with(|record| {
        record.set(SealRecord {
            sealed: true,
            ..record.get()
        });
    });
}

/// Record one contention failure (`code`, `message`) of the running
/// attempt: whether it is a lock wait that expired after the attempt's
/// seal. That is the schema's own after-seal expiry (a wait inside a
/// sealing function), or a plain lock timeout once a sealing statement has
/// returned. The advisory helper's refusal of an unplanned lock, deadlock,
/// serialization failure and every lock timeout before the seal are not.
pub(crate) fn note_contention(code: &str, message: &str) {
    let _ = ATTEMPT.try_with(|record| {
        let current = record.get();
        let expired_after_seal = code == "55P03"
            && message != UNPLANNED_ADVISORY_REFUSED
            && (message.starts_with(SEALED_WAIT_EXPIRED)
                || (current.sealed && message == LOCK_TIMEOUT_MESSAGE));
        record.set(SealRecord {
            expired_after_seal,
            ..current
        });
    });
}

/// PostgreSQL's message for a statement cancelled by `lock_timeout`.
const LOCK_TIMEOUT_MESSAGE: &str = "canceling statement due to lock timeout";

impl RetryPolicy {
    /// Run `attempt` until it answers anything but a failure `aborted`
    /// accepts (one the database is known to have rolled back), pausing
    /// before each retry (FIG-5242, FIG-7700).
    ///
    /// - Per wait: after an attempt takes its artifact locks, each later
    ///   lock wait lasts at most the sealed `lock_timeout`
    ///   (`guards.sealed_lock_timeout_ms`), then the attempt rolls back.
    /// - Attempts: an attempt that rolled back because such a wait expired
    ///   does not spend one of the policy's `attempts`. Every other
    ///   contention does: deadlock, serialization failure, a lock timeout
    ///   before the seal, and the advisory helper's refusal of an advisory
    ///   lock the attempt did not plan (a planning gap, which waiting does
    ///   not cure, so it stays bounded by the count).
    /// - Pauses: the policy's doubling backoff and jitter, capped at its
    ///   longest pause, grow with every retry, spent or not.
    /// - Deadline: `deadline`, the whole operation's, bounds the loop. A
    ///   pause that would end at or past it is not taken, and the last
    ///   failure, the existing contention answer, is returned. Without a
    ///   deadline every contention spends an attempt.
    ///
    /// `attempt` runs one complete transaction from its `BEGIN`, and
    /// returns only once a failed transaction is rolled back or dropped:
    /// no pause holds one. Each attempt runs with a fresh seal record,
    /// which its sealing statements ([`note_sealed`]) and the contention
    /// classification ([`note_contention`]) fill. Never put a tool or model
    /// call, or another retry loop, inside `attempt`.
    ///
    /// # Errors
    ///
    /// The last attempt's failure.
    pub async fn run<T, E, Fut>(
        &self,
        deadline: Option<Instant>,
        aborted: impl Fn(&E) -> bool,
        mut attempt: impl FnMut() -> Fut,
    ) -> Result<T, E>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        let mut spent = 0;
        let mut retry = 0;
        loop {
            let running = attempt();
            // Boxed: the scope holds the whole attempt, the largest part of
            // every caller's future.
            let (outcome, record) = Box::pin(ATTEMPT.scope(
                std::cell::Cell::new(SealRecord::default()),
                async move {
                    let outcome = running.await;
                    (outcome, ATTEMPT.with(std::cell::Cell::get))
                },
            ))
            .await;
            match outcome {
                Err(error) if aborted(&error) => {
                    let free = record.expired_after_seal && deadline.is_some();
                    if !free && spent + 1 >= self.attempts {
                        return Err(error);
                    }
                    let Some(pause) = self.pause_within(retry, deadline) else {
                        return Err(error);
                    };
                    tokio::time::sleep(pause).await;
                    retry = retry.saturating_add(1);
                    if !free {
                        spent += 1;
                    }
                }
                outcome => return outcome,
            }
        }
    }

    /// The pause before retry `retry`, or `None` when it would end at or
    /// past `deadline`.
    pub(crate) fn pause_within(&self, retry: u32, deadline: Option<Instant>) -> Option<Duration> {
        let pause = self.pause(retry);
        match deadline {
            Some(deadline) if Instant::now() + pause >= deadline => None,
            _ => Some(pause),
        }
    }
}

/// One attempt's failure: one the database rolled back and a retry may
/// clear, or the operation's answer.
#[derive(Debug)]
pub(crate) enum Attempt<E> {
    Aborted(E),
    Failed(E),
}

impl<E> Attempt<E> {
    pub(crate) fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted(_))
    }

    pub(crate) fn into_error(self) -> E {
        match self {
            Self::Aborted(error) | Self::Failed(error) => error,
        }
    }
}

/// How a `COMMIT` that did not succeed ended.
#[derive(Debug)]
pub(crate) enum Uncommitted {
    /// The transaction failed before `COMMIT` was sent, or the server
    /// answered `COMMIT` with this error: it rolled back.
    RolledBack(sqlx::Error),
    /// The connection was lost at `COMMIT`, and the transaction's recorded
    /// outcome says it rolled back.
    Lost(sqlx::Error),
    /// Whether it committed could not be learned.
    Unknown {
        message: String,
        code: Option<lash_core_store::store::StorageCode>,
    },
}

/// A transaction's id, by which a `COMMIT` whose answer was lost is
/// reconciled.
#[derive(Clone, Debug)]
pub(crate) struct XactId(String);

impl XactId {
    pub(crate) fn new(id: String) -> Self {
        Self(id)
    }

    /// The id of the transaction open on `connection`, assigned now if it
    /// has none yet.
    pub(crate) async fn of(connection: &mut PgConnection) -> Result<Self, sqlx::Error> {
        crate::observed_sql::query_scalar(connection_sql().select_xact_id.sql())
            .fetch_one(crate::observed_sql::executor(connection))
            .await
            .map(Self)
    }
}

/// What a `COMMIT` is reconciled against: the pool its outcome is read
/// from, the pauses between reads while the server still finishes it, and
/// the operation's deadline.
pub(crate) struct Settle<'a> {
    pub(crate) pool: &'a PgPool,
    pub(crate) retry: &'a RetryPolicy,
    pub(crate) deadline: Option<Instant>,
    /// A connection a test loses at this `COMMIT`.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fault: Option<&'a crate::testing::CommitFault>,
}

/// `COMMIT` `tx`, transaction `xact`, reconciling a lost answer from the
/// transaction's recorded outcome.
pub(crate) async fn commit_reconciled(
    tx: Transaction<'_, Postgres>,
    xact: &XactId,
    settle: &Settle<'_>,
) -> Result<(), Uncommitted> {
    #[cfg(any(test, feature = "testing"))]
    let committed = match settle.fault {
        Some(fault) => crate::observed_sql::control("COMMIT", fault.commit(tx)).await,
        None => crate::observed_sql::control("COMMIT", tx.commit()).await,
    };
    #[cfg(not(any(test, feature = "testing")))]
    let committed = crate::observed_sql::control("COMMIT", tx.commit()).await;
    match committed {
        Ok(()) => Ok(()),
        Err(error) if answered(&error) => Err(Uncommitted::RolledBack(error)),
        Err(error) => reconcile(xact, error, settle).await,
    }
}

/// Whether the server answered a failed `COMMIT` itself, so the
/// transaction rolled back: any error but a broken connection, the session
/// ending under it (`08`, `57`), or a server fault (`58`, `XX`).
fn answered(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    database
        .code()
        .and_then(|code| code.get(..2).map(str::to_owned))
        .is_some_and(|class| !matches!(class.as_str(), "08" | "57" | "58" | "XX"))
}

/// Read `xact`'s recorded outcome until it is decided: committed, or
/// rolled back after `lost`. A transaction still in progress is the server
/// finishing the lost session: read again after a pause, never past the
/// deadline nor more often than the policy's attempts.
async fn reconcile(
    xact: &XactId,
    lost: sqlx::Error,
    settle: &Settle<'_>,
) -> Result<(), Uncommitted> {
    let mut code = crate::support::storage_code(&lost);
    let mut last = format!("the connection was lost at COMMIT: {lost}");
    for read in 0..settle.retry.attempts {
        if read > 0 {
            let Some(pause) = settle.retry.pause_within(read - 1, settle.deadline) else {
                break;
            };
            tokio::time::sleep(pause).await;
        }
        let status = recorded_status(xact, settle);
        let status = match settle.deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, status).await {
                Ok(status) => status,
                Err(_) => break,
            },
            None => status.await,
        };
        match status.as_ref().map(Option::as_deref) {
            Ok(Some("committed")) => return Ok(()),
            Ok(Some("aborted")) => return Err(Uncommitted::Lost(lost)),
            Ok(Some(other)) => last = format!("transaction {} is {other}", xact.0),
            Ok(None) => {
                return Err(Uncommitted::Unknown {
                    message: format!("the server no longer records transaction {}", xact.0),
                    code,
                });
            }
            Err(error) => {
                code = crate::support::storage_code(error);
                last = format!("its outcome could not be read: {error}");
            }
        }
    }
    Err(Uncommitted::Unknown {
        message: format!(
            "whether transaction {} committed is unknown: {last}",
            xact.0
        ),
        code,
    })
}

/// Read `xact`'s recorded outcome on a connection of the pool the commit
/// ran on.
async fn recorded_status(
    xact: &XactId,
    settle: &Settle<'_>,
) -> Result<Option<String>, sqlx::Error> {
    let status = crate::observed_sql::query_scalar::<_, Option<String>>(
        connection_sql().select_xact_status.sql(),
    )
    .bind(&xact.0);
    #[cfg(any(test, feature = "testing"))]
    if let Some(fault) = settle.fault
        && let Some(lost) = fault.outcome_connection(settle.pool).await
    {
        return status
            .fetch_one(crate::observed_sql::executor(&mut *lost?))
            .await;
    }
    status
        .fetch_one(crate::observed_sql::executor(settle.pool))
        .await
}
