//! The one retry owner of a replayable PostgreSQL transaction (FIG-5242).
//!
//! A replayable transaction is a complete database write whose inputs are
//! fixed before it begins: the same payload under the same epoch, with no
//! tool or model call inside it. [`RetryPolicy::run`] runs it again from a
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

impl RetryPolicy {
    /// Run `attempt` until it answers anything but a failure `aborted`
    /// accepts (one the database is known to have rolled back), at most
    /// `attempts` times, pausing before each retry.
    ///
    /// `attempt` runs one complete transaction from its `BEGIN`, and
    /// returns only once a failed transaction is rolled back or dropped:
    /// no pause holds one. A pause that would end at or past `deadline`, the
    /// whole operation's, is not taken: the last failure is the answer.
    /// Never put a tool or model call, or another retry loop, inside
    /// `attempt`.
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
        let mut retry = 0;
        loop {
            match attempt().await {
                Err(error) if aborted(&error) && retry + 1 < self.attempts => {
                    let Some(pause) = self.pause_within(retry, deadline) else {
                        return Err(error);
                    };
                    tokio::time::sleep(pause).await;
                    retry += 1;
                }
                outcome => return outcome,
            }
        }
    }

    /// The pause before retry `retry`, or `None` when it would end at or
    /// past `deadline`.
    fn pause_within(&self, retry: u32, deadline: Option<Instant>) -> Option<Duration> {
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
    Unknown(String),
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
        sqlx::query_scalar(connection_sql().select_xact_id.sql())
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
                return Err(Uncommitted::Unknown(format!(
                    "the server no longer records transaction {}",
                    xact.0
                )));
            }
            Err(error) => last = format!("its outcome could not be read: {error}"),
        }
    }
    Err(Uncommitted::Unknown(format!(
        "whether transaction {} committed is unknown: {last}",
        xact.0
    )))
}

/// Read `xact`'s recorded outcome on a connection of the pool the commit
/// ran on.
async fn recorded_status(
    xact: &XactId,
    settle: &Settle<'_>,
) -> Result<Option<String>, sqlx::Error> {
    let status = sqlx::query_scalar::<_, Option<String>>(connection_sql().select_xact_status.sql())
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
