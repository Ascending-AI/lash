use super::*;
use crate::ActorContext;
use crate::TurnId;

/// Work admitted at a terminal checkpoint and withheld from that
/// checkpoint's delivery.
///
/// FIG-3157: a terminal finish ends the turn. The committed finish is the
/// turn's answer, so a delivery admitted at `BeforeCompletion` never extends
/// it — it starts a follow-on physical turn inside the same logical run,
/// carrying the admitted work as that turn's input. The rows stay bound to
/// the run throughout (FIG-3927).
#[derive(Clone, Default)]
pub(in crate::runtime) struct WithheldTerminalWork {
    pub(in crate::runtime) queued: Vec<crate::AdmittedQueuedWork>,
    pub(in crate::runtime) turn_inputs: Vec<crate::AdmittedTurnInputs>,
}

impl WithheldTerminalWork {
    pub(in crate::runtime) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.turn_inputs.is_empty()
    }

    pub(in crate::runtime) fn take_if_any(&mut self) -> Option<Self> {
        (!self.is_empty()).then(|| std::mem::take(self))
    }
}

/// The rows one turn executes, each admitted to the turn's run (FIG-3927):
/// what the run's admission bound.
pub(super) struct LogicalTurnAdmissions {
    pub(super) queued: Vec<crate::AdmittedQueuedWork>,
    pub(super) turn_inputs: Vec<crate::AdmittedTurnInputs>,
}

impl LogicalTurnAdmissions {
    pub(super) fn new(
        queued: Vec<crate::AdmittedQueuedWork>,
        turn_inputs: Vec<crate::AdmittedTurnInputs>,
    ) -> Self {
        Self {
            queued,
            turn_inputs,
        }
    }
}

impl LashRuntime {
    pub(in crate::runtime) fn emit_physical_turn_start(
        observer: &TurnObserver,
        scoped_effect_controller: &ActorContext,
        turn_id: &TurnId,
        admissions: &LogicalTurnAdmissions,
        announce_queued_work: bool,
        tool_restore: Option<crate::ToolRestoreReport>,
    ) {
        let mut cursor =
            super::turn_loop::turn_observation_cursor(scoped_effect_controller, turn_id, "start");
        super::turn_loop::emit_turn_started(observer, &mut cursor, turn_id);
        // The restore this run's transition (or a later re-sync) made, when
        // something persisted had no source: the sender reads it on the run's
        // output and observers on the session's feed (FIG-5134).
        if let Some(report) = tool_restore.filter(|report| !report.is_clean()) {
            cursor.observe(
                &observer.for_turn(turn_id),
                crate::engine::ObservedEvent::Activity {
                    correlation_id: None,
                    event: crate::TurnEvent::ToolRestoreReported { report },
                },
            );
        }
        if !announce_queued_work {
            // Work withheld from a terminal checkpoint already announced its
            // start at the boundary that admitted it (FIG-3157).
            return;
        }
        for queued in &admissions.queued {
            let work = queued.materialize_queued_checkpoint_work();
            super::turn_loop::emit_queued_work_started(
                observer,
                &mut cursor,
                turn_id,
                crate::AdmissionBoundary::Idle,
                queued,
                work.turn_causes,
            );
        }
    }

    /// How this runtime's turns frame their stream deltas for the host.
    pub(super) fn delta_framing(&self) -> super::turn_observer::DeltaFraming {
        super::turn_observer::DeltaFraming {
            clock: std::sync::Arc::clone(&self.host.core.clock),
            coalescing: self.host.core.control.delta_coalescing,
        }
    }
}
