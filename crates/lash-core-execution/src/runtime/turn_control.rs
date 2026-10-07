//! Host turn control on the durable substrate (ADR 0132 §3, §11; L3,
//! FIG-5172).
//!
//! A cancel is session mail: one mailbox transaction records the turn's
//! cancel request (the first policy wins, a stronger mode escalates it) and
//! control-wakes the session, whose owner honours it at its next fenced read
//! and ends the turn `Cancelled`. A turn's terminal is its run row's: the
//! terminal, its typed cause and the head revision its commit published.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

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

/// A host-local request to stop the turn it was handed to: a shutdown lever,
/// a process runner stopping its child turn.
///
/// `Immediate` fires the handle's token; `AfterStep` leaves the token alone
/// and asks the turn to stop at its next step boundary. The first origin
/// recorded with a request wins; a token installed with an origin supplies the
/// origin when nothing else recorded one. A durable cancel is session mail
/// ([`request_turn_cancel`]), not this handle.
#[derive(Clone, Default)]
pub struct LocalTurnStop {
    immediate: CancellationToken,
    after_step: CancellationToken,
    origin: Arc<Mutex<LocalStopOrigin>>,
}

#[derive(Default)]
struct LocalStopOrigin {
    configured: Option<Option<String>>,
    observed: Option<Option<String>>,
}

impl LocalTurnStop {
    /// A stop nothing has requested yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A stop requested `Immediate` when `token` fires, with `origin` as the
    /// origin to record if the token fires on its own.
    pub fn from_token(token: CancellationToken, origin: Option<String>) -> Self {
        let stop = Self {
            immediate: token,
            ..Self::default()
        };
        stop.origin.lock_recover().configured = Some(origin);
        stop
    }

    /// Request the stop in `mode`, recording `origin` unless an earlier request
    /// already recorded one.
    pub fn request(&self, mode: TurnCancelMode, origin: Option<String>) {
        {
            let mut state = self.origin.lock_recover();
            if state.observed.is_none() {
                state.observed = Some(origin);
            }
        }
        match mode {
            TurnCancelMode::Immediate => self.immediate.cancel(),
            TurnCancelMode::AfterStep => self.after_step.cancel(),
        }
    }

    /// The origin the stop's evidence carries.
    pub fn origin(&self) -> Option<String> {
        let state = self.origin.lock_recover();
        state
            .observed
            .clone()
            .or_else(|| state.configured.clone())
            .flatten()
    }

    /// The `Immediate` lever's token.
    pub fn immediate_token(&self) -> CancellationToken {
        self.immediate.clone()
    }

    /// The strongest mode requested so far, if any.
    pub fn requested(&self) -> Option<TurnCancelMode> {
        if self.immediate.is_cancelled() {
            Some(TurnCancelMode::Immediate)
        } else if self.after_step.is_cancelled() {
            Some(TurnCancelMode::AfterStep)
        } else {
            None
        }
    }

    /// The cancellation turn `turn` honours now: an `Immediate` request
    /// anywhere, an `AfterStep` one only at a step boundary.
    #[must_use]
    pub fn honoured(
        &self,
        turn: &crate::TurnId,
        at_step_boundary: bool,
    ) -> Option<TurnCancellationEvidence> {
        let mode = self.requested()?;
        if mode == TurnCancelMode::AfterStep && !at_step_boundary {
            return None;
        }
        Some(TurnCancellationEvidence {
            origin: self.origin(),
            mode,
            ..TurnCancellationEvidence::internal(turn)
        })
    }
}

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
    /// The turn already ended: nothing was written.
    CompletionWonRace,
    /// The session holds no such turn: nothing was written.
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

/// How long [`TurnWorkDriver::await_terminal`] waits between reads of the
/// turn's row: from the first interval, doubling to the last.
const TERMINAL_POLL: (Duration, Duration) = (Duration::from_millis(20), Duration::from_secs(1));

/// Exact-turn control over the durable backend.
///
/// `Requested` means the request was recorded on the turn's row and the
/// session woken: the turn's owner, or the next one after a crash, ends the
/// turn `Cancelled` without starting new work. Lash cannot guarantee that
/// detached tasks or non-cooperative providers have stopped.
///
/// Session and turn ids are routing identity, not authorization. Hosts must
/// enforce authorization before exposing this driver across a trust boundary.
#[derive(Clone)]
pub struct TurnWorkDriver {
    backend: Backend,
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
        Self { backend }
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

    /// Request a cancel of `request`'s turn.
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
        let mut interval = TERMINAL_POLL.0;
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
            interval = interval.saturating_mul(2).min(TERMINAL_POLL.1);
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
                "the terminal cause of turn `{}` in session `{}` does not decode: {error}",
                address.turn_id, address.session_id
            ),
        )
    };
    let cause = ended
        .cause_json
        .as_deref()
        .ok_or_else(|| decode_error("the row records no cause".to_owned()))?;
    let stop = match serde_json::from_str::<crate::store::RunTerminalCause>(cause)
        .map_err(|error| decode_error(error.to_string()))?
    {
        crate::store::RunTerminalCause::Committed { outcome, .. } => outcome.stop().cloned(),
        crate::store::RunTerminalCause::Cancelled { evidence } => {
            Some(TurnStop::Cancelled { evidence })
        }
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
