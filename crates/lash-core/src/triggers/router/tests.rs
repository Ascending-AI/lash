use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::Mutex;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn residual_trigger_projection_identity_goldens() {
    let source = serde_json::json!({"b": [1, true], "a": "λ"});
    assert_eq!(
        hex(&trigger_source_preimage("webhook\0type", &source)),
        "6c6173682d737461626c652d6964656e74697479020100000000000000136c6173682e747269676765722d736f75726365000000000000000c776562686f6f6b007479706500000000000000177b2261223a22cebb222c2262223a5b312c747275655d7d"
    );
    assert_eq!(
        default_trigger_source_key("webhook\0type", &source),
        "trigger-source:v1:blake3:c8840de389d5af3008240cb4a75277c96a958037c9a7249e7e2a184400c7cfb0"
    );

    assert_eq!(
        hex(&trigger_delivery_process_preimage(
            "trigger:key:a:b",
            "subscription\0x",
            "inc:λ",
            42,
        )),
        "6c6173682d737461626c652d6964656e746974790201000000000000001d6c6173682e747269676765722d64656c69766572792d70726f63657373000000000000000f747269676765723a6b65793a613a62000000000000000e737562736372697074696f6e00780000000000000006696e633acebb000000000000002a"
    );
    assert_eq!(
        deterministic_delivery_process_id("trigger:key:a:b", "subscription\0x", "inc:λ", 42,)
            .unwrap(),
        "process:trigger-delivery:v1:blake3:7ff0a51d9a9d0e1e854502116f8b2d9a1b467b0169a5ad7b1cbc4b74e87e2919"
    );
    assert_eq!(
        derived_trigger_subscription_key("worker\0name", "source:λ", "key\0route"),
        "derived/v3/5737eff4c14bed2a7dc7e7eb4a68c5b9968dd56a4aac8cffcfb9e0f64436d443"
    );
}

#[test]
fn occurrence_uses_idempotency_key_and_structural_conflict_material() {
    let request = TriggerOccurrenceRequest::new(
        "source",
        "key",
        serde_json::json!({"value": 1}),
        "caller:key",
    )
    .with_source(serde_json::json!({"origin": true}));
    assert_eq!(deterministic_occurrence_id(&request), "trigger:caller:key");
    let record = TriggerOccurrenceRecord {
        occurrence_id: "trigger:caller:key".to_string(),
        source_type: request.source_type.clone(),
        source_key: request.source_key.clone(),
        payload: request.payload.clone(),
        idempotency_key: request.idempotency_key.clone(),
        source: request.source.clone(),
        session_id: None,
        outcome: TriggerOccurrenceOutcome::Fired,
        occurred_at_ms: 42,
    };
    assert_eq!(
        serde_json::to_string(&request).expect("serialize fired occurrence request"),
        r#"{"source_type":"source","source_key":"key","payload":{"value":1},"idempotency_key":"caller:key","source":{"origin":true}}"#,
        "the default fired request must stay byte-for-byte stable"
    );
    assert_eq!(
        serde_json::to_string(&record).expect("serialize fired occurrence record"),
        r#"{"occurrence_id":"trigger:caller:key","source_type":"source","source_key":"key","payload":{"value":1},"idempotency_key":"caller:key","source":{"origin":true},"occurred_at_ms":42}"#,
        "the default fired record must stay byte-for-byte stable"
    );
    assert_eq!(
        serde_json::from_str::<TriggerOccurrenceRecord>(
            r#"{"occurrence_id":"trigger:caller:key","source_type":"source","source_key":"key","payload":{"value":1},"idempotency_key":"caller:key","source":{"origin":true},"occurred_at_ms":42}"#,
        )
        .expect("decode a pre-outcome occurrence record")
        .outcome,
        TriggerOccurrenceOutcome::Fired,
        "records written before the outcome field must decode as fired"
    );
    assert!(trigger_occurrence_request_matches_record(&request, &record));
    let mut normalized = request.clone();
    normalized.payload = serde_json::json!({"value": -0.0});
    let mut normalized_record = record.clone();
    normalized_record.payload = serde_json::json!({"value": 0.0});
    normalized.source = Some(serde_json::Value::Null);
    normalized_record.source = None;
    assert!(trigger_occurrence_request_matches_record(
        &normalized,
        &normalized_record
    ));
    let mut changed = request;
    changed.payload = serde_json::json!({"value": 2});
    assert!(!trigger_occurrence_request_matches_record(
        &changed, &record
    ));
}

fn minimal_identity_corpus_draft(input: crate::ProcessInput) -> TriggerSubscriptionDraft {
    TriggerSubscriptionDraft::for_process(
        "sub",
        crate::ProcessExecutionEnvRef::new("env"),
        "source",
        "key",
        input,
        crate::ProcessIdentity::new("kind"),
    )
}

fn enriched_identity_corpus_draft(input: crate::ProcessInput) -> TriggerSubscriptionDraft {
    let mut bindings = BTreeMap::new();
    bindings.insert("event".to_string(), TriggerInputBinding::Event);
    bindings.insert(
        "fixed".to_string(),
        TriggerInputBinding::Fixed {
            value: serde_json::json!([null, false, true, -1, 0, u64::MAX, 1.5, "a:b", [], {"x": 0}]),
        },
    );
    let mut selector_fields = BTreeMap::new();
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
    let mut draft = minimal_identity_corpus_draft(input)
        .with_source(serde_json::json!({"source": [0, "0"]}))
        .with_payload_schema(crate::LashSchema::new(
            serde_json::json!({"type": "object"}),
        ))
        .with_wake_target(crate::SessionScope::for_agent_frame(
            "session",
            crate::FrameNodeId::new("frame").expect("test frame identity is non-empty"),
        ))
        .with_event_types([crate::ProcessEventType {
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
        }])
        .with_input_template(bindings)
        .with_name("name")
        .with_target_label("label");
    draft.target_identity = crate::ProcessIdentity::for_definition(
        crate::ProcessDefinitionRef::unclaimed("kind", serde_json::json!({"definition": 0})),
        Some("label"),
    );
    draft
}

#[test]
fn trigger_definition_identity_golden_corpus() {
    let tool = crate::ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            "call",
            crate::ToolId::new("tool-id"),
            "tool",
            serde_json::json!({"arg": 0}),
            Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item".to_string()),
                opaque: None,
                ..Default::default()
            }),
            serde_json::json!({"prepared": true}),
        ),
    };
    let inputs = [
        tool,
        crate::ProcessInput::Engine {
            kind: "engine".to_string(),
            payload: serde_json::json!({"payload": 0}),
        },
        crate::ProcessInput::SessionTurn {
            definition_key: "golden-session-turn:v1".to_string(),
            create_request: Box::new(crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )),
            turn_input: Box::new(crate::TurnInput::empty()),
            output_contract: crate::ToolOutputContract::FromInputSchema {
                input_field: "field".to_string(),
                default_schema: Some(serde_json::json!({})),
            },
        },
        crate::ProcessInput::External {
            metadata: serde_json::json!({"metadata": 0}),
        },
        crate::ProcessInput::SessionTurn {
            definition_key: "golden-session-turn-static:v1".to_string(),
            create_request: Box::new(crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )),
            turn_input: Box::new(crate::TurnInput::empty()),
            output_contract: crate::ToolOutputContract::Static,
        },
    ];
    let owners = [
        TriggerOwnerScope::session("owner"),
        TriggerOwnerScope::host("owner").expect("host owner"),
        TriggerOwnerScope::Platform,
        TriggerOwnerScope::session("owner"),
        TriggerOwnerScope::host("static-owner").expect("host owner"),
    ];
    let actual = owners
        .iter()
        .zip(inputs)
        .enumerate()
        .map(|(index, (owner, input))| {
            let draft = if index == 0 {
                enriched_identity_corpus_draft(input)
            } else {
                minimal_identity_corpus_draft(input)
            };
            (
                hex(&trigger_subscription_definition_preimage(owner, &draft)),
                trigger_subscription_definition_fingerprint(owner, &draft),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7601000000000000000773657373696f6e0100000000000000056672616d650100000000000000046e616d650000000000000006736f7572636500000000000000036b657900000000000000127b22736f75726365223a5b302c2230225d7d00000000000000117b2274797065223a226f626a656374227d000000000000000000000000000000027b7d0101000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c00000000000000097b22617267223a307d010100000000000000046974656d0000000000000000117b227072657061726564223a747275657d00000000000000046b696e640100000000000000056c6162656c0100000000000000107b22646566696e6974696f6e223a307d000000000000000100000000000000096170702e6576656e7400000000000000117b2274797065223a226f626a656374227d0103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740300000000000000013000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000056576656e7401000000000000000566697865640200000000000000405b6e756c6c2c66616c73652c747275652c2d312c302c31383434363734343037333730393535313631352c312e352c22613a62222c5b5d2c7b2278223a307d5d0100000000000000056c6162656c",
            "trigger-definition:v3:blake3:5e263c2841a7c380bf10f4d89b9be4920b148011e582f6228f5b4fd4abe8d195",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0200000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000d7b227061796c6f6164223a307d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:a19efb9486669ed2268b20762592b63d3a8f166d49d03c81a9e4603904849e58",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0300000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01030000000000000016676f6c64656e2d73657373696f6e2d7475726e3a76310200000000000000056669656c640100000000000000027b7d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:1aebd58a1ec81a02e792795266b8eb89e5e537eabb606b3ddb7c1a663414875a",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d0104000000000000000e7b226d65746164617461223a307d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:0338f0ecb430796becdfd2cbaf4fe6cd0bddacc2e9f9c267aaba43ed12d3dc7f",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e02000000000000000c7374617469632d6f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d0103000000000000001d676f6c64656e2d73657373696f6e2d7475726e2d7374617469633a76310100000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:fc885b81eea518ce802a904d4953ecf827d659de65305190f37643da62a811e6",
        ),
    ];
    assert_eq!(actual.len(), expected.len());
    for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
        assert_eq!(preimage, expected_preimage);
        assert_eq!(key, expected_key);
    }
}

#[test]
fn replay_route_rotates_trigger_definition_to_the_current_family_without_moving_the_legacy_one() {
    let owner = TriggerOwnerScope::session("owner");
    let mut draft = minimal_identity_corpus_draft(crate::ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            "call",
            crate::ToolId::new("tool-id"),
            "tool",
            serde_json::json!({}),
            Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item".to_string()),
                opaque: None,
                origin: None,
            }),
            serde_json::Value::Null,
        ),
    });
    let legacy = trigger_subscription_definition_fingerprint(&owner, &draft);
    assert!(legacy.starts_with("trigger-definition:v3:blake3:"));

    let crate::ProcessInput::ToolCall { call } = &mut draft.target else {
        unreachable!()
    };
    call.replay.as_mut().expect("replay").origin =
        Some(lash_sansio::llm::types::ProviderRouteIdentity::new(
            "openai-compatible",
            "https://gateway.example/v1",
            "shared-model",
        ));
    let routed = trigger_subscription_definition_fingerprint(&owner, &draft);
    assert!(routed.starts_with("trigger-definition:v5:blake3:"));
    assert_ne!(legacy, routed);
}

#[test]
fn executable_trigger_definition_changes_rotate_the_fingerprint() {
    let mut first = minimal_identity_corpus_draft(crate::ProcessInput::External {
        metadata: serde_json::json!({"revision": 1}),
    });
    first.event_types = vec![crate::ProcessEventType {
        name: "app.event".to_string(),
        payload_schema: crate::LashSchema::new(serde_json::json!({"type": "string"})),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    }];
    let mut second = first.clone();
    second.event_types[0].payload_schema =
        crate::LashSchema::new(serde_json::json!({"type": "number"}));
    assert_ne!(
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &second)
    );

    let mut annotated = first.clone();
    annotated.event_types[0].payload_schema = crate::LashSchema::new(
        serde_json::json!({"type": "string", "description": "display only"}),
    );
    assert_eq!(
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &annotated),
        "schema annotations are not executable trigger definition"
    );

    let mut ordered = first.clone();
    ordered.event_types.push(crate::ProcessEventType {
        name: "app.another".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    });
    let mut reversed = ordered.clone();
    reversed.event_types.reverse();
    assert_eq!(
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &ordered),
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &reversed),
        "event declaration source order is not executable trigger definition"
    );
}

#[test]
fn trigger_operation_identity_golden_corpus() {
    let owner = TriggerOwnerScope::session("owner");
    let actor = crate::ProcessOriginator::host_scoped("actor");
    let session_actor = crate::ProcessOriginator::session(crate::SessionScope::new("actor"));
    let draft = minimal_identity_corpus_draft(crate::ProcessInput::External {
        metadata: serde_json::json!({"metadata": 0}),
    });
    let commands = [
        TriggerCommand::Register {
            owner_scope: owner.clone(),
            actor: actor.clone(),
            draft: draft.clone(),
        },
        TriggerCommand::List {
            owner_scope: owner.clone(),
            filter: TriggerSubscriptionFilter {
                registrant_scope_id: Some("r".to_string()),
                subscription_key: Some("s".to_string()),
                name: None,
                source_type: Some("t".to_string()),
                source_key: None,
                target: Some(serde_json::json!({"target": 0})),
                enabled: Some(false),
            },
        },
        TriggerCommand::List {
            owner_scope: TriggerOwnerScope::Platform,
            filter: TriggerSubscriptionFilter {
                registrant_scope_id: None,
                subscription_key: None,
                name: None,
                source_type: None,
                source_key: None,
                target: None,
                enabled: Some(true),
            },
        },
        TriggerCommand::Update {
            owner_scope: owner.clone(),
            actor: actor.clone(),
            subscription_key: "sub".to_string(),
            draft: draft.clone(),
            expected_revision: 0,
        },
        TriggerCommand::Enable {
            owner_scope: owner.clone(),
            actor: actor.clone(),
            subscription_key: "sub".to_string(),
            expected_revision: 0,
        },
        TriggerCommand::Disable {
            owner_scope: owner.clone(),
            actor: actor.clone(),
            subscription_key: "sub".to_string(),
            expected_revision: 0,
        },
        TriggerCommand::Delete {
            owner_scope: owner.clone(),
            actor: crate::ProcessOriginator::host(),
            subscription_key: "sub".to_string(),
            expected_revision: 0,
        },
        TriggerCommand::Revive {
            owner_scope: owner.clone(),
            actor: actor.clone(),
            subscription_key: "sub".to_string(),
            draft,
            expected_revision: 0,
        },
        TriggerCommand::Prune {
            owner_scope: owner.clone(),
            actor: session_actor,
            subscription_keys: vec!["ab".to_string(), "a".to_string()],
        },
    ];
    let actual = commands
        .iter()
        .map(|command| {
            (
                hex(&super::super::trigger_command_preimage(command)),
                super::super::trigger_command_fingerprint(command),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64010100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d0104000000000000000e7b226d65746164617461223a307d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-command:v6:blake3:808379fb1bd9f6d8c27192163bfe9e2586b5f4f831975d478cddde9646bbc828",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64020100000000000000056f776e657201000000000000000172000100000000000000017300010000000000000001740001000000000000000c7b22746172676574223a307d0100",
            "trigger-command:v8:blake3:c414f58bbe2aed975ae0e6b9b4df6cbc0b853334a9d03f8e89a2f37cea51a89b",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e640203000000000000000101",
            "trigger-command:v8:blake3:553ecaeeccc1402a0055a1502244af82ab2ee1758d238334b076b8792f9d2e65",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64030100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d0104000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
            "trigger-command:v6:blake3:ad0c8b943cde6989c9866c9c0ada849aa439818229e4907bb83288110f0a07bf",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64040100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
            "trigger-command:v6:blake3:2682e5eced91258a0c641291c70432139d59ed31c223e2988eba706ed4d1a52e",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64050100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
            "trigger-command:v6:blake3:313920e9e688c752dd498275b570f610eabba9262728f5c9f73c362024bd3abe",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64060100000000000000056f776e6572010000000000000000037375620000000000000000",
            "trigger-command:v6:blake3:bcf6ccd45fc3e1f3bcbbd20f2e62013113fe39d29d3a4cef68f9d5ffec78064f",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64070100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d0104000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
            "trigger-command:v6:blake3:b38ed370579112542311e06a2679346e5e0caf3132663acf48850e0b7d874b45",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020600000000000000146c6173682e747269676765722d636f6d6d616e64080100000000000000056f776e65720200000000000000056163746f72000000000000000200000000000000026162000000000000000161",
            "trigger-command:v6:blake3:768682191da8e25ffe2fce4bd7f5f118b85818ffa64bb45ff127d80d20ca728f",
        ),
    ];
    assert_eq!(actual.len(), expected.len());
    for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
        assert_eq!(preimage, expected_preimage);
        assert_eq!(key, expected_key);
    }

    assert_eq!(
        (
            hex(&trigger_subscription_address_preimage(
                &TriggerOwnerScope::session("ab"),
                "c",
            )),
            deterministic_subscription_id(&TriggerOwnerScope::session("ab"), "c"),
        ),
        ("6c6173682d737461626c652d6964656e74697479020200000000000000216c6173682e747269676765722d737562736372697074696f6e2d616464726573730100000000000000026162000000000000000163".to_string(), "trigger-subscription:v2:blake3:b509fac416c668d5f457e8dcdc96be35efb397a5584e3b4d2745c97ac115b248".to_string())
    );
    assert_eq!(
        deterministic_subscription_id(&TriggerOwnerScope::session("a"), "bc"),
        "trigger-subscription:v2:blake3:03b6c9ef0ec67e6deb429d74ac6e1d8e596d161aa4add887fd232e0796c260e8"
    );
    assert_eq!(
        (
            hex(&super::super::trigger_operation_receipt_preimage(
                &TriggerOwnerScope::Platform,
                "op:0",
            )),
            super::super::trigger_operation_receipt_id(&TriggerOwnerScope::Platform, "op:0"),
        ),
        ("6c6173682d737461626c652d6964656e746974790202000000000000001e6c6173682e747269676765722d6f7065726174696f6e2d616464726573730300000000000000046f703a30".to_string(), "trigger-operation:v2:blake3:46b0b5f8027144df9bb5e7e175ba3aa82f973b797b0c66ee3194e8bd6657e3f4".to_string())
    );
}

fn button_payload_schema() -> crate::LashSchema {
    crate::LashSchema::any()
}

fn trigger_process_draft(
    source_key: &str,
    process_name: &str,
    env_ref: crate::ProcessExecutionEnvRef,
) -> TriggerSubscriptionDraft {
    TriggerSubscriptionDraft::for_process(
        format!("test/{process_name}"),
        env_ref,
        "ui.button.pressed",
        source_key,
        crate::ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: serde_json::json!({ "process": process_name }),
        },
        crate::ProcessIdentity::labelled("testing-fixture", Some(process_name)),
    )
    .with_payload_schema(crate::LashSchema::any())
}

async fn register(
    store: &InMemoryTriggerStore,
    operation_id: &str,
    draft: TriggerSubscriptionDraft,
) -> TriggerSubscriptionRecord {
    let outcome = store
        .execute_command(
            operation_id,
            TriggerCommand::Register {
                owner_scope: TriggerOwnerScope::host("test").unwrap(),
                actor: crate::ProcessOriginator::host_scoped("test"),
                draft,
            },
        )
        .await
        .expect("execute registration")
        .expect("register subscription");
    let TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("expected mutation receipt")
    };
    receipt.record_snapshot
}

async fn register_for_session(
    store: &InMemoryTriggerStore,
    operation_id: &str,
    session_id: &SessionId,
    draft: TriggerSubscriptionDraft,
) -> TriggerSubscriptionRecord {
    let outcome = store
        .execute_command(
            operation_id,
            TriggerCommand::Register {
                owner_scope: TriggerOwnerScope::session(session_id),
                actor: crate::ProcessOriginator::session(crate::SessionScope::new(session_id)),
                draft,
            },
        )
        .await
        .expect("execute session registration")
        .expect("register session subscription");
    let TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("expected mutation receipt")
    };
    receipt.record_snapshot
}

fn button_occurrence(
    source_key: impl Into<String>,
    idempotency_key: impl Into<String>,
) -> TriggerOccurrenceRequest {
    TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        source_key,
        serde_json::json!({ "button": "Blue" }),
        idempotency_key,
    )
}

#[test]
fn trigger_catalog_rejects_duplicate_trigger_source_identity() {
    let mut catalog = TriggerEventCatalog::new();
    catalog
        .declare(TriggerEvent::new(
            "Button",
            "ui.button",
            "pressed",
            button_payload_schema(),
        ))
        .expect("first trigger occurrence");

    let err = catalog
        .declare(TriggerEvent::new(
            "AlternateButton",
            "ui.button",
            "pressed",
            button_payload_schema(),
        ))
        .expect_err("duplicate public source identity should be rejected");

    assert!(err.contains("duplicate trigger source `ui.button.pressed`"));
}

fn captured_provider_source() -> TriggerSourceCapture {
    TriggerSourceCapture::provider(
        ["ui", "button"],
        crate::LashSchema::new(serde_json::json!({
            "type": "object",
            "properties": {"account": {"type": "string"}},
            "required": ["account"],
            "additionalProperties": false
        })),
        "ui-provider",
        serde_json::json!({"account": "a", "grant": "opaque"}),
    )
}

struct StubRestorer {
    refusal: Option<TriggerRouteRefusal>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    seen: Arc<Mutex<Vec<TriggerSourceCapture>>>,
}

#[async_trait::async_trait]
impl TriggerRouteRestorer for StubRestorer {
    async fn restore(&self, capture: &TriggerSourceCapture) -> Result<(), TriggerRouteRefusal> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen.lock_recover().push(capture.clone());
        match &self.refusal {
            None => Ok(()),
            Some(refusal) => Err(refusal.clone()),
        }
    }
}

async fn router_with_restorer(
    store: Arc<InMemoryTriggerStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    restorer: Option<Arc<StubRestorer>>,
) -> TriggerRouter {
    let mut router = TriggerRouter::new(
        store,
        crate::testing::process_work_wiring_for_registry(registry),
    )
    .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
    if let Some(restorer) = restorer {
        router = router.with_route_restorer(restorer);
    }
    router
}

/// FIG-2913: an explicit update after a delivery was reserved must not
/// rewrite that delivery's captured contract or route.
#[tokio::test]
async fn update_after_reservation_leaves_the_reserved_delivery_capture_intact() {
    let store = Arc::new(InMemoryTriggerStore::default());
    let (_process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
    let draft = trigger_process_draft(&source_key, "reserved", env_ref.clone())
        .with_source_capture(captured_provider_source());
    let registered = register(store.as_ref(), "reserved-register", draft).await;

    let receipt = store
        .ingest_occurrence(
            TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key.clone(),
                serde_json::json!({"button": "Blue"}),
                "reserve-first",
            )
            .with_source(serde_json::json!({"account": "a"})),
        )
        .await
        .expect("reserve delivery");
    assert_eq!(receipt.reservations.len(), 1);
    assert_eq!(
        receipt.reservations[0].subscription.source_capture,
        captured_provider_source(),
        "the reservation pins the capture that was live when it reserved"
    );

    let rerouted = TriggerSourceCapture::provider(
        ["ui", "button"],
        crate::LashSchema::any(),
        "other-provider",
        serde_json::json!({"account": "b"}),
    );
    let updated = store
        .execute_command(
            "reserved-update",
            TriggerCommand::Update {
                owner_scope: TriggerOwnerScope::host("test").unwrap(),
                actor: crate::ProcessOriginator::host_scoped("test"),
                subscription_key: registered.subscription_key.clone(),
                draft: trigger_process_draft(&source_key, "reserved", env_ref)
                    .with_source_capture(rerouted.clone()),
                expected_revision: registered.revision,
            },
        )
        .await
        .expect("execute update")
        .expect("update subscription");
    let TriggerCommandOutcome::Mutation { receipt: updated } = updated else {
        panic!("expected mutation receipt")
    };
    assert_eq!(updated.record_snapshot.source_capture, rerouted);
    assert_ne!(
        updated.record_snapshot.definition_fingerprint, registered.definition_fingerprint,
        "a rerouted source is a different definition"
    );

    let replayed = store
        .ingest_occurrence(
            TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key,
                serde_json::json!({"button": "Blue"}),
                "reserve-first",
            )
            .with_source(serde_json::json!({"account": "a"})),
        )
        .await
        .expect("replay reservation");
    assert_eq!(
        replayed.reservations[0].subscription.source_capture,
        captured_provider_source(),
        "the already-reserved delivery keeps the capture it reserved against"
    );
}

/// FIG-2913: a delivery validates the occurrence against the captured
/// source contract, not against the live catalog.
#[tokio::test]
async fn delivery_refuses_an_occurrence_that_leaves_the_captured_contract() {
    let store = Arc::new(InMemoryTriggerStore::default());
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
    register(
        store.as_ref(),
        "contract-register",
        trigger_process_draft(&source_key, "contract", env_ref)
            .with_source_capture(captured_provider_source()),
    )
    .await;
    let router = router_with_restorer(
        Arc::clone(&store),
        Arc::clone(&registry),
        process_env_store,
        None,
    )
    .await;
    let controller = crate::NativeRuntimeEffectController::default();
    let scoped = crate::ScopedEffectController::borrowed(
        &controller,
        crate::ExecutionScope::runtime_operation("captured-contract"),
    )
    .expect("bind scope");

    let report = router
        .emit(
            TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key.clone(),
                serde_json::json!({"button": "Blue"}),
                "off-contract",
            )
            .with_source(serde_json::json!({"unexpected": true})),
            &scoped,
        )
        .await
        .expect("emit");
    assert!(
        matches!(
            &report.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::Failed { reason }
                if reason.contains("captured source contract")
        ),
        "off-contract occurrence must refuse, got {:?}",
        report.deliveries[0].outcome
    );

    let on_contract = router
        .emit(
            TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key,
                serde_json::json!({"button": "Blue"}),
                "on-contract",
            )
            .with_source(serde_json::json!({"account": "a"})),
            &scoped,
        )
        .await
        .expect("emit on-contract");
    assert_eq!(
        on_contract.deliveries[0].outcome,
        TriggerDeliveryEmitOutcome::Started
    );
}

/// FIG-2913: a temporarily unavailable provider keeps the reserved work and
/// retries the same delivery identity; a revoked route refuses visibly and
/// never reports a false start.
#[tokio::test]
async fn transient_route_failure_retries_the_same_identity_and_revocation_refuses() {
    for (refusal, marker) in [
        (
            TriggerRouteRefusal::Unavailable {
                provider_id: "ui-provider".to_string(),
                message: "connect timeout".to_string(),
            },
            "temporarily unavailable",
        ),
        (
            TriggerRouteRefusal::Revoked {
                provider_id: "ui-provider".to_string(),
                message: "grant withdrawn".to_string(),
            },
            "refuses the captured route",
        ),
    ] {
        let store = Arc::new(InMemoryTriggerStore::default());
        let registry: Arc<dyn crate::ProcessRegistry> =
            Arc::new(crate::TestLocalProcessRegistry::default());
        let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        register(
            store.as_ref(),
            "route-register",
            trigger_process_draft(&source_key, "route", env_ref)
                .with_source_capture(captured_provider_source()),
        )
        .await;
        let restorer = Arc::new(StubRestorer {
            refusal: Some(refusal.clone()),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let router = router_with_restorer(
            Arc::clone(&store),
            Arc::clone(&registry),
            Arc::clone(&process_env_store),
            Some(Arc::clone(&restorer)),
        )
        .await;
        let controller = crate::NativeRuntimeEffectController::default();
        let scoped = crate::ScopedEffectController::borrowed(
            &controller,
            crate::ExecutionScope::runtime_operation("route-restore"),
        )
        .expect("bind scope");
        let occurrence = || {
            TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key.clone(),
                serde_json::json!({"button": "Blue"}),
                "route-attempt",
            )
            .with_source(serde_json::json!({"account": "a"}))
        };

        let report = router.emit(occurrence(), &scoped).await.expect("emit");
        let delivery = &report.deliveries[0];
        assert!(
            matches!(
                &delivery.outcome,
                TriggerDeliveryEmitOutcome::Failed { reason } if reason.contains(marker)
            ),
            "expected a visible {marker} refusal, got {:?}",
            delivery.outcome
        );
        assert!(
            registry
                .get_process(&delivery.process_id)
                .await
                .expect("read process")
                .is_none(),
            "a refused route must not start the target process"
        );
        assert_eq!(
            restorer.seen.lock_recover()[0],
            captured_provider_source(),
            "the restorer sees the capture, never a re-resolved definition"
        );

        // The reservation stayed durable. A restored provider retries the
        // identical delivery identity rather than minting a new one.
        let restored = Arc::new(StubRestorer {
            refusal: None,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let router = router_with_restorer(
            store,
            Arc::clone(&registry),
            process_env_store,
            Some(restored),
        )
        .await;
        let retry = router
            .emit(occurrence(), &scoped)
            .await
            .expect("retry emit");
        assert_eq!(retry.deliveries[0].process_id, delivery.process_id);
        assert_eq!(
            retry.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::AlreadyReserved
        );
    }
}

#[tokio::test]
async fn trigger_store_rejects_mismatched_target_label() {
    let store = InMemoryTriggerStore::default();
    let draft = TriggerSubscriptionDraft::for_process(
        "mismatched-label",
        crate::ProcessExecutionEnvRef::new("process-env:test"),
        "ui.button.pressed",
        "source-key",
        crate::ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        crate::ProcessIdentity::labelled("external", Some("expected")),
    )
    .with_target_label("other");

    let err = store
        .execute_command(
            "mismatched-label",
            TriggerCommand::Register {
                owner_scope: TriggerOwnerScope::host("test").unwrap(),
                actor: crate::ProcessOriginator::host_scoped("test"),
                draft,
            },
        )
        .await
        .expect("store execution")
        .expect_err("mismatched target labels should be rejected");
    assert!(err.to_string().contains("target_label must match"));
}

#[tokio::test]
async fn trigger_emit_report_records_started_and_already_reserved_deliveries() {
    let store = Arc::new(InMemoryTriggerStore::default());
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
    let subscription = register(
        store.as_ref(),
        "started-register",
        trigger_process_draft(&source_key, "started", env_ref),
    )
    .await;
    let router = TriggerRouter::new(
        store,
        crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
    )
    .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
    let controller = crate::NativeRuntimeEffectController::default();
    let scoped_controller = crate::ScopedEffectController::borrowed(
        &controller,
        crate::ExecutionScope::runtime_operation("trigger-blue-report"),
    )
    .expect("bind trigger report scope");

    let report = router
        .emit(
            button_occurrence(source_key.clone(), "button-blue-report"),
            &scoped_controller,
        )
        .await
        .expect("emit trigger");
    assert_eq!(report.deliveries.len(), 1);
    let delivery = &report.deliveries[0];
    assert_eq!(delivery.occurrence_id, report.occurrence_id);
    assert_eq!(delivery.subscription_id, subscription.subscription_id);
    assert_eq!(delivery.outcome, TriggerDeliveryEmitOutcome::Started);
    let record = registry
        .get_process(&delivery.process_id)
        .await
        .expect("read process")
        .expect("started process record");
    assert!(matches!(
        record.provenance.caused_by,
        Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id: Some(subscription_id),
            ..
        }) if occurrence_id == report.occurrence_id
            && subscription_id == subscription.subscription_id
    ));

    let replay = router
        .emit(
            button_occurrence(source_key, "button-blue-report"),
            &scoped_controller,
        )
        .await
        .expect("replay trigger");
    assert_eq!(replay.deliveries.len(), 1);
    assert_eq!(
        replay.deliveries[0].outcome,
        TriggerDeliveryEmitOutcome::AlreadyReserved
    );
    assert_eq!(replay.deliveries[0].process_id, delivery.process_id);
}

#[tokio::test]
async fn session_trigger_process_is_observed_by_its_registrant() {
    let store = Arc::new(InMemoryTriggerStore::default());
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
    register_for_session(
        store.as_ref(),
        "session-register",
        &SessionId::from("session-owner"),
        trigger_process_draft(&source_key, "session-owned", env_ref),
    )
    .await;
    let router = TriggerRouter::new(
        store,
        crate::testing::process_work_wiring_for_registry(
            Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>
        ),
    )
    .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
    let controller = crate::NativeRuntimeEffectController::default();
    let scoped_controller = crate::ScopedEffectController::borrowed(
        &controller,
        crate::ExecutionScope::runtime_operation("session-trigger-blue"),
    )
    .expect("bind session trigger scope");

    let report = router
        .emit(
            button_occurrence(source_key, "session-button-blue"),
            &scoped_controller,
        )
        .await
        .expect("emit session trigger");
    let process_id = &report.deliveries[0].process_id;
    assert!(
        crate::ProcessObserverRegistry::is_observer(
            registry.as_ref(),
            &SessionId::from("session-owner"),
            process_id
        )
        .await
        .expect("read initial observer"),
        "the session that explicitly registered the trigger must observe its process"
    );
}
