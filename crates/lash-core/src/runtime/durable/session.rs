//! The session activation and the turn's phases (ADR 0132 §4). Owned by V0
//! (FIG-5170), then L3 (FIG-5172).
//!
//! # Contracts
//!
//! - The durable entry is: load rows, [`restore_turn`] (exactly one
//!   `restore_from_checkpoint`), re-deliver the pending effect, then
//!   [`run_phases`]. The turn driver becomes this phase runner; it commits at
//!   the catalog's labels (`turn.admit`, `turn.prepare`, `model.start`,
//!   `model.done`, `round.present+model.start`, `turn.commit`) and never
//!   re-executes orchestration to reach a recorded outcome.
//! - The activation calls `session_mail::drain_session_mail` first on every
//!   claim and acts on what it returns.
//! - Extension points that let L3, L4 and L6b work in parallel: the
//!   `model.done` transaction calls `round::admit_round` (S4); the
//!   `round.present+model.start` transaction calls `round::present`; the
//!   `turn.commit` transaction calls `process::end_scope(tx,
//!   ScopeKey::Turn(..), batch)` (S6). L3 owns the transactions; the callee's
//!   owner owns what it writes.
//! - The checkpoint is L3a's bounded encoding, referenced from
//!   [`TurnRow::checkpoint_ref`] by digest.

use lash_durable::runner::{Activation, Owned};
use lash_durable::{ActorTx, DurableError, DurableInstant};

use crate::{ActorContext, Backend, Effect, InputId, SessionId, TurnId, TurnMachine};

pub use lash_durable::domain::{ModelPin, TurnCancelRequest, TurnPhase, TurnRow, TurnTerminal};

/// The session actor's activation: claim, drain mail, admit and run phases,
/// release.
#[derive(Clone, Debug)]
pub struct SessionActivation {
    backend: Backend,
}

impl SessionActivation {
    /// The activation of sessions over `backend`.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }

    /// The backend it runs over.
    #[must_use]
    pub fn backend(&self) -> &Backend {
        &self.backend
    }
}

#[async_trait::async_trait]
impl Activation for SessionActivation {
    async fn activate(&self, _owned: Owned) {
        todo!("V0 (FIG-5170): drain session mail, admit, restore and run the turn's phases")
    }
}

/// The inputs a drain hands the activation to admit as one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedInputs {
    /// The run they open.
    pub run: TurnId,
    /// The inputs, in admission order.
    pub inputs: Vec<InputId>,
    /// The admission (base head revision, run spec, plugin revision),
    /// encoded by its owner.
    pub admission_json: String,
}

/// A turn restored from its rows: the machine, the effect it re-delivers,
/// and its row.
pub struct RestoredTurn {
    /// The machine, restored from the checkpoint.
    pub machine: TurnMachine,
    /// The effect the checkpoint re-delivers, if it is waiting on one.
    pub pending: Option<Effect>,
    /// The turn's row.
    pub row: TurnRow,
}

/// How a phase run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseExit {
    /// The turn committed with this terminal.
    Committed(TurnTerminal),
    /// Nothing is runnable: release as `waiting` until `due` or mail.
    Suspended {
        /// The earliest due time.
        due: Option<DurableInstant>,
    },
    /// Ownership was lost: drop everything held for the actor.
    Lost,
}

/// Why a turn was not admitted; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnAdmitRefusal {
    /// The session already has an unfinished turn.
    #[error("session {0} already has an unfinished turn")]
    OpenTurnExists(SessionId),
}

/// Why a turn could not be restored from its rows.
#[derive(Debug, thiserror::Error)]
pub enum TurnRestoreError {
    /// The row names no checkpoint to restore from.
    #[error("turn {0} has no checkpoint")]
    NoCheckpoint(TurnId),
    /// The checkpoint does not restore.
    #[error(transparent)]
    Checkpoint(#[from] lash_sansio::TurnCheckpointRestoreError),
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
}

/// Why a phase run failed.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// The runtime failed.
    #[error(transparent)]
    Runtime(#[from] crate::RuntimeError),
}

/// Admit `inputs` as the session's turn on `tx`: the turn row, the bound
/// inputs and the turn deadline (`turn.admit`).
///
/// # Errors
///
/// [`TurnAdmitRefusal`]; nothing is recorded.
pub async fn admit_turn(
    _cx: &ActorContext,
    _tx: &mut ActorTx,
    _inputs: AdmittedInputs,
) -> Result<TurnRow, TurnAdmitRefusal> {
    todo!("V0 (FIG-5170): admit a turn: its row, bound inputs and deadline")
}

/// Restore `row`'s turn: exactly one `restore_from_checkpoint`, reported to
/// the context's probe, and the effect it re-delivers.
///
/// # Errors
///
/// [`TurnRestoreError`].
pub async fn restore_turn(
    _cx: &ActorContext,
    _row: &TurnRow,
) -> Result<RestoredTurn, TurnRestoreError> {
    todo!("V0 (FIG-5170): restore a turn from its checkpoint and name its pending effect")
}

/// Run the turn's phases from `turn`, committing at each label, until it
/// commits, suspends or loses ownership.
///
/// # Errors
///
/// [`TurnError`].
pub async fn run_phases(_cx: &ActorContext, _turn: RestoredTurn) -> Result<PhaseExit, TurnError> {
    todo!("V0 (FIG-5170): run a turn's phases with explicit commits")
}

/// Request a cancel of `session`'s turn: mail plus a control wake.
///
/// # Errors
///
/// The store's refusal.
pub async fn request_turn_cancel(
    _backend: &Backend,
    _session: &SessionId,
    _request: TurnCancelRequest,
) -> Result<(), DurableError> {
    todo!("L3 (FIG-5172): request a turn cancel as session mail")
}
