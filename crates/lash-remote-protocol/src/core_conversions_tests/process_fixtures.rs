use super::*;

pub(super) fn process_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: lash_core::JsonSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            terminal: Some(lash_core::ProcessTerminalSpec {
                status: lash_core::TerminalProcessStatus::Completed,
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
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        lash_core::ProcessProvenance::host().with_caused_by(Some(
            lash_core::CausalRef::TriggerOccurrence {
                occurrence_id: "trigger:1".to_string(),
                subscription_id: None,
                subscription_incarnation: None,
                subscription_revision: None,
            },
        )),
        lash_core::Lifetime::Detached,
    )
    .with_event_types([process_event_type()])
    .with_wake_session_id(Some(SessionId::from("session-a")));
    let mut record = lash_core::ProcessRecord::from_registration(registration, process_id.clone());
    record.external_ref = Some(lash_core::ProcessExternalRef {
        backend: "worker".to_string(),
        id: "external:1".to_string(),
        metadata: Some(serde_json::json!({ "queue": "default" })),
        segment_ordinal: None,
    });
    record.lifecycle = lash_core::ProcessLifecycleState::Waiting {
        wait: lash_core::WaitState {
            kind: lash_core::WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: lash_core::runtime::process_signal_wait_key(process_id, "ready", 1),
                ordinal: 1,
            },
            since_ms: 10,
        },
        park: None,
    };
    record
}

pub(super) fn process_event(process_id: &ProcessId) -> lash_core::ProcessEvent {
    lash_core::ProcessEvent {
        process_id: process_id.clone(),
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
            process_id: process_id.clone(),
        })),
        semantics: lash_core::runtime::ProcessEventSemantics {
            terminal: Some(lash_core::facade_support::ProcessTerminalSemantics {
                outcome: lash_core::ProcessTerminal::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::json!(true)),
                ),
            }),
            wake: Some(lash_core::facade_support::ProcessWake {
                input: "wake".to_string(),
            }),
            signal_wait: None,
        },
        occurred_at: 12,
    }
}

pub(super) fn observed_process() -> lash_core::facade_support::ObservedProcess {
    lash_core::facade_support::ObservedProcess {
        process_id: lash_sansio::ProcessId::fixture("process:observed"),
        last_event_sequence: 0,
        identity: lash_core::ProcessIdentity::labelled("external", Some("External".to_string())),
        lifecycle: lash_core::ProcessStatus::Running,
        lifetime: lash_core::LifetimeDecision::Detached,
        ancestry: lash_core::Ancestry::root(),
        error: None,
        error_code: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        first_started: None,
        cancel_request: None,
        input: lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        originator: lash_core::ProcessOriginator::host(),
        env_ref: None,
        caused_by: None,
        external_ref: None,
        wait: None,
        park: None,
        child_session_id: None,
    }
}

pub(super) fn observed_work_item() -> lash_core::facade_support::ObservedWorkItem {
    lash_core::facade_support::ObservedWorkItem {
        process: observed_process(),
        events: Vec::new(),
    }
}

pub(super) fn process_definition_identity(process_name: &str) -> serde_json::Value {
    serde_json::json!({
        "module_ref": "lashlang:v2:blake3:module",
        "host_requirements_ref": "lashlang-host-requirements:v1:sha256:host",
        "process_id": {
            "component": "process-component",
            "pos": 1
        },
        "process_name": process_name
    })
}

pub(super) fn engine_process_input(
    process_name: &str,
    args: serde_json::Value,
) -> lash_core::ProcessInput {
    let _ = process_name;
    lash_core::ProcessInput::Engine {
        kind: "lashlang".to_string(),
        payload: serde_json::json!({
            "args": args
        }),
    }
}

pub(super) fn engine_process_identity(process_name: &str) -> lash_core::ProcessIdentity {
    lash_core::ProcessIdentity::for_definition(
        lash_core::ProcessDefinitionRef::unclaimed(
            "lashlang",
            process_definition_identity(process_name),
        ),
        Some(process_name.to_string()),
    )
}

pub(super) fn trigger_definition_ids() -> [Option<lash_sansio::ProcessDefinitionId>; 3] {
    [
        None,
        Some(lash_sansio::ProcessDefinitionId::from_sha256_digest(
            [17; 32],
        )),
        Some(lash_sansio::ProcessDefinitionId::from_sha256_digest(
            [23; 32],
        )),
    ]
}

pub(super) fn assert_trigger_identity_projection(
    original: &lash_core::ProcessIdentity,
    remote: &RemoteProcessIdentity,
) -> lash_core::ProcessIdentity {
    let persisted = serde_json::to_value(original).expect("persisted identity json");
    assert!(original.definition.is_some());
    assert_eq!(
        persisted.get("definition"),
        Some(&serde_json::to_value(&original.definition).expect("admitted definition json"))
    );
    assert_eq!(
        serde_json::from_value::<lash_core::ProcessIdentity>(persisted)
            .expect("restore persisted identity"),
        *original
    );

    let mut expected_wire = serde_json::json!({
        "kind": original.kind,
        "label": original.label,
    });
    if let Some(id) = &original.definition_id {
        expected_wire["definition_id"] = serde_json::to_value(id).expect("definition ID json");
    }
    assert_eq!(
        serde_json::to_value(remote).expect("wire identity json"),
        expected_wire
    );

    let mut projected = original.clone();
    projected.definition = None;
    projected
}
