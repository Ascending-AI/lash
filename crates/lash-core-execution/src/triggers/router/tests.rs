use super::*;

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
        trace: None,
    };
    assert_eq!(
        serde_json::to_value(&request).expect("serialize fired occurrence request")["outcome"],
        serde_json::json!({"kind": "fired"}),
        "every fired request records its outcome"
    );
    assert_eq!(
        serde_json::to_value(&record).expect("serialize fired occurrence record")["outcome"],
        serde_json::json!({"kind": "fired"}),
        "every fired record records its outcome"
    );
    assert!(
        serde_json::from_str::<TriggerOccurrenceRecord>(
            r#"{"occurrence_id":"trigger:caller:key","source_type":"source","source_key":"key","payload":{"value":1},"idempotency_key":"caller:key","source":{"origin":true},"occurred_at_ms":42}"#,
        )
        .is_err(),
        "records without an outcome are refused"
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
        .with_payload_schema(
            crate::JsonSchema::admit(serde_json::json!({"type": "object"}))
                .expect("valid declared payload schema"),
        )
        .with_wake_target(crate::SessionScope::for_agent_frame(
            "session",
            crate::FrameNodeId::new("frame").expect("test frame identity is non-empty"),
        ))
        .with_event_types([crate::ProcessEventType {
            name: "app.event".to_string(),
            payload_schema: crate::JsonSchema::admit(serde_json::json!({"type": "object"}))
                .expect("valid declared payload schema"),
            semantics: crate::ProcessEventSemanticsSpec {
                terminal: Some(crate::ProcessTerminalSpec {
                    status: crate::TerminalProcessStatus::Completed,
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
    let inputs = [
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
            result: crate::SessionTurnOutcome::FinalValue {
                schema: Some(
                    lash_sansio::JsonSchema::admit(serde_json::json!({}))
                        .expect("declared result schema"),
                ),
            },
        },
        crate::ProcessInput::SessionTurn {
            definition_key: "golden-session-turn-static:v1".to_string(),
            create_request: Box::new(crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )),
            turn_input: Box::new(crate::TurnInput::empty()),
            result: crate::SessionTurnOutcome::Turn,
        },
    ];
    let owners = [
        TriggerOwnerScope::host("owner").expect("host owner"),
        TriggerOwnerScope::Platform,
        TriggerOwnerScope::host("static-owner").expect("host owner"),
    ];
    let actual = owners
        .iter()
        .zip(inputs)
        .map(|(owner, input)| {
            let draft = minimal_identity_corpus_draft(input);
            (
                hex(&trigger_subscription_definition_preimage(owner, &draft)),
                trigger_subscription_definition_fingerprint(owner, &draft),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0200000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000d7b227061796c6f6164223a307d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:a19efb9486669ed2268b20762592b63d3a8f166d49d03c81a9e4603904849e58",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0300000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01030000000000000016676f6c64656e2d73657373696f6e2d7475726e3a7631020100000000000000027b7d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-definition:v3:blake3:2452cffc456ab1530c5dd8c35b4547e7123554a29682e8d44cd6a39a118e6a8a",
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
fn executable_trigger_definition_changes_rotate_the_fingerprint() {
    let mut first = minimal_identity_corpus_draft(crate::ProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({"revision": 1}),
    });
    first.event_types = vec![crate::ProcessEventType {
        name: "app.event".to_string(),
        payload_schema: crate::JsonSchema::admit(serde_json::json!({"type": "string"}))
            .expect("valid declared payload schema"),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    }];
    let mut second = first.clone();
    second.event_types[0].payload_schema =
        crate::JsonSchema::admit(serde_json::json!({"type": "number"}))
            .expect("valid declared payload schema");
    assert_ne!(
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &second)
    );

    let mut annotated = first.clone();
    annotated.event_types[0].payload_schema = crate::JsonSchema::admit(
        serde_json::json!({"type": "string", "description": "display only"}),
    )
    .expect("valid declared payload schema");
    assert_eq!(
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
        trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &annotated),
        "schema annotations are not executable trigger definition"
    );

    let mut ordered = first.clone();
    ordered.event_types.push(crate::ProcessEventType {
        name: "app.another".to_string(),
        payload_schema: crate::JsonSchema::any(),
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
    let draft = minimal_identity_corpus_draft(crate::ProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({"metadata": 0}),
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
    for command in &commands {
        let key = super::super::trigger_command_fingerprint(command);
        assert!(
            key.starts_with(&format!(
                "trigger-command:v{}:blake3:",
                super::super::command::TRIGGER_COMMAND_FAMILY_VERSION,
            )),
            "retired command family: {key}"
        );
    }
    let actual = commands
        .iter()
        .map(|command| {
            (
                hex(&super::super::command::trigger_command_preimage(command)),
                super::super::trigger_command_fingerprint(command),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64010100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000e7b226d65746164617461223a307d00000000000000046b696e6400000000000000000000000000000000000000",
            "trigger-command:v8:blake3:f7b1e8db73f56556638512f36b6ab4321ba36185c4a1ccf7da6e104b750ff7bf",
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
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64030100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
            "trigger-command:v8:blake3:f44cca662a335daa0445dcb8df7c3ff94fcb4ba9eb7ada48eb3798ebfd0fa283",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64040100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
            "trigger-command:v8:blake3:ff67b6492a755c198bc76427e44d11f9575eb5b2065d53b70d373926e4fe5605",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64050100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
            "trigger-command:v8:blake3:c3772794dc89de48fc0c6bf73ca6a1b8b5c7b8a65366eebab3af7ee7411de757",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64060100000000000000056f776e6572010000000000000000037375620000000000000000",
            "trigger-command:v8:blake3:3d9bd2ce7e06b357a810f5f9f8d7c14da320709c5c8f1a8b2199cafef56fd307",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64070100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
            "trigger-command:v8:blake3:b910dbe614aca31183769fa61701af2e06af528f9cda6650a54bff7868c8152f",
        ),
        (
            "6c6173682d737461626c652d6964656e74697479020800000000000000146c6173682e747269676765722d636f6d6d616e64080100000000000000056f776e65720200000000000000056163746f72000000000000000200000000000000026162000000000000000161",
            "trigger-command:v8:blake3:587b1cc0a89f9b429e79629e7bfdc4fd3732f3edd862919cc388a7c74fd002dc",
        ),
    ];
    assert_eq!(
        actual,
        expected.map(|(preimage, key)| (preimage.to_string(), key.to_string())),
    );

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
            hex(&super::super::command::trigger_operation_receipt_preimage(
                &TriggerOwnerScope::Platform,
                "op:0",
            )),
            super::super::trigger_operation_receipt_id(&TriggerOwnerScope::Platform, "op:0"),
        ),
        ("6c6173682d737461626c652d6964656e746974790202000000000000001e6c6173682e747269676765722d6f7065726174696f6e2d616464726573730300000000000000046f703a30".to_string(), "trigger-operation:v2:blake3:46b0b5f8027144df9bb5e7e175ba3aa82f973b797b0c66ee3194e8bd6657e3f4".to_string())
    );
}

fn button_payload_schema() -> crate::JsonSchema {
    crate::JsonSchema::any()
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

#[test]
fn enriched_engine_trigger_definition_keeps_v3_and_tracks_payload() {
    let mut draft = enriched_identity_corpus_draft(crate::ProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({"payload": 0}),
    });
    let owner = TriggerOwnerScope::session("owner");
    assert_eq!(
        hex(&trigger_subscription_definition_preimage(&owner, &draft)),
        "6c6173682d737461626c652d6964656e74697479020300000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7601000000000000000773657373696f6e0100000000000000056672616d650100000000000000046e616d650000000000000006736f7572636500000000000000036b657900000000000000127b22736f75726365223a5b302c2230225d7d00000000000000117b2274797065223a226f626a656374227d000000000000000000000000000000027b7d01020000000000000006656e67696e65000000000000000d7b227061796c6f6164223a307d00000000000000046b696e640100000000000000056c6162656c0100000000000000107b22646566696e6974696f6e223a307d000000000000000100000000000000096170702e6576656e7400000000000000117b2274797065223a226f626a656374227d0103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740300000000000000013000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000056576656e7401000000000000000566697865640200000000000000405b6e756c6c2c66616c73652c747275652c2d312c302c31383434363734343037333730393535313631352c312e352c22613a62222c5b5d2c7b2278223a307d5d0100000000000000056c6162656c"
    );
    let first = trigger_subscription_definition_fingerprint(&owner, &draft);
    assert_eq!(
        first,
        "trigger-definition:v3:blake3:e81330239140b8feb59360f1dcdbecf2a98fd210dfd84828fd2524ac5a20aea3"
    );
    assert!(first.starts_with("trigger-definition:v3:blake3:"));
    let crate::ProcessStartTarget::Input(crate::ProcessInput::Engine { payload, .. }) =
        &mut draft.target
    else {
        unreachable!()
    };
    *payload = serde_json::json!({"payload": 1});
    assert_ne!(
        first,
        trigger_subscription_definition_fingerprint(&owner, &draft)
    );
}
