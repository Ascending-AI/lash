use super::{
    ProcessEventAppendPlan, prepare_process_event_append, prepare_process_registration,
    process_registration_fingerprint, validate_process_registration,
};
use crate::{
    AbandonRequest, ProcessEventAppendRequest, ProcessExternalRef, ProcessInput, ProcessProvenance,
    ProcessRecord, ProcessRegistration, ProcessStarted, RecoveryContract, WaitKind, WaitState,
};

fn fixture_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
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
            session_id: "s".to_string(),
            turn_id: "t".to_string(),
        },
        crate::CausalRef::Effect {
            session_id: "s".to_string(),
            turn_id: None,
            effect_id: "e".to_string(),
        },
        crate::CausalRef::ToolCall {
            session_id: "s".to_string(),
            call_id: "c".to_string(),
        },
        crate::CausalRef::Process {
            process_id: "p".to_string(),
        },
        crate::CausalRef::ProcessEvent {
            process_id: "p".to_string(),
            sequence: 0,
        },
        crate::CausalRef::TriggerOccurrence {
            occurrence_id: "o".to_string(),
            subscription_id: Some("s".to_string()),
            subscription_incarnation: None,
            subscription_revision: Some(0),
        },
        crate::CausalRef::SessionNode {
            session_id: "s".to_string(),
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
    enriched.wake_session_id = Some("wake".to_string());
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
            let observers = ["ab".to_string(), "a".to_string(), "ab".to_string()];
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
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e01000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c00000000000000107b2269676e6f726564223a747275657d0000000000000000046e756c6c01000000000000000004746f6f6c010000000000000004746f6f6c0001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:293548f74115045e46e127b83cbd5775e422af68b25248e73bc2ec1e4f057941",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e020000000000000006656e67696e6500000000000000107b2269676e6f726564223a747275657d02000000000000000006656e67696e65000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:ae40f9040129a34a4372527d18c436ed609f7e16cc5e0cc49f630adcb2d9a59a",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e030000000000000023726567697374726174696f6e2d676f6c64656e2d73657373696f6e2d7475726e3a7631010300000000000000000c73657373696f6e5f7475726e0100000000000000056368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:e3fb715f1bdb9b700378a7338ca748223a11b9e931f89af669e6df1b1499ffb7",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e03000000000000002b726567697374726174696f6e2d676f6c64656e2d64796e616d69632d73657373696f6e2d7475726e3a763102000000000000000d726573756c745f736368656d610100000000000000117b2274797065223a226f626a656374227d0300000000000000000c73657373696f6e5f7475726e01000000000000000d64796e616d69632d6368696c640001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:c46d8d5a706f35b6f7cc39e31b85e48705c7bb6022d5ad274c313bedec0ed550",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000107b2269676e6f726564223a747275657d0300000000000000000865787465726e616c000001000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:9187a17e026c5eb5edbec62b3fc075d3b762c50c7cf4d8214ecf3d8773dae30e",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100010100000000000000017300000000000000017400000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:eea96452329b6ead21a8de32577cf357cb0f023569941a837682a5aa9b211a58",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c0000010001020000000000000001730000000000000000016500000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:bc22654d63d2b0a59a2ef55155e33fb1cf8628e36d3ceeeb87963008d83cc654",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100010300000000000000017300000000000000016300000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:4109a73ddbb13d69dae9ad5275225c0f735684af9146313cec9e5cd89c1d4be5",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100010400000000000000017000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:6d8d4495233740acfe1495a6215afeb61e464a8f1294b86cadfcb22074fc3703",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c000001000105000000000000000170000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:cf97019331bf3360c1b7d0486523d4eb42d5bb806071d61fdb6b01d124998d4a",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100010600000000000000016f010000000000000001730001000000000000000000000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:4a12555d21bd3db75c594392a7521820c6aa94eadad3e5c43aa3d97e01268ec4",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100010700000000000000017300000000000000016e00000000000000000000000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:00cb1a9a246ebf3473799513aaef6e91fde38206933193e05d037ffdd48af485",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c01010000000000000000000000046b696e64010000000000000003613a620100000000000000405b6e756c6c2c66616c73652c747275652c2d312c302c31383434363734343037333730393535313631352c312e352c22613a62222c5b5d2c7b2278223a307d5d02000000000000000773657373696f6e00010000000000000003656e7601000000000000000477616b65000000000000000100000000000000096170702e6576656e7400000000000000117b2274797065223a226f626a656374227d0103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740300000000000000013000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:c2e8e2f0b322ca4424a66421babc42619c3e50ebf8ba6e27e1fbbd65f0c6be64",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e70726f636573732d726567697374726174696f6e2d646566696e6974696f6e0400000000000000046e756c6c0100000000000000000865787465726e616c00000100000000000000000000000600000000000000087374617475732e30000000000000000474727565010101010000000000000000087374617475732e31000000000000000474727565010201010000000000000000087374617475732e320000000000000004747275650103000000000000000000087374617475732e33000000000000000474727565010401010000000000000000087374617475732e34000000000000000474727565010501010000000000000000087374617475732e350000000000000004747275650106010100000000000000000200000000000000016100000000000000026162",
            "process-registration-definition:v2:sha256:0e4f42e5c43fb319a4c11c1558eff280e8cfa2a8cdc1b71271ba5ac18014f424",
        ),
    ];
    assert_eq!(actual.len(), expected.len());
    for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
        assert_eq!(preimage, expected_preimage);
        assert_eq!(key, expected_key);
    }
}

#[test]
fn replay_route_rotates_process_registration_to_the_current_family_without_moving_v2() {
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
    let legacy = process_registration_fingerprint(&registration, &[]);
    assert!(legacy.starts_with("process-registration-definition:v2:sha256:"));

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
    assert!(routed.starts_with("process-registration-definition:v4:sha256:"));
    assert_ne!(legacy, routed);
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
        "session", "frame-a",
    ));
    let mut second = first.clone();
    second.provenance = crate::ProcessProvenance::session(crate::SessionScope::for_agent_frame(
        "session", "frame-b",
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
