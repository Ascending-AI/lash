use std::sync::Arc;

use crate::{InMemoryLiveReplayStore, SessionObservationEvent};

impl InMemoryLiveReplayStore {
    /// Reopen this process-local observation buffer over the same preserved replay history.
    ///
    /// The returned handle deliberately keeps both the replay incarnation and
    /// shared event buffers. Test and conformance code must construct a fresh
    /// store instead when it cannot prove that both survived restart.
    pub fn reopen_preserving_history(&self) -> Self {
        self.clone_preserving_history()
    }

    /// Install a gate between replay visibility and subscriber notification.
    pub fn with_before_notification_gate_for_testing(
        self,
        gate: impl Fn(&[Arc<SessionObservationEvent>]) + Send + Sync + 'static,
    ) -> Self {
        self.with_before_notification_gate(gate)
    }
}

impl crate::InMemoryProcessReplayStore {
    /// Reopen this process-local buffer over the same preserved replay
    /// history, as [`InMemoryLiveReplayStore::reopen_preserving_history`]
    /// does.
    pub fn reopen_preserving_history(&self) -> Self {
        self.clone_preserving_history()
    }
}

/// A provisional language observation of `process_id` for replay tests: one
/// started node. `event_key` is its redelivery identity and `label` the
/// fact it states.
pub fn process_language_observation(
    process_id: &crate::ProcessId,
    event_key: &str,
    label: &str,
) -> crate::LanguageExecutionObservation {
    crate::LanguageExecutionObservation {
        language: "fixture".to_string(),
        execution: lash_trace::TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: lash_trace::TraceLanguageExecutionIdentity {
                scope: lash_trace::TraceRuntimeScope::none(),
                subject: lash_trace::TraceRuntimeSubject::Process {
                    process_id: process_id.clone(),
                },
                source_identity: "fixture-source".to_string(),
                module_ref: "fixture-module".to_string(),
                entry_kind: "process".to_string(),
                entry_ref: None,
                entry_name: "fixture".to_string(),
                engine_execution_id: None,
                generation: None,
            },
            payload: lash_trace::TraceLanguageExecutionPayload::NodeStarted {
                node_id: "fixture-node".to_string(),
                node_kind: lash_sansio::ExecutionNodeKind::Call,
                label: label.to_string(),
                occurrence: 0,
                call_id: None,
            },
        },
        observed_at_ms: 0,
    }
}

/// The label [`process_language_observation`] gave a process observation
/// event, or a description of any other payload.
pub fn process_observation_label(event: &crate::ProcessObservationEvent) -> String {
    match &event.payload {
        crate::ProcessObservationEventPayload::LanguageExecution(observation) => match &observation
            .execution
            .payload
        {
            lash_trace::TraceLanguageExecutionPayload::NodeStarted { label, .. } => label.clone(),
            other => format!("language:{other:?}"),
        },
        crate::ProcessObservationEventPayload::Committed { event } => {
            format!("committed:{}", event.sequence)
        }
    }
}

/// A committed fact of a process at `sequence` for replay tests: a cancel
/// request by `requester`.
pub fn process_committed_event(
    sequence: u64,
    requester: &str,
) -> crate::runtime::ObservedProcessEvent {
    crate::runtime::ObservedProcessEvent {
        sequence,
        fact: crate::ProcessLifecycleFact::CancelRequested(crate::CancelRequest::new(
            crate::CancelOrigin::OperatorRequested,
            requester,
            sequence,
        )),
        occurred_at_ms: sequence,
    }
}

/// A session-cell language observation for the session replay identity law.
pub fn session_language_observation(
    session_id: &crate::SessionId,
    event_key: &str,
    label: &str,
) -> crate::LanguageExecutionObservation {
    let mut observation =
        process_language_observation(&crate::ProcessId::fixture("session-cell"), event_key, label);
    observation.execution.identity.subject = lash_trace::TraceRuntimeSubject::Effect {
        address: lash_sansio::EffectAddress::new(
            lash_sansio::ExecutionScope::session_operation(session_id.clone(), "cell"),
            "cell-execution",
        )
        .expect("fixture effect address"),
        effect_id: "cell".into(),
    };
    observation.execution.identity.entry_kind = "cell".into();
    observation
}
