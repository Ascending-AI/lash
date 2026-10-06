//! The session's mailbox, drained on every claim (ADR 0132 §3, §12). Owned
//! by L3s (FIG-5196).
//!
//! Every producer of session work (pending inputs, queued work batches,
//! control intents, turn cancel requests) writes its row and wakes the
//! session in its own transaction (`wake_within` in each dialect). The
//! session activation calls [`drain_session_mail`] first on every claim: it
//! reads the mailbox tables, applies them under the epoch (inputs, queued
//! work, control intents, plugin transitions: what the shift applied under
//! its fence) and returns what the activation must do.

use lash_durable::{ActorTx, DurableError};

use super::session::{AdmittedInputs, TurnCancelRequest};
use crate::ActorContext;

/// A request to close the session, drained from its mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCloseRequest {
    /// The request, encoded by its owner.
    pub request_json: String,
}

/// What a drain hands the session activation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMailDrain {
    /// Inputs to admit as the next turn.
    pub admit: Option<AdmittedInputs>,
    /// A cancel of the open turn to run.
    pub cancel: Option<TurnCancelRequest>,
    /// A close of the session to begin.
    pub close: Option<SessionCloseRequest>,
}

/// Why a drain failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionMailError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// A mailbox row does not decode.
    #[error("session mail does not decode: {0}")]
    Undecodable(String),
}

/// Drain the session's mailbox on `tx`, applying it under the epoch, and
/// return what the activation must do.
///
/// # Errors
///
/// [`SessionMailError`].
pub async fn drain_session_mail(
    _cx: &ActorContext,
    _tx: &mut ActorTx,
) -> Result<SessionMailDrain, SessionMailError> {
    todo!(
        "L3s (FIG-5196): drain pending inputs, queued work, control intents and plugin transitions under the epoch"
    )
}
