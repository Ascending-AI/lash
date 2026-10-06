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
    let registration = lash_core::testing::held_engine_registration(
        serde_json::json!({ "label": "Held" }),
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
        input: lash_core::testing::held_engine_input(serde_json::json!({ "label": "Held" })),
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
