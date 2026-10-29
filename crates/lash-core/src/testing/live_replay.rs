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
/// fact it states, as the node it names.
pub fn process_language_observation(
    process_id: &lash_vocabulary::identity::ProcessId,
    event_key: &str,
    label: &str,
) -> crate::LanguageExecutionObservation {
    crate::LanguageExecutionObservation {
        language: Some("fixture".to_string()),
        execution: lash_trace::TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: lash_trace::TraceLanguageExecutionIdentity {
                scope: lash_trace::TraceRuntimeScope::none(),
                document: lash_trace::TraceExecutionCode::Start(
                    lash_kernel_value::names::LinkedStart {
                        link: lash_kernel_value::content_ids::LinkId::from_bytes([0x5a; 32]),
                        selector: lash_kernel_value::names::StartSelector::Export(
                            lash_kernel_value::names::Name::new("fixture"),
                        ),
                    },
                ),
                entry_name: "fixture".to_string(),
                engine_execution_id: None,
                generation: None,
            },
            payload: lash_trace::TraceLanguageExecutionPayload::Node {
                subject: lash_trace::TraceRuntimeSubject::Process {
                    process_id: process_id.clone(),
                },
                at: lash_protocol_types::effect_identity_fixture(label, 1),
                fact: lash_trace::TraceNodeFact::Started { call_id: None },
            },
        },
        observed_at_ms: 0,
    }
}

/// The label a fixture site was built from
/// ([`lash_protocol_types::effect_identity_fixture`]).
fn fixture_label(site: &lash_kernel_value::sites::PositionLabel) -> String {
    site.callee_label()
        .map_or_else(|| site.to_string(), str::to_owned)
}

/// The label [`process_language_observation`] gave a process observation
/// event, or a description of any other payload.
pub fn process_observation_label(event: &crate::ProcessObservationEvent) -> String {
    match &event.payload {
        crate::ProcessObservationEventPayload::Activity { event_key, .. } => {
            format!("activity:{event_key}")
        }
        crate::ProcessObservationEventPayload::LanguageExecution(observation) => {
            match &observation.execution.payload {
                lash_trace::TraceLanguageExecutionPayload::Node {
                    at,
                    fact: lash_trace::TraceNodeFact::Started { .. },
                    ..
                } => fixture_label(&at.position()),
                other => format!("language:{other:?}"),
            }
        }
        crate::ProcessObservationEventPayload::StepBodyStarted(observation) => {
            format!(
                "step body {} attempt {}",
                fixture_label(&observation.step.at.position()),
                observation.step.attempt
            )
        }
        crate::ProcessObservationEventPayload::Committed { event } => {
            format!("committed:{}", event.sequence)
        }
    }
}

/// The start of attempt `attempt` of an admitted step body of `process`, at
/// the first occurrence of the node `label`, for replay tests.
pub fn process_step_body_started(
    process: &lash_vocabulary::identity::ProcessId,
    label: &str,
    attempt: u32,
    observed_at_ms: u64,
) -> lash_trace::StepBodyStartedObservation {
    lash_trace::StepBodyStartedObservation {
        step: lash_trace::StepBodyStarted {
            process_id: process.clone(),
            origin: (lash_protocol_types::effect_identity_fixture(label, 1))
                .position()
                .site()
                .clone(),
            at: lash_protocol_types::effect_identity_fixture(label, 1),
            call_id: crate::ToolCallId::fixture(label),
            attempt,
        },
        observed_at_ms,
    }
}

/// A committed fact of a process at `sequence` for replay tests: a cancel
/// request by `requester`.
pub fn process_committed_event(
    sequence: u64,
    requester: &str,
) -> lash_execution_contracts::process::ObservedProcessEvent {
    lash_execution_contracts::process::ObservedProcessEvent {
        sequence,
        fact: lash_execution_contracts::process::ProcessLifecycleFact::CancelRequested(
            crate::CancelRequest::new(crate::CancelOrigin::OperatorRequested, requester, sequence),
        ),
        occurred_at_ms: sequence,
    }
}

/// A session-cell language observation for the session replay identity law.
pub fn session_language_observation(
    session_id: &lash_kernel_value::identity::SessionId,
    event_key: &str,
    label: &str,
) -> crate::LanguageExecutionObservation {
    let mut observation = process_language_observation(
        &lash_vocabulary::identity::ProcessId::fixture("session-cell"),
        event_key,
        label,
    );
    let subject = lash_trace::TraceRuntimeSubject::Effect {
        address: lash_protocol_types::EffectAddress::new(
            lash_protocol_types::ExecutionScope::session_operation(session_id.clone(), "cell"),
            "cell-execution",
        )
        .expect("fixture effect address"),
        effect_id: "cell".into(),
    };
    if let lash_trace::TraceLanguageExecutionPayload::Node {
        subject: current, ..
    } = &mut observation.execution.payload
    {
        *current = subject;
    }
    if let lash_trace::TraceExecutionCode::Start(start) =
        &mut observation.execution.identity.document
    {
        start.selector = lash_kernel_value::names::StartSelector::Main;
    }
    observation
}
