//! Host turn control on the durable substrate (ADR 0132 §3, §11; L3,
//! FIG-5172).
//!
//! A cancel is session mail: one mailbox transaction records the turn's
//! cancel request (the first policy wins, a stronger mode escalates it) and
//! control-wakes the session, whose owner honours it at its next fenced read
//! and ends the turn `Cancelled`. A cancel of a turn no run opened yet, whose
//! input is still queued, withdraws that input in the same transaction
//! instead (FIG-5262). A turn's terminal is its run row's: the terminal, its
//! typed cause and the head revision its commit published.

use std::sync::Arc;

use lash_durable::domain::{
    MailAnswer, MailDomainWrite, TurnCancelAnswer, TurnCancelRequest as DurableCancelRequest,
    TurnEnd,
};
use lash_durable::{CommitLabel, DurableError, MailTx};
use serde::{Deserialize, Serialize};

pub use lash_core_store::turn_control_vocabulary::*;
pub use lash_sansio::{TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence};

use super::RuntimeError;
use crate::{Backend, TurnOutcome, TurnStop};
use lash_sansio::{InputId, SessionId};

/// What a cancel request did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "cancellation", rename_all = "snake_case")]
pub enum TurnCancelOutcome {
    Requested(TurnCancellationEvidence),
    AlreadyRequested(TurnCancellationEvidence),
    /// The turn already held a weaker request and this stronger one upgraded
    /// it. The evidence is the escalating request's.
    Escalated(TurnCancellationEvidence),
    /// The turn already accepted a different undelivered-input policy.
    ///
    /// Cancellation policy belongs to the first request the turn accepted. A
    /// timing escalation may change when that cancellation is honoured, but
    /// it never changes who accepted the policy or what that policy is.
    PolicyConflict {
        requested: TurnCancelUndeliveredInputPolicy,
        accepted: TurnCancellationEvidence,
    },
    /// No run had opened the turn yet: its input was still queued, and the
    /// request withdrew it. The input never runs. Distinct from a cancelled
    /// run: nothing of the input was applied or interrupted.
    Withdrawn {
        /// The withdrawn input.
        input: InputId,
    },
    /// The turn already ended: nothing was written.
    CompletionWonRace,
    /// The session holds no such turn, open or queued: nothing was written.
    UnknownOrRevoked,
}

/// Result of a cancel request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelReceipt {
    pub outcome: TurnCancelOutcome,
}

/// The terminal of one turn, as its run row records it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnTerminal {
    Committed {
        /// A stopped turn's typed cause, with its cancellation evidence. The
        /// answer body is read from the session's committed head.
        stop: Option<TurnStop>,
    },
}

/// Terminal attachment for a foreground turn.
#[async_trait::async_trait]
pub trait TurnAttach: Send + Sync {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError>;
}

impl TurnTerminal {
    /// The terminal of a turn that ended with `outcome`.
    #[must_use]
    pub fn committed(outcome: &TurnOutcome) -> Self {
        Self::Committed {
            stop: match outcome {
                TurnOutcome::Stopped(stop) => Some(stop.clone()),
                _ => None,
            },
        }
    }
}

/// The typed cause a turn's terminal records: its stop, if it stopped.
///
/// # Errors
///
/// [`RuntimeError`] when the stop does not encode.
pub fn turn_stop_cause(stop: &TurnStop) -> Result<String, RuntimeError> {
    serde_json::to_string(stop).map_err(|error| {
        RuntimeError::new(
            crate::RuntimeErrorCode::TurnTerminalDecode,
            format!("a turn stop does not encode: {error}"),
        )
    })
}

/// Where a [`TurnWorkDriver`] publishes the queue change its withdrawal made:
/// best-effort, after the withdrawal committed, so a publication that fails
/// never fails the cancel.
#[async_trait::async_trait]
pub trait QueueWithdrawalPublisher: Send + Sync {
    /// Publish that `input` left `session`'s queue, withdrawn.
    async fn publish_withdrawn(&self, session: &SessionId, input: &InputId);
}

/// Exact-turn control over the durable backend.
///
/// `Requested` means the request was recorded on the turn's row and the
/// session woken: the turn's owner, or the next one after a crash, ends the
/// turn `Cancelled` without starting new work. Lash cannot guarantee that
/// detached tasks or non-cooperative providers have stopped.
///
/// A turn no run opened yet is addressed by the run its queued input will
/// open: the run its source key names, or else its input id. Cancelling it
/// withdraws the input instead and answers
/// [`Withdrawn`](TurnCancelOutcome::Withdrawn). One mailbox transaction does
/// exactly one of the two: when the session's admission bound the input
/// first, the request cancels the run that took it.
///
/// Session and turn ids are routing identity, not authorization. Hosts must
/// enforce authorization before exposing this driver across a trust boundary.
#[derive(Clone)]
pub struct TurnWorkDriver {
    terminal_poll: super::PollPacing,
    backend: Backend,
    withdrawals: Option<Arc<dyn QueueWithdrawalPublisher>>,
}

impl std::fmt::Debug for TurnWorkDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnWorkDriver").finish_non_exhaustive()
    }
}

impl TurnWorkDriver {
    /// Turn control over `backend`.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            withdrawals: None,
            terminal_poll: super::PollPacing::terminal_standard(),
        }
    }

    /// Poll terminal rows on this validated schedule.
    pub fn with_terminal_pacing(mut self, pacing: super::PollPacing) -> Self {
        self.terminal_poll = pacing;
        self
    }

    /// This driver, publishing each withdrawal's queue change to
    /// `publisher` once it committed.
    #[must_use]
    pub fn publishing_withdrawals(mut self, publisher: Arc<dyn QueueWithdrawalPublisher>) -> Self {
        self.withdrawals = Some(publisher);
        self
    }

    /// The turn a cancel of the run `run` addresses: on the durable path a
    /// run is one turn.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] when the address is invalid.
    pub async fn running_turn(&self, run: &TurnAddress) -> Result<TurnAddress, RuntimeError> {
        run.validate()?;
        Ok(run.clone())
    }

    /// Request a cancel of `request`'s turn, or withdraw its input while no
    /// run has opened it.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] when the request is invalid or the store refused it;
    /// nothing was written.
    pub async fn request_cancel(
        &self,
        request: TurnCancelRequest,
    ) -> Result<TurnCancelReceipt, RuntimeError> {
        request.validate()?;
        let evidence = request.evidence();
        let answer = request_turn_cancel(
            &self.backend,
            DurableCancelRequest {
                session: request.address.session_id.clone(),
                run: request.address.turn_id.clone(),
                request_id: request.request_id.clone(),
                origin: request.origin.clone(),
                reason: request.reason.clone(),
                undelivered: request.undelivered,
                mode: request.mode,
            },
        )
        .await
        .map_err(store_error)?;
        let accepted = |accepted: &DurableCancelRequest| TurnCancellationEvidence {
            request_id: accepted.request_id.clone(),
            origin: accepted.origin.clone(),
            reason: accepted.reason.clone(),
            undelivered: accepted.undelivered,
            mode: accepted.mode,
            honoured_after_step: None,
        };
        let outcome = match answer {
            TurnCancelAnswer::Requested => TurnCancelOutcome::Requested(evidence),
            TurnCancelAnswer::Escalated { .. } => TurnCancelOutcome::Escalated(evidence),
            TurnCancelAnswer::AlreadyRequested { accepted: existing } => {
                TurnCancelOutcome::AlreadyRequested(accepted(&existing))
            }
            TurnCancelAnswer::PolicyConflict { accepted: existing } => {
                TurnCancelOutcome::PolicyConflict {
                    requested: request.undelivered,
                    accepted: accepted(&existing),
                }
            }
            TurnCancelAnswer::Withdrawn { input } => {
                if let Some(publisher) = &self.withdrawals {
                    publisher
                        .publish_withdrawn(&request.address.session_id, &input)
                        .await;
                }
                TurnCancelOutcome::Withdrawn { input }
            }
            TurnCancelAnswer::AlreadyEnded => {
                let ended = self
                    .backend
                    .durable()
                    .turn_end(&request.address.session_id, &request.address.turn_id)
                    .await
                    .map_err(store_error)?;
                if ended.is_some() {
                    TurnCancelOutcome::CompletionWonRace
                } else {
                    TurnCancelOutcome::UnknownOrRevoked
                }
            }
        };
        Ok(TurnCancelReceipt { outcome })
    }

    /// The terminal of the turn at `address`, once it ended.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] when the address is invalid, the store fails, or the
    /// recorded cause does not decode.
    pub async fn await_terminal(
        &self,
        address: &TurnAddress,
    ) -> Result<TurnTerminal, RuntimeError> {
        address.validate()?;
        let clock = self.backend.clock();
        let mut interval = self.terminal_poll.initial();
        loop {
            let ended = self
                .backend
                .durable()
                .turn_end(&address.session_id, &address.turn_id)
                .await
                .map_err(store_error)?;
            if let Some(ended) = ended {
                return terminal_of(address, &ended);
            }
            clock.sleep(interval).await;
            interval = self.terminal_poll.next(interval);
        }
    }
}

#[async_trait::async_trait]
impl TurnAttach for TurnWorkDriver {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError> {
        TurnWorkDriver::await_terminal(self, address).await
    }
}

/// The terminal a turn's row records: its run's end, stored as every run's
/// end is ([`RunTerminalCause`](crate::store::RunTerminalCause)).
fn terminal_of(address: &TurnAddress, ended: &TurnEnd) -> Result<TurnTerminal, RuntimeError> {
    let decode_error = |error: String| {
        RuntimeError::new(
            crate::RuntimeErrorCode::TurnTerminalDecode,
            format!(
                "the terminal cause of turn `{}` in session `{}` is not a turn's: {error}",
                address.turn_id, address.session_id
            ),
        )
    };
    let stop = match &ended.cause {
        crate::store::RunTerminalCause::Committed { outcome, .. } => outcome.stop().cloned(),
        crate::store::RunTerminalCause::Cancelled { evidence } => Some(TurnStop::Cancelled {
            evidence: evidence.clone(),
        }),
        other => {
            return Err(decode_error(format!(
                "a session actor's turn ends committed or cancelled, not {other:?}"
            )));
        }
    };
    Ok(TurnTerminal::Committed { stop })
}

/// Request a cancel of one of a session's turns, from outside the session
/// actor: one mailbox transaction records it on the turn's cancel-request
/// row and control-wakes the session (`mail.session`). The owner sees it on
/// the turn's row at its next fenced read, through the wake hint or its
/// poll; the next owner sees it after a crash.
///
/// # Errors
///
/// The store's refusal; nothing was written.
pub async fn request_turn_cancel(
    backend: &Backend,
    request: DurableCancelRequest,
) -> Result<TurnCancelAnswer, DurableError> {
    let mut tx = MailTx::new();
    tx.write(MailDomainWrite::RequestTurnCancel(request));
    let mut commit = backend.commit_mail(tx, CommitLabel::MAIL_SESSION).await?;
    match commit.answers.pop() {
        Some(MailAnswer::RequestTurnCancel(answer)) if commit.answers.is_empty() => Ok(answer),
        other => Err(DurableError::Store(lash_durable::StoreFailure {
            kind: lash_durable::StoreFailureKind::Corrupt,
            message: format!("a turn cancel request was answered with {other:?}"),
        })),
    }
}

fn store_error(error: DurableError) -> RuntimeError {
    RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, error.to_string())
}
