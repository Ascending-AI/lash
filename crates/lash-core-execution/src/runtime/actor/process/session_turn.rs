//! A `SessionTurn` process on its actor (FIG-5208): the turn it runs is
//! session mail to its child session's actor, never a turn run inside the
//! process.
//!
//! The process's passes, each from committed rows:
//!
//! 1. It pins a `child_session` wait on itself (`process.advance`), before
//!    anything can end the child's turn.
//! 2. It creates the child session, or finds it, and mails the turn's input
//!    to it under the child turn's id ([`SessionTurns::mail`]), then records
//!    that it did (`process.advance`). Both writes are idempotent by the
//!    child's id and the turn's, so a pass that did not commit mails the
//!    same input again.
//! 3. It releases as `waiting`. The child's session actor admits the input
//!    and runs the turn with the deployment's turn services; the
//!    transaction that ends the turn (its commit, or its cancel) resolves
//!    the wait and wakes the process.
//! 4. Once the wait is resolved, it commits its terminal from the child
//!    turn's committed end ([`SessionTurns::outcome`]).
//!
//! A cancel withdraws the child's input while no run has taken it, and the
//! process ends `Cancelled` at once; once a run took it, the cancel is the
//! child turn's own cancel request, and the process ends with that turn.

use lash_durable::domain::{ProcessActorRow, ProcessWrite, ScopeKey, WaitState};
use lash_durable::runner::Owned;
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, Release};
use serde::{Deserialize, Serialize};

use super::ProcessActivation;
use super::activation::{Live, Pass, corrupt, registry_failure};
use super::driver::StoredWaitId;
use super::terminal::{ProcessParkReason, record_terminal};
use crate::runtime::actor::waits::{self, WaitKind, WaitSpec};
use crate::{CancelOrigin, PluginError, ProcessId, ProcessOutcome, ProcessRecord};

/// The session work a node runs `SessionTurn` processes with: the child
/// session's creation, the mail of its turn's input, the turn's cancel and
/// the process's answer from the turn's end. A deployment implements it
/// over its own plugins, models and stores.
#[async_trait::async_trait]
pub trait SessionTurns: Send + Sync {
    /// Create `process`'s child session, or find the one an earlier pass
    /// created, and mail the turn's input to it under the child turn's id.
    /// Idempotent: a repeat finds the session and the input already there.
    ///
    /// # Errors
    ///
    /// A failure another pass may not meet; nothing deterministic is an
    /// error ([`SessionTurnMail::Refused`]).
    async fn mail(&self, process: &ProcessRecord) -> Result<SessionTurnMail, PluginError>;

    /// Stop `process`'s child turn: withdraw its input while no run has
    /// taken it, or else request the turn's cancel.
    ///
    /// # Errors
    ///
    /// A store failure; nothing was withdrawn.
    async fn cancel(&self, process: &ProcessRecord) -> Result<SessionTurnCancel, PluginError>;

    /// The process's answer from its child turn's committed end.
    ///
    /// # Errors
    ///
    /// A store failure, or a child turn that has not ended.
    async fn outcome(&self, process: &ProcessRecord) -> Result<ProcessOutcome, PluginError>;
}

/// What [`SessionTurns::mail`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionTurnMail {
    /// The child session holds the turn's input, under the child turn's id.
    Mailed,
    /// No pass can mail it (a request this deployment refuses for good, or
    /// an execution authority that does not validate): the process ends
    /// with this answer.
    Refused(ProcessOutcome),
}

/// What [`SessionTurns::cancel`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTurnCancel {
    /// No run took the turn's input, and it is withdrawn: no turn runs.
    Withdrawn,
    /// A run took it: the turn's cancel is requested, and its end resolves
    /// the process's wait.
    Requested,
    /// The turn already ended: its end resolved the process's wait.
    Ended,
}

/// What a `SessionTurn` process keeps beside its record, on its row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildTurnDriver {
    /// The `child_session` wait the child turn's end resolves.
    wait: StoredWaitId,
    /// Whether the turn's input is mailed to the child session.
    mailed: bool,
    /// Whether a cancel reached the child turn.
    cancel_requested: bool,
}

impl ChildTurnDriver {
    fn decode(stored: Option<&str>) -> Result<Option<Self>, DurableError> {
        stored
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| corrupt("a session-turn process's driver state", error))
    }

    #[expect(
        clippy::expect_used,
        reason = "the driver state is plain data whose encoding cannot fail"
    )]
    fn advance(&self, tx: &mut ActorTx, process: &ProcessId, expected_rev: u64) {
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev,
            driver_json: serde_json::to_string(self).expect("the driver state encodes"),
        }));
    }
}

impl ProcessActivation {
    /// One pass of a `SessionTurn` process, under `tx`. See the module docs.
    #[expect(
        clippy::too_many_arguments,
        reason = "the pass's rows, its record, its cancel and the activation's memory"
    )]
    pub(super) async fn session_turn_pass(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        row: &ProcessActorRow,
        record: &ProcessRecord,
        cancel: Option<CancelOrigin>,
        live: &mut Live,
    ) -> Result<Pass, DurableError> {
        let Some(turns) = self.session_turns.clone() else {
            if let Some(origin) = cancel {
                return self.end_engine_free(owned, tx, process, live, origin).await;
            }
            return self
                .park(owned, tx, &ProcessParkReason::UnservedSessionTurn)
                .await;
        };
        let Some(mut driver) = ChildTurnDriver::decode(row.driver_json.as_deref())? else {
            if let Some(origin) = cancel {
                return self.end_engine_free(owned, tx, process, live, origin).await;
            }
            // The wait exists before the turn's input does, so the end of
            // the turn always finds it.
            let (wait, _) = waits::pin(
                &mut tx,
                self.backend.completion_secrets(),
                WaitSpec {
                    kind: WaitKind::ChildSession,
                    scope: ScopeKey::Process(process.clone()),
                    target_process: Some(process.clone()),
                    deadline: None,
                },
            )
            .map_err(|refusal| corrupt("a child-session wait", refusal))?;
            ChildTurnDriver {
                wait: StoredWaitId(wait.id()),
                mailed: false,
                cancel_requested: false,
            }
            .advance(&mut tx, process, row.state_rev);
            owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
            return Ok(Pass::Again);
        };
        let wait = owned.store().wait(&driver.wait.0).await?;
        match wait.map(|wait| wait.state) {
            Some(WaitState::Resolved) => {
                let outcome = turns
                    .outcome(record)
                    .await
                    .map_err(|error| registry_failure(&error))?;
                return self.end(owned, tx, process, &outcome).await;
            }
            Some(WaitState::Pending) => {}
            other => {
                return Err(corrupt(
                    "a session-turn process's child-session wait",
                    format!("it is {other:?} while the process runs"),
                ));
            }
        }
        if let Some(origin) = cancel
            && !driver.cancel_requested
        {
            match turns
                .cancel(record)
                .await
                .map_err(|error| registry_failure(&error))?
            {
                SessionTurnCancel::Withdrawn => {
                    return self.end_engine_free(owned, tx, process, live, origin).await;
                }
                SessionTurnCancel::Requested | SessionTurnCancel::Ended => {
                    driver.cancel_requested = true;
                    driver.advance(&mut tx, process, row.state_rev);
                    tx.ack_seen().give_up(Release::Waiting { next_due: None });
                    owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                    return Ok(Pass::Released);
                }
            }
        }
        if !driver.mailed {
            match turns
                .mail(record)
                .await
                .map_err(|error| registry_failure(&error))?
            {
                SessionTurnMail::Mailed => {
                    driver.mailed = true;
                    driver.advance(&mut tx, process, row.state_rev);
                    owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                    return Ok(Pass::Again);
                }
                SessionTurnMail::Refused(outcome) => {
                    return self.end(owned, tx, process, &outcome).await;
                }
            }
        }
        // The child turn runs on its session's actor; its end wakes this one.
        tx.ack_seen().give_up(Release::Waiting { next_due: None });
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        Ok(Pass::Released)
    }

    async fn end(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        outcome: &ProcessOutcome,
    ) -> Result<Pass, DurableError> {
        record_terminal(&mut tx, process, outcome)?;
        tx.ack_seen();
        owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
        Ok(Pass::Again)
    }
}
