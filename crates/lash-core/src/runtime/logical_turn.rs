use super::*;
use crate::ActorContext;
use lash_vocabulary::identity::RunIdCoreSupport;
use lash_vocabulary::identity::TurnId;

/// The rows one turn executes, each admitted to the turn's run (FIG-3927):
/// what the run's admission bound.
pub(super) struct LogicalTurnAdmissions {
    pub(super) turn_inputs: Vec<crate::AdmittedTurnInputs>,
}

impl LogicalTurnAdmissions {
    pub(super) fn new(turn_inputs: Vec<crate::AdmittedTurnInputs>) -> Self {
        Self { turn_inputs }
    }
}

impl LashRuntime {
    pub(in crate::runtime) fn emit_physical_turn_start(
        observer: &TurnObserver,
        scoped_effect_controller: &ActorContext,
        run: &TurnId,
    ) {
        let mut cursor =
            super::turn_loop::turn_observation_cursor(scoped_effect_controller, run, "start");
        super::turn_loop::emit_turn_started(
            observer,
            &mut cursor,
            run,
            &lash_vocabulary::identity::RunId::from_turn_id(run.clone()),
        );
    }

    /// How this runtime's turns frame their stream deltas for the host.
    pub(super) fn delta_framing(&self) -> super::turn_observer::DeltaFraming {
        super::turn_observer::DeltaFraming {
            clock: std::sync::Arc::clone(&self.host.core.clock),
            coalescing: self.host.core.control.delta_coalescing,
        }
    }
}
