//! The one classification of a [`RuntimeErrorCode`] (FIG-3575).
//!
//! Every code has exactly one posture, declared with its spelling in the code table. The
//! retry projections ([`RuntimeErrorCode::is_retryable`],
//! [`RuntimeErrorCode::is_terminal`]) and the cause a failed turn settles by
//! ([`RuntimeErrorCode::turn_failure_cause`]) are all read from it, so they
//! cannot disagree: a code is terminal exactly when it is an outcome.

use super::RuntimeErrorCode;

/// The decided posture of a [`RuntimeErrorCode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeErrorClass {
    /// Retrying the identical operation is explicitly safe.
    Retryable,
    /// A fact about this attempt: the identical call is not declared safe to
    /// repeat, but a redrive under fresh authority can succeed.
    Redrivable,
    /// Retrying cannot succeed without changing input, configuration,
    /// wiring, or corrupted durable state: a redrive reproduces it.
    Terminal,
    /// A re-executed program refused a replay its journal does not support
    /// (FIG-3586). A redrive by this build reproduces the refusal with zero
    /// dispatch, but the turn is not failed either: redeploying the build
    /// that wrote the journal serves it, so the turn waits for an operator.
    Parked,
}

/// How a turn failure settles, decided by its cause.
///
/// The rule follows FIG-3528: a journaled outcome stays on the result surface,
/// and a live fault aborts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnFailureCause {
    /// Deterministic over the turn's journaled inputs: a redrive reproduces it.
    /// A direct turn records it as a failed turn, and a queued run settles
    /// failed once instead of retrying it.
    Outcome,
    /// A fact about this execution attempt, not about the turn: lost lease,
    /// journal or store I/O, or a process-local task, engine or shutdown
    /// fault. Nothing may be recorded, so the invocation aborts with `Err`. An
    /// aborted direct turn returns its acceptance receipt; a queued run stays
    /// pending for its retry budget.
    LiveFault,
    /// A re-executed lashlang run refused to replay a journal it cannot serve
    /// (FIG-3586): its commands no longer match the recorded ones, or the
    /// journal predates this build's key grammar. Nothing was dispatched and
    /// nothing is recorded as the turn's outcome. The invocation aborts with
    /// `Err` exactly as a live fault does — admission held, receipt returned —
    /// and the park is recorded, but a queued run spends no retry budget on
    /// it: every redrive by this build refuses again with zero dispatch, and
    /// what serves the turn is an operator redeploying the build that wrote
    /// its journal, cancelling it, or forking it.
    Parked,
}

impl TurnFailureCause {
    /// Whether a turn failing with this cause aborts its invocation with
    /// `Err` rather than recording a failed turn.
    pub const fn aborts_invocation(self) -> bool {
        matches!(self, Self::LiveFault | Self::Parked)
    }
}

impl RuntimeErrorCode {
    /// The cause class of a turn that fails with this code: an outcome
    /// exactly when the code is terminal.
    pub const fn turn_failure_cause(&self) -> TurnFailureCause {
        match self.classification() {
            RuntimeErrorClass::Terminal => TurnFailureCause::Outcome,
            RuntimeErrorClass::Retryable | RuntimeErrorClass::Redrivable => {
                TurnFailureCause::LiveFault
            }
            RuntimeErrorClass::Parked => TurnFailureCause::Parked,
        }
    }
}
