use super::*;

pub(super) fn process_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            terminal: Some(lash_core::ProcessTerminalSpec {
                status: lash_core::ProcessStatus::Completed,
                await_output: Some(lash_core::ProcessValueSelector::Pointer(
                    "/await_output".to_string(),
                )),
            }),
            wake: Some(lash_core::ProcessWakeSpec {
                when: None,
                input: lash_core::ProcessValueSelector::Pointer("/text".to_string()),
            }),
        },
    }
}

pub(super) fn process_record(process_id: &ProcessId) -> lash_core::ProcessRecord {
    let registration = lash_core::ProcessRegistration::new(
        process_id,
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host().with_caused_by(Some(
            lash_core::CausalRef::TriggerOccurrence {
                occurrence_id: "trigger:1".to_string(),
                subscription_id: None,
                subscription_incarnation: None,
                subscription_revision: None,
            },
        )),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_event_types([process_event_type()])
    .with_wake_session_id(Some(SessionId::from("session-a".to_string())));
    let mut record = lash_core::ProcessRecord::from_registration(
        registration,
        lash_core::ProcessIncarnation::from_registration_sequence(1),
    );
    record.external_ref = Some(lash_core::ProcessExternalRef {
        backend: "worker".to_string(),
        id: "external:1".to_string(),
        metadata: Some(serde_json::json!({ "queue": "default" })),
    });
    record.wait = Some(lash_core::WaitState {
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("process:{process_id}:signal.ready:1"),
            ordinal: 1,
        },
        since_ms: 10,
    });
    record
}

pub(super) fn process_event(process_id: &ProcessId) -> lash_core::ProcessEvent {
    lash_core::ProcessEvent {
        process_id: ProcessId::from(process_id.to_string()),
        process_incarnation: lash_core::ProcessIncarnation::from_registration_sequence(1),
        sequence: 1,
        event_type: "process.completed".to_string(),
        payload: serde_json::json!({ "await_output": { "type": "success", "value": true } }),
        invocation: lash_core::RuntimeInvocation::effect(
            lash_core::EffectAddress::new(
                lash_core::ExecutionScope::turn("session-a", "turn-a"),
                "replay:1",
            )
            .expect("valid test effect address"),
            lash_core::RuntimeAttribution::for_turn("session-a", "turn-a", 1, 0),
            "effect:1",
        )
        .with_caused_by(Some(lash_core::CausalRef::Process {
            process_id: ProcessId::from(process_id.to_string()),
        })),
        semantics: lash_core::runtime::ProcessEventSemantics {
            terminal: Some(lash_core::facade_support::ProcessTerminalSemantics {
                status: lash_core::ProcessStatus::Completed,
                outcome: lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::json!(true)),
                ),
            }),
            wake: Some(lash_core::facade_support::ProcessWake {
                input: "wake".to_string(),
            }),
        },
        occurred_at: 12,
    }
}

pub(super) fn observed_process() -> lash_core::facade_support::ObservedProcess {
    lash_core::facade_support::ObservedProcess {
        process_id: ProcessId::from("process:observed"),
        incarnation: lash_core::ProcessIncarnation::from_registration_sequence(1),
        last_event_sequence: 0,
        graph_key: "process:process:observed:incarnation:1".to_string(),
        kind: "external".to_string(),
        identity: lash_core::ProcessIdentity::new("external")
            .with_label(Some("External".to_string())),
        lifecycle: lash_core::ProcessStatus::Running,
        status_label: "running".to_string(),
        terminal: false,
        disposition: lash_core::RecoveryContract::ExternallyOwned,
        error: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        first_started: None,
        lease_holder: None,
        lease_expires_at_ms: None,
        abandon_request: None,
        cancel_request: None,
        input: lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        originator: lash_core::ProcessOriginator::host(),
        env_ref: None,
        caused_by: None,
        external_ref: None,
        wait: None,
        child_session_id: None,
        label: "External".to_string(),
    }
}

pub(super) fn observed_work_item() -> lash_core::facade_support::ObservedWorkItem {
    lash_core::facade_support::ObservedWorkItem {
        process: observed_process(),
        events: Vec::new(),
        event_tail_sequence: 0,
        state: lash_core::facade_support::ObservedWorkItemState::Coherent,
        kind: "external".to_string(),
        label: "External".to_string(),
    }
}
