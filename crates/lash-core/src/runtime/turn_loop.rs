use super::logical_turn::LogicalTurnAdmissions;
use super::*;
use crate::ActorContext;
use crate::SessionId;
use crate::TurnId;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use context_pressure::ContextPressureStep;
use lash_sansio::core_support::*;

mod context_pressure;
mod durable;
mod execute;
#[cfg(feature = "testing")]
pub mod prepare;
#[cfg(not(feature = "testing"))]
mod prepare;
mod resident_session;

pub(in crate::runtime) use durable::DurableTurn;
pub(in crate::runtime) use resident_session::ResidentSessionContinuity;
pub use resident_session::ResidentSessionState;

fn queued_work_batch_ids(queued: &crate::AdmittedQueuedWork) -> Vec<crate::BatchId> {
    queued
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect()
}

fn turn_phase_id(parent_turn_id: &TurnId, phase: &str) -> TurnId {
    parent_turn_id.with_suffix(format_args!(":{phase}"))
}

/// A fresh observation cursor for one turn-level emission lane of the
/// physical turn `turn_id` admitted under `controller`'s scope (ADR 0105 §1:
/// `(replay key, ordinal)` is the identity). Frames of one logical turn share
/// the scope's journal key, so `turn_id` and `lane` keep each physical turn's
/// lanes distinct.
pub(in crate::runtime) fn turn_observation_cursor(
    scoped_effect_controller: &ActorContext,
    turn_id: &TurnId,
    lane: &str,
) -> crate::engine::ObservationCursor {
    let execution_scope = scoped_effect_controller.execution_scope();
    debug_assert!(
        execution_scope.journal_identity().is_ok(),
        "turn observation lanes require the scope's journal identity, but scope `{}` names none",
        execution_scope.id(),
    );
    let scope = execution_scope
        .journal_identity()
        .map(|identity| identity.key().to_owned())
        .unwrap_or_else(|_| format!("turn:{}", execution_scope.id()));
    crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new(format!(
        "{scope}:{turn_id}:{lane}"
    )))
}

pub(in crate::runtime) fn emit_turn_started(
    observer: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    turn_id: &TurnId,
) {
    cursor.observe(
        &observer.for_turn(turn_id),
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        },
    );
}

pub(in crate::runtime) fn emit_queued_work_started(
    observer: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    turn_id: &TurnId,
    boundary: crate::AdmissionBoundary,
    queued: &crate::AdmittedQueuedWork,
    causes: Vec<crate::TurnCause>,
) {
    cursor.observe(
        &observer.for_turn(turn_id),
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::QueuedWorkStarted {
                boundary,
                batch_ids: queued_work_batch_ids(queued)
                    .into_iter()
                    .map(crate::BatchId::into_inner)
                    .collect(),
                causes,
            },
        },
    );
}

pub(in crate::runtime) fn send_queued_work_started_event(
    event_tx: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    boundary: crate::AdmissionBoundary,
    queued: &crate::AdmittedQueuedWork,
    causes: Vec<crate::TurnCause>,
) {
    cursor.observe(
        event_tx,
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::QueuedWorkStarted {
                boundary,
                batch_ids: queued_work_batch_ids(queued)
                    .into_iter()
                    .map(crate::BatchId::into_inner)
                    .collect(),
                causes,
            },
        },
    );
}

/// [`LashRuntime::max_context_tokens`] of `state`.
pub(super) fn max_context_tokens_of(
    state: &crate::RuntimeSessionState,
) -> Result<usize, RuntimeError> {
    state
        .effective_policy()
        .context_window_tokens()
        .ok_or_else(|| {
            crate::runtime::turn_config::llm_profile_unconfigured(
                crate::SessionError::LlmProfileUnconfigured {
                    session_id: state.session_id.clone(),
                },
            )
        })
}

impl LashRuntime {
    /// The recorded prompt budget queued-run admission measures against.
    /// A session whose recorded config selects no model has none, and its
    /// runs are refused: no deployment can run them.
    pub(super) fn max_context_tokens(&self) -> Result<usize, RuntimeError> {
        max_context_tokens_of(&self.state)
    }

    /// The host's queued-work batching over this session's model: the
    /// policy its next idle admission of next-turn input composes under
    /// (ADR 0101 §5.2). `None` for a session that selects no model, whose
    /// run is refused whatever it takes.
    pub(in crate::runtime) fn input_admission(
        &self,
    ) -> Option<crate::runtime::durable::session_mail::InputAdmission> {
        let max_context_tokens = self.max_context_tokens().ok()?;
        let batching = &self.host.core.durability.queued_work_batching;
        Some(crate::runtime::durable::session_mail::InputAdmission {
            max_inputs: batching.max_turn_input_admission(),
            policy: batching.admission_policy(max_context_tokens),
        })
    }

    /// Install explicitly unstable internal instrumentation for this runtime.
    #[doc(hidden)]
    pub fn set_turn_phase_probe(&mut self, probe: Arc<dyn RuntimeTurnPhaseProbe>) {
        self.host.core.turn_phase_probes.set_for_scope(
            &crate::SessionScope::new(self.state.session_id.clone()),
            Arc::clone(&probe),
        );
        self.turn_phase_probe = Some(probe);
    }

    #[doc(hidden)]
    pub fn set_turn_phase_probe_if_changed(
        &mut self,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) -> bool {
        let changed = self
            .turn_phase_probe
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &probe));
        self.set_turn_phase_probe(probe);
        changed
    }

    fn mark_phase_begin(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.begin(phase);
        }
    }

    fn mark_phase_end(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.end(phase);
        }
    }
}

async fn emit_turn_activity_to_sink(events: &dyn TurnActivitySink, activity: TurnActivity) {
    if !events.is_noop() {
        events.emit(activity).await;
    }
}

async fn emit_turn_activity_to_sink_for_turn(
    events: &dyn TurnActivitySink,
    turn_id: &TurnId,
    activity: TurnActivity,
) {
    if !events.is_noop() {
        events.emit_for_turn(turn_id, activity).await;
    }
}

/// Publish one observation to its host sink, addressing an activity to its
/// physical turn when it has one.
pub(in crate::runtime) async fn publish_observation(
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
    observation: Observation,
) {
    match observation.event {
        RuntimeStreamEvent::Session(event) => emit_session_event_to_sink(events, event).await,
        RuntimeStreamEvent::Turn(activity) => match observation.turn {
            Some(turn_id) => {
                emit_turn_activity_to_sink_for_turn(turn_events, &turn_id, activity).await;
            }
            None => emit_turn_activity_to_sink(turn_events, activity).await,
        },
    }
}
