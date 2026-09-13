use super::{
    ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    process_registration_fingerprint, validate_process_registration,
};
use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use crate::{
    AbandonRequest, ProcessEventAppendRequest, ProcessExternalRef, ProcessIncarnation,
    ProcessInput, ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessStarted,
    RecoveryContract, WaitKind, WaitState,
};

fn fixture_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn registration_for_input(input: ProcessInput) -> ProcessRegistration {
    ProcessRegistration::new(
        "lookup-id-is-not-in-the-fingerprint",
        input,
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

#[test]
fn process_registration_identity_golden_corpus() {
    let inputs = [
        ProcessInput::ToolCall {
            call: crate::PreparedToolCall::from_parts(
                "call",
                crate::ToolId::new("tool-id"),
                "tool",
                serde_json::json!({"ignored": true}),
                None,
                serde_json::Value::Null,
            ),
        },
        ProcessInput::Engine {
            kind: "engine".to_string(),
            payload: serde_json::json!({"ignored": true}),
        },
        ProcessInput::SessionTurn {
            definition_key: "registration-golden-session-turn:v1".to_string(),
            create_request: Box::new(
                crate::SessionCreateRequest::root(
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id("child"),
            ),
            turn_input: Box::new(crate::TurnInput::empty()),
            output_contract: crate::ToolOutputContract::Static,
        },
        ProcessInput::SessionTurn {
            definition_key: "registration-golden-dynamic-session-turn:v1".to_string(),
            create_request: Box::new(
                crate::SessionCreateRequest::root(
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id("dynamic-child"),
            ),
            turn_input: Box::new(crate::TurnInput::empty()),
            output_contract: crate::ToolOutputContract::from_input_schema(
                "result_schema",
                Some(serde_json::json!({"type": "object"})),
            ),
        },
        ProcessInput::External {
            metadata: serde_json::json!({"ignored": true}),
        },
    ];
    let causes = [
        crate::CausalRef::Turn {
            session_id: SessionId::from("s"),
            turn_id: TurnId::from("t"),
        },
        crate::CausalRef::Effect {
            address: crate::EffectAddress::new(crate::ExecutionScope::runtime_operation("s"), "e")
                .expect("valid effect cause"),
        },
        crate::CausalRef::ToolCall {
            session_id: SessionId::from("s"),
            call_id: "c".to_string(),
        },
        crate::CausalRef::Process {
            process_id: ProcessId::from("p"),
        },
        crate::CausalRef::ProcessEvent {
            process_id: ProcessId::from("p"),
            sequence: 0,
        },
        crate::CausalRef::TriggerOccurrence {
            occurrence_id: "o".to_string(),
            subscription_id: Some("s".to_string()),
            subscription_incarnation: None,
            subscription_revision: Some(0),
        },
        crate::CausalRef::SessionNode {
            session_id: SessionId::from("s"),
            node_id: "n".to_string(),
        },
    ];
    let mut registrations = inputs
        .into_iter()
        .enumerate()
        .map(|(index, input)| {
            let mut registration = registration_for_input(input);
            registration.disposition = match index {
                0 => RecoveryContract::Rerunnable,
                1 => RecoveryContract::OwnerBound,
                _ => RecoveryContract::ExternallyOwned,
            };
            registration
        })
        .collect::<Vec<_>>();
    registrations.extend(causes.into_iter().map(|cause| {
        let mut registration = registration_for_input(ProcessInput::External {
            metadata: serde_json::Value::Null,
        });
        registration.provenance.caused_by = Some(cause);
        registration
    }));
    let mut enriched = registration_for_input(ProcessInput::External {
        metadata: serde_json::Value::Null,
    });
    enriched.max_attempts = Some(0);
    enriched.identity = crate::ProcessIdentity::new("kind")
        .with_label(Some("a:b"))
        .with_definition(Some(serde_json::json!([
            null, false, true, -1, 0, u64::MAX, 1.5, "a:b", [], {"x": 0}
        ])));
    enriched.provenance.originator =
        crate::ProcessOriginator::session(crate::SessionScope::new("session"));
    enriched.env_ref = Some(crate::ProcessExecutionEnvRef::new("env"));
    enriched.wake_session_id = Some(SessionId::from("wake"));
    let mut selector_fields = std::collections::BTreeMap::new();
    selector_fields.insert(
        "const".to_string(),
        crate::ProcessValueSelector::Const(serde_json::json!(0)),
    );
    selector_fields.insert("payload".to_string(), crate::ProcessValueSelector::Payload);
    selector_fields.insert(
        "pointer".to_string(),
        crate::ProcessValueSelector::Pointer("/x".to_string()),
    );
    selector_fields.insert(
        "present".to_string(),
        crate::ProcessValueSelector::Present("/y".to_string()),
    );
    enriched.event_types = vec![crate::ProcessEventType {
        name: "app.event".to_string(),
        payload_schema: crate::LashSchema::new(serde_json::json!({"type": "object"})),
        semantics: crate::ProcessEventSemanticsSpec {
            terminal: Some(crate::ProcessTerminalSpec {
                status: crate::ProcessStatus::Completed,
                await_output: Some(crate::ProcessValueSelector::Template {
                    template: "{payload}:{pointer}:{const}:{present}".to_string(),
                    fields: selector_fields,
                }),
            }),
            wake: Some(crate::ProcessWakeSpec {
                when: None,
                input: crate::ProcessValueSelector::Payload,
            }),
        },
    }];
    registrations.push(enriched);

    let mut terminal_statuses = registration_for_input(ProcessInput::External {
        metadata: serde_json::Value::Null,
    });
    terminal_statuses.event_types = [
        crate::ProcessStatus::Running,
        crate::ProcessStatus::Waiting,
        crate::ProcessStatus::Completed,
        crate::ProcessStatus::Failed,
        crate::ProcessStatus::Cancelled,
        crate::ProcessStatus::Abandoned,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, status)| crate::ProcessEventType {
        name: format!("status.{index}"),
        payload_schema: crate::LashSchema::new(serde_json::Value::Bool(true)),
        semantics: crate::ProcessEventSemanticsSpec {
            terminal: Some(crate::ProcessTerminalSpec {
                status,
                await_output: (status != crate::ProcessStatus::Completed)
                    .then_some(crate::ProcessValueSelector::Payload),
            }),
            wake: None,
        },
    })
    .collect();
    registrations.push(terminal_statuses);

    let actual = registrations
        .iter()
        .map(|registration| {
            let observers = [
                SessionId::from("ab"),
                SessionId::from("a"),
                SessionId::from("ab"),
            ];
            (
                hex(&super::process_registration_fingerprint_preimage(
                    registration,
                    &observers,
                )),
                process_registration_fingerprint(registration, &observers),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e01000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c00000000000000107b2269676e6f726564223a747275657d0000000000000000046e756c6c010003010000000000000004746f6f6c010000000000000004746f6f6c0001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:6eacdce818234e52319fa20eac824eda75f772cc3d6ba778418acfb694f36f5e",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e020000000000000006656e67696e6500000000000000107b2269676e6f726564223a747275657d020003010000000000000006656e67696e65000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:43a75e0a9d20cb48a721a9fbe26490a927f00c1efd5ad7fa950ea4f84d782083",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e030000000000000023726567697374726174696f6e2d676f6c64656e2d73657373696f6e2d7475726e3a76310103000301000000000000000c73657373696f6e5f7475726e0100000000000000056368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:cd865abe772d75f350ca3d2d149aa3e909c7a8cfc4b250fdce859d51871a5ec6",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e03000000000000002b726567697374726174696f6e2d676f6c64656e2d64796e616d69632d73657373696f6e2d7475726e3a763102000000000000000d726573756c745f736368656d610100000000000000117b2274797065223a226f626a656374227d03000301000000000000000c73657373696f6e5f7475726e01000000000000000d64796e616d69632d6368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:fb6102a9a5f3f8d882686d51a2634d0cdfbe6a4c5f733274f16d131424a9b0a5",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000107b2269676e6f726564223a747275657d03000301000000000000000865787465726e616c000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:00812b5fab28ae40c8f7082c10620ebec8cb62e2764c29e0425882b4174b77e8",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100010100000000000000017300000000000000017400000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:d6bc50e858c4941b816f1bf766f7c4d596a28daa114e26971b315912d9e7d2de",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c0000010001020500000000000000017300000000000000016500000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:24c9d2495d755b871ca946beba4d66ee050058c5190cd615153d39d17eeaad47",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100010300000000000000017300000000000000016300000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:23ab4155c8ff7482fcf5175891e272787a77d443e889d8ebcfbcd045bd46a896",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100010400000000000000017000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:d736bd70d7903e1252add148325b71f282e0798e943cc2bc26127db5120ac18a",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c000001000105000000000000000170000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:fb4430a0fd2e7eb6c577e044565bad7f6f28ad65ef7e9d0d22cd5bfec72cf151",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100010600000000000000016f010000000000000001730001000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:386b9cb9e3e3abf1a26cb16bbf83cda53231314f8b705ee2fe984590789fde84",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100010700000000000000017300000000000000016e00000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:65a5858b7458277429fe67af401e3e27a3fff874f48ef0fd077085e2b567c723",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c010100000000030100000000000000046b696e64010000000000000003613a620100000000000000405b6e756c6c2c66616c73652c747275652c2d312c302c31383434363734343037333730393535313631352c312e352c22613a62222c5b5d2c7b2278223a307d5d02000000000000000773657373696f6e00010000000000000003656e7601000000000000000477616b65000000000000000100000000000000096170702e6576656e7400000000000000117b2274797065223a226f626a656374227d0103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740300000000000000013000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:7877bd6a141400a2245a9a416d4ad86b1f72a1adea241b3a86c0869ef47b2b93",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01000301000000000000000865787465726e616c00000100000000000000000000000600000000000000087374617475732e30000000000000000474727565010101010000000000000000087374617475732e31000000000000000474727565010201010000000000000000087374617475732e320000000000000004747275650103000000000000000000087374617475732e33000000000000000474727565010401010000000000000000087374617475732e34000000000000000474727565010501010000000000000000087374617475732e350000000000000004747275650106010100000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v6:blake3:3f1655953abf729697833d382fafb10c0701e4566008babcdde483e178a07922",
        ),
    ];
    assert_eq!(actual.len(), expected.len());
    for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
        assert_eq!(preimage, expected_preimage);
        assert_eq!(key, expected_key);
    }
}

#[test]
fn replay_route_participates_in_the_current_process_registration_family() {
    let registration = registration_for_input(ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            "call",
            crate::ToolId::new("tool-id"),
            "tool",
            serde_json::json!({"argument": true}),
            Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item".to_string()),
                opaque: Some("opaque".to_string()),
                origin: None,
            }),
            serde_json::Value::Null,
        ),
    });
    let without_route = process_registration_fingerprint(&registration, &[]);
    assert!(without_route.starts_with("process-registration-definition:v6:blake3:"));

    let mut routed = registration;
    let ProcessInput::ToolCall { call } = std::sync::Arc::make_mut(&mut routed.input) else {
        unreachable!()
    };
    call.replay.as_mut().expect("replay").origin =
        Some(lash_sansio::llm::types::ProviderRouteIdentity::new(
            "openai-compatible",
            "https://gateway.example/v1",
            "shared-model",
        ));
    let routed = process_registration_fingerprint(&routed, &[]);
    assert!(routed.starts_with("process-registration-definition:v6:blake3:"));
    assert_ne!(without_route, routed);
}

#[test]
fn process_id_rejects_reserved_segment_separator() {
    let registration = fixture_registration("foo#1");
    let error =
        prepare_process_registration(registration).expect_err("segment separator must be rejected");
    assert!(error.to_string().contains("reserved segment separator `#`"));
}

#[test]
fn producer_cannot_override_runtime_lifecycle_event_types() {
    let mut collision =
        super::runtime_lifecycle_event_type("process.waiting").expect("reserved event type");
    collision.semantics.terminal = Some(crate::ProcessTerminalSpec {
        status: crate::ProcessStatus::Completed,
        await_output: None,
    });
    let registration = fixture_registration("reserved-collision").with_event_types([collision]);
    let error = prepare_process_registration(registration)
        .expect_err("reserved lifecycle collision must be rejected");
    assert!(
        error
            .to_string()
            .contains("reserved runtime lifecycle event type `process.waiting`")
    );
}

#[test]
fn terminal_semantics_reject_non_terminal_status() {
    let registration = fixture_registration("invalid-terminal-status").with_extra_event_types([
        crate::ProcessEventType {
            name: "producer.invalid_terminal".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec {
                terminal: Some(crate::ProcessTerminalSpec {
                    status: crate::ProcessStatus::Running,
                    await_output: Some(crate::ProcessValueSelector::Payload),
                }),
                ..crate::ProcessEventSemanticsSpec::default()
            },
        },
    ]);
    let error = prepare_process_registration(registration)
        .expect_err("non-terminal status must be rejected at registration");
    assert!(
        error
            .to_string()
            .contains("must declare a terminal status, got `running`")
    );
}

#[test]
fn exact_core_defaults_are_excluded_but_core_named_overrides_conflict() {
    let mut without_core_events = fixture_registration("first-lookup-id");
    without_core_events.event_types.clear();
    let with_core_events = prepare_process_registration(fixture_registration("second-lookup-id"))
        .expect("prepare exact core defaults");
    assert_eq!(
        process_registration_fingerprint(&with_core_events, &[]),
        process_registration_fingerprint(&without_core_events, &[])
    );

    let mut overridden = with_core_events.clone();
    let completed = overridden
        .event_types
        .iter_mut()
        .find(|event_type| event_type.name == "process.completed")
        .expect("completed default");
    completed.semantics.terminal = Some(crate::ProcessTerminalSpec {
        status: crate::ProcessStatus::Completed,
        await_output: Some(crate::ProcessValueSelector::Pointer(
            "/hijacked".to_string(),
        )),
    });
    validate_process_registration(&overridden).expect("core-named override remains valid");
    assert_ne!(
        process_registration_fingerprint(&overridden, &[]),
        process_registration_fingerprint(&without_core_events, &[]),
        "a core-named executable override must not false-merge with the default"
    );
}

#[test]
fn tool_call_registration_refuses_empty_call_id_or_tool_name() {
    let empty_call_id = registration_for_input(ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            "  ",
            crate::ToolId::new("tool-id"),
            "tool",
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        ),
    });
    assert!(
        validate_process_registration(&empty_call_id)
            .expect_err("empty call id must be refused")
            .to_string()
            .contains("tool call must carry a call id")
    );

    let empty_tool_name = registration_for_input(ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            "call",
            crate::ToolId::new("tool-id"),
            "\t",
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        ),
    });
    assert!(
        validate_process_registration(&empty_tool_name)
            .expect_err("empty tool name must be refused")
            .to_string()
            .contains("tool call must carry a tool name")
    );
}

#[test]
fn executable_registration_changes_rotate_the_definition_fingerprint() {
    let base = registration_for_input(ProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({"revision": 1}),
    });
    let changed_input = registration_for_input(ProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({"revision": 2}),
    });
    assert_ne!(
        process_registration_fingerprint(&base, &[]),
        process_registration_fingerprint(&changed_input, &[])
    );

    let mut changed_event = base.clone();
    changed_event.event_types = vec![crate::ProcessEventType {
        name: "app.event".to_string(),
        payload_schema: crate::LashSchema::new(serde_json::json!({"type": "string"})),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    }];
    let mut other_event = changed_event.clone();
    other_event.event_types[0].payload_schema =
        crate::LashSchema::new(serde_json::json!({"type": "number"}));
    assert_ne!(
        process_registration_fingerprint(&changed_event, &[]),
        process_registration_fingerprint(&other_event, &[])
    );

    let mut annotated_event = changed_event.clone();
    annotated_event.event_types[0].payload_schema =
        crate::LashSchema::new(serde_json::json!({"type": "string", "title": "display only"}));
    assert_eq!(
        process_registration_fingerprint(&changed_event, &[]),
        process_registration_fingerprint(&annotated_event, &[]),
        "non-executable schema annotations are not definition identity"
    );

    let mut reordered_events = changed_event.clone();
    reordered_events.event_types.push(crate::ProcessEventType {
        name: "app.another".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    });
    let mut opposite_order = reordered_events.clone();
    opposite_order.event_types.reverse();
    assert_eq!(
        process_registration_fingerprint(&reordered_events, &[]),
        process_registration_fingerprint(&opposite_order, &[]),
        "source order is not executable definition"
    );
}

#[test]
fn session_originator_elevation_changes_registration_fingerprint() {
    let mut first = registration_for_input(ProcessInput::External {
        metadata: serde_json::Value::Null,
    });
    first.provenance = crate::ProcessProvenance::session(crate::SessionScope::for_agent_frame(
        "session",
        crate::facade_support::frame_node_id(&SessionId::from("session"), "frame-a"),
    ));
    let mut second = first.clone();
    second.provenance = crate::ProcessProvenance::session(crate::SessionScope::for_agent_frame(
        "session",
        crate::facade_support::frame_node_id(&SessionId::from("session"), "frame-b"),
    ));
    assert_ne!(
        process_registration_fingerprint(&first, &[]),
        process_registration_fingerprint(&second, &[]),
        "elevation is executable wake authority and cannot replay as the same process definition"
    );
}

#[test]
fn session_turn_definition_key_owns_excluded_request_identity() {
    fn session_turn(key: &str, child: &str, prompt: &str) -> ProcessRegistration {
        registration_for_input(ProcessInput::SessionTurn {
            definition_key: key.to_string(),
            create_request: Box::new(
                crate::SessionCreateRequest::root(
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id(child),
            ),
            turn_input: Box::new(crate::TurnInput::text(prompt)),
            output_contract: crate::ToolOutputContract::Static,
        })
    }

    let first = session_turn("caller-definition:v1", "child", "transfer 10");
    let changed_without_rotation = session_turn("caller-definition:v1", "child", "transfer 10000");
    assert_eq!(
        process_registration_fingerprint(&first, &[]),
        process_registration_fingerprint(&changed_without_rotation, &[]),
        "keeping definition_key stable deliberately declares growable inputs identical"
    );
    let rotated = session_turn("caller-definition:v2", "child", "transfer 10000");
    assert_ne!(
        process_registration_fingerprint(&first, &[]),
        process_registration_fingerprint(&rotated, &[])
    );
}

#[test]
fn persisted_record_without_lifecycle_declarations_accepts_runtime_events() {
    let registration = prepare_process_registration(fixture_registration("pre-upgrade-record"))
        .expect("prepare pre-upgrade fixture");
    let registration_fingerprint = process_registration_fingerprint(&registration, &[]);
    assert!(
        registration
            .event_types
            .iter()
            .all(|event_type| !super::is_runtime_lifecycle_event_type(&event_type.name)),
        "runtime lifecycle types must not be persisted as producer declarations"
    );
    let encoded = serde_json::to_vec(&ProcessRecord::from_prepared_registration(
        registration,
        registration_fingerprint,
        ProcessIncarnation::from_registration_sequence(1),
        1,
    ))
    .expect("encode pre-upgrade row");
    let mut record: ProcessRecord =
        serde_json::from_slice(&encoded).expect("decode pre-upgrade row");
    let wait = WaitState {
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:pre-upgrade-record:signal.ready:1".to_string(),
            ordinal: 1,
        },
        since_ms: 2,
    };
    let requests = [
        ProcessEventAppendRequest::first_started(
            &record.id,
            &ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 2,
            },
            false,
        ),
        ProcessEventAppendRequest::wait_entered(&record.id, &wait),
        ProcessEventAppendRequest::wait_cleared(&record.id, &wait),
        ProcessEventAppendRequest::external_ref_set(
            &record.id,
            &ProcessExternalRef {
                backend: "fixture".to_string(),
                id: "external".to_string(),
                metadata: None,
            },
        ),
        ProcessEventAppendRequest::abandon_requested(
            &record.id,
            &AbandonRequest {
                requested_by: "fixture".to_string(),
                requested_at_ms: 3,
                reason: None,
            },
        ),
    ];
    for (index, request) in requests.into_iter().enumerate() {
        let sequence = index as u64 + 1;
        let plan = prepare_process_event_append(
            &record,
            request,
            sequence,
            (sequence > 1).then_some(sequence - 1),
            None,
            sequence + 10,
            None,
        )
        .expect("runtime-owned lifecycle append must validate");
        let ProcessEventAppendPlan::Insert {
            projected_record, ..
        } = plan
        else {
            panic!("unique lifecycle fixture must insert")
        };
        record = projected_record;
    }
    assert!(record.first_started.is_some());
    assert!(record.wait.is_none());
    assert!(record.external_ref.is_some());
    assert!(record.abandon_request.is_some());
}

#[test]
fn host_signal_replay_key_with_fold_validation_suffix_does_not_panic() {
    let registration = fixture_registration("host-signal-fold-validation-key")
        .with_extra_event_types([crate::ProcessEventType {
            name: "signal.ready".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }]);
    let record = ProcessRecord::from_registration(
        registration,
        ProcessIncarnation::from_registration_sequence(1),
    );
    let request = ProcessEventAppendRequest::new(
        "signal.ready",
        serde_json::json!({
            "value": "ready",
        }),
    )
    .with_replay_key("host-supplied:fold-validation");

    let plan = prepare_process_event_append(&record, request, 1, None, None, 42, None)
        .expect("host-supplied signal replay key should retain the existing append contract");
    assert!(matches!(plan, ProcessEventAppendPlan::Insert { .. }));
}

#[test]
fn lifecycle_and_resolved_attempts_are_registration_identity() {
    let mut registration = fixture_registration("lifecycle-fingerprint");
    let baseline = process_registration_fingerprint(&registration, &[]);
    registration.max_attempts = Some(3);
    let bounded = process_registration_fingerprint(&registration, &[]);
    assert_ne!(baseline, bounded);
    registration.lifecycle.parent = crate::ParentScope::Process {
        process_id: crate::ProcessId::from("parent"),
        incarnation: crate::ProcessIncarnation::from_registration_sequence(7),
    };
    let parent_scoped = process_registration_fingerprint(&registration, &[]);
    assert_ne!(bounded, parent_scoped);
    registration.lifecycle.on_parent_end = crate::OnParentEnd::Cancel;
    let cancel = process_registration_fingerprint(&registration, &[]);
    assert_ne!(parent_scoped, cancel);
    registration.lifecycle.parent = crate::ParentScope::Process {
        process_id: crate::ProcessId::from("parent"),
        incarnation: crate::ProcessIncarnation::from_registration_sequence(8),
    };
    assert_ne!(cancel, process_registration_fingerprint(&registration, &[]));
}
