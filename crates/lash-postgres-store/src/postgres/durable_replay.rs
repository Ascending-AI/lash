//! A durable commit's one retry owner (FIG-5242):
//! [`DurableStore::commit`](lash_durable::DurableStore::commit) and
//! [`DurableStore::commit_mail`](lash_durable::DurableStore::commit_mail) run their immutable write under the
//! label's [`RetryPolicy`](crate::host::RetryPolicy), each attempt from a
//! fresh `BEGIN` through the writer fence and, for an owner, its epoch
//! fence, within the role's one operation deadline. A `COMMIT` whose answer
//! was lost is reconciled from the transaction's recorded outcome
//! ([`crate::replayable`]), never run again blind.

use lash_durable::{
    ActorCommit, ActorTx, CommitCapacity, CommitLabel, DurableError, DurableInstant, MailCommit,
    MailTx, StoreFailure, StoreFailureKind,
};

use super::{
    PostgresDurableStore, SQL, Tx, apply_mail, apply_owner, group_members, sqlx_failure,
    store_failure,
};
use crate::guarded_tx::begin_durable;
use crate::replayable::{Attempt, Settle, Uncommitted, XactId};

/// A refusal as a replayable commit attempt's failure: contention rolled
/// the attempt back, and anything else (an ownership fence included) is the
/// commit's answer.
fn attempt(error: DurableError) -> Attempt<DurableError> {
    if matches!(
        &error,
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Contended,
            ..
        })
    ) {
        Attempt::Aborted(error)
    } else {
        Attempt::Failed(error)
    }
}

impl PostgresDurableStore {
    /// [`DurableStore::commit`](lash_durable::DurableStore::commit): the owner's write, applied after its
    /// ownership fence, run again while the database rolls it back for
    /// contention.
    pub(super) async fn commit_owner(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        let deadline = self.deadline(label.capacity());
        self.within(label.capacity(), async {
            crate::observed_sql::measure(&self.observer, label, group_members(&tx), async {
                if tx.ack().is_some_and(|through| through > tx.seen()) {
                    return Err(DurableError::AckBeyondRead {
                        actor: tx.actor().clone(),
                    });
                }
                let tx = &tx;
                self.replay(label, deadline, || async move {
                    let (mut guarded, now, fence) = self.open_replayable(label, Some(tx)).await?;
                    let outcome = Box::pin(apply_owner(
                        &mut guarded,
                        tx,
                        fence,
                        now,
                        self.fence.fleet(),
                        self.pools.maintenance.checkpoint_ref_chunk as usize,
                    ))
                    .await;
                    self.settle(label, guarded, outcome, deadline).await
                })
                .await
            })
            .await
        })
        .await
    }

    /// [`DurableStore::commit_mail`](lash_durable::DurableStore::commit_mail): the mailbox write, run again while the
    /// database rolls it back for contention.
    pub(super) async fn commit_mailbox(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let deadline = self.deadline(label.capacity());
        self.within(label.capacity(), async {
            crate::observed_sql::measure(&self.observer, label, 0, async {
                let tx = &tx;
                self.replay(label, deadline, || async move {
                    let (mut guarded, now, _) = self.open_replayable(label, None).await?;
                    let outcome =
                        Box::pin(apply_mail(&mut guarded, tx, now, self.fence.fleet())).await;
                    self.settle(label, guarded, outcome, deadline).await
                })
                .await
            })
            .await
        })
        .await
    }

    /// When an operation on `capacity` starting now must have finished: its
    /// role's whole-operation deadline, which its retries share.
    fn deadline(&self, capacity: CommitCapacity) -> Option<tokio::time::Instant> {
        self.route(capacity)
            .prelude
            .deadline()
            .map(|deadline| tokio::time::Instant::now() + deadline)
    }

    /// Run one replayable commit `attempt` under `label`'s retry policy:
    /// the commit's one retry owner. A wait's resolution and its due
    /// settlement retry under the wait policy, every other commit under the
    /// durable one.
    async fn replay<T, Fut>(
        &self,
        label: CommitLabel,
        deadline: Option<tokio::time::Instant>,
        attempt: impl FnMut() -> Fut,
    ) -> Result<T, DurableError>
    where
        Fut: std::future::Future<Output = Result<T, Attempt<DurableError>>>,
    {
        let policy = if label == CommitLabel::WAIT_RESOLVE || label == CommitLabel::WAIT_TIMEOUT {
            &self.pools.wait_retry
        } else {
            &self.pools.durable_retry
        };
        policy
            .run(deadline, Attempt::is_aborted, attempt)
            .await
            .map_err(Attempt::into_error)
    }

    /// [`open`](Self::open) for a replayable commit: its first statement
    /// also reads the transaction's id, by which a lost `COMMIT` is
    /// reconciled, and for an owner's commit `owner` runs the ownership
    /// fence, answering the state revision it bumped (`None`: fenced out).
    async fn open_replayable(
        &self,
        label: CommitLabel,
        owner: Option<&ActorTx>,
    ) -> Result<(Tx, DurableInstant, Option<i64>), Attempt<DurableError>> {
        tracing::trace!(label = label.as_str(), "durable postgres commit");
        let route = self.route(label.capacity());
        let mut locked = begin_durable(route.pool, &self.fence, route.prelude)
            .await
            .map_err(|error| attempt(store_failure(error)))?;
        let (recorded, now, xact, fence): (Option<i32>, i64, String, Option<i64>) = match owner {
            Some(owner) => sqlx::query_as(SQL.postgres.owner_envelope.sql())
                .bind(owner.actor().as_str())
                .bind(owner.epoch().0)
                .fetch_one(crate::observed_sql::executor(locked.connection()))
                .await
                .map_err(|error| attempt(sqlx_failure(error)))?,
            None => {
                let (recorded, now, xact): (Option<i32>, i64, String) =
                    sqlx::query_as(SQL.postgres.fence_clock_and_xact.sql())
                        .fetch_one(crate::observed_sql::executor(locked.connection()))
                        .await
                        .map_err(|error| attempt(sqlx_failure(error)))?;
                (recorded, now, xact, None)
            }
        };
        let mut tx = locked
            .admit(&self.fence, recorded)
            .await
            .map_err(|error| attempt(store_failure(error)))?;
        tx.note_xact(XactId::new(xact));
        let now = self.instant_or(now).map_err(Attempt::Failed)?;
        Ok((tx, now, fence))
    }

    /// End a replayable commit's attempt: roll a refusal back, and commit a
    /// value, reconciling a lost `COMMIT` from the transaction's recorded
    /// outcome. Contention and a confirmed rollback of a lost `COMMIT` are
    /// aborted attempts; an unknown outcome is the operation's answer.
    async fn settle<T>(
        &self,
        label: CommitLabel,
        tx: Tx,
        outcome: Result<T, DurableError>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<T, Attempt<DurableError>> {
        let value = match outcome {
            Ok(value) => value,
            Err(error) => {
                // A failed transaction is gone before any pause: rolled back
                // here, or by the server when its connection closes.
                let _ = tx.rollback().await;
                return Err(attempt(error));
            }
        };
        let settle = Settle {
            pool: self.route(label.capacity()).pool,
            retry: &self.pools.durable_retry,
            deadline,
            #[cfg(any(test, feature = "testing"))]
            fault: self.commit_fault.as_deref(),
        };
        match tx.commit_reconciled(&settle).await {
            Ok(()) => Ok(value),
            Err(Uncommitted::RolledBack(error)) => Err(attempt(sqlx_failure(error))),
            Err(Uncommitted::Lost(error)) => {
                Err(Attempt::Aborted(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: format!(
                        "the connection was lost at COMMIT, which rolled back: {error}"
                    ),
                })))
            }
            Err(Uncommitted::Unknown(message)) => {
                Err(Attempt::Failed(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message,
                })))
            }
        }
    }
}
