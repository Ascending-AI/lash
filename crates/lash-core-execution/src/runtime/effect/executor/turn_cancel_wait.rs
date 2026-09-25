//! The turn-cancel controls a wait or a process operation carries.

use tokio_util::sync::CancellationToken;

use super::WaitControls;

/// Valid turn-cancellation controls for a process operation.
///
/// Presence means the process operation observes the exact turn gate; absence
/// means it does not. Keeping token and scope together makes an enabled
/// observation without a durable scope unrepresentable.
#[derive(Clone)]
pub struct ProcessTurnCancellation {
    pub cancellation: CancellationToken,
    pub scope: crate::ExecutionScope,
}

impl ProcessTurnCancellation {
    pub fn new(cancellation: CancellationToken, scope: crate::ExecutionScope) -> Self {
        Self {
            cancellation,
            scope,
        }
    }
}

/// The complete turn-cancel trio for one wait: the cancellation token the wait
/// races against, whether the wait observes turn cancellation, and the
/// execution scope its turn-cancel gate registers under.
///
/// The observation flag is the presence of the scope, so a wait that observes
/// turn cancellation without naming a durable scope — and, more importantly, a
/// wait that stamps the scope while silently keeping the executor's default
/// observation — is unrepresentable. Every in-workspace wait builder takes this
/// value whole (`RuntimeEffectLocalExecutor::sleep_under`,
/// `RuntimeEffectLocalExecutor::await_event_under`,
/// `ProcessOpScope::with_turn_cancellation`), and it is produced by a single
/// accessor on the execution that owns the observation decision
/// (`RuntimeExecutionContext::turn_cancel_wait`, or
/// `ScopedEffectController::turn_cancel_wait` where no execution context
/// exists).
#[derive(Clone)]
pub struct TurnCancelWait {
    cancellation: CancellationToken,
    /// `Some` when the wait attaches the turn-cancel gate for that scope;
    /// `None` when the enclosing execution runs without turn observation, as
    /// process bodies do.
    observed_scope: Option<crate::ExecutionScope>,
}

impl TurnCancelWait {
    /// The wait races `cancellation` and observes the turn-cancel gate of
    /// `scope`.
    pub fn observing(cancellation: CancellationToken, scope: crate::ExecutionScope) -> Self {
        Self {
            cancellation,
            observed_scope: Some(scope),
        }
    }

    /// The wait races `cancellation` and never attaches a turn-cancel gate.
    pub fn unobserved(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            observed_scope: None,
        }
    }

    /// The cooperative cancellation the wait races, for callers that carry
    /// the trio whole and still need the token alone.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Whether the wait races its turn's cancellation gate.
    pub fn observes_turn_cancel(&self) -> bool {
        self.observed_scope.is_some()
    }

    /// The scope whose turn-cancel gate the wait races, if it races one.
    pub fn observed_scope(&self) -> Option<&crate::ExecutionScope> {
        self.observed_scope.as_ref()
    }

    /// The turn cancellation a process operation observes, if any.
    pub(crate) fn process_turn_cancellation(&self) -> Option<ProcessTurnCancellation> {
        self.observed_scope
            .as_ref()
            .map(|scope| ProcessTurnCancellation::new(self.cancellation.clone(), scope.clone()))
    }

    pub(super) fn controls(&self) -> WaitControls {
        WaitControls {
            cancellation: self.cancellation.clone(),
            observe_turn_cancel: self.observed_scope.is_some(),
            turn_cancel_scope: self.observed_scope.clone(),
        }
    }
}
