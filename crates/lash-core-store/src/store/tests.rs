use super::*;

fn legacy_turn_commit_hash(commit: &RuntimeCommit) -> String {
    fn scrub(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                let is_message = map.contains_key("role") && map.contains_key("parts");
                let is_message_part = map.contains_key("kind")
                    && map.contains_key("content")
                    && map.contains_key("id");
                if is_message || is_message_part {
                    map.remove("id");
                }
                for key in ["node_id", "parent_node_id", "leaf_node_id", "timestamp"] {
                    map.remove(key);
                }
                map.values_mut().for_each(scrub);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }

    let mut semantic = commit.clone();
    semantic.expected_head_revision = 0;
    let mut value = serde_json::to_value(semantic).expect("serialize legacy commit");
    scrub(&mut value);
    crate::stable_hash::stable_json_sha256_hex(&value).expect("hash legacy commit")
}

fn intent_fixture() -> RuntimeCommit {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from("golden-session"),
        turn_index: 7,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    let graph_data = state.session_graph.data_mut();
    std::sync::Arc::make_mut(&mut graph_data.nodes.make_mut()[0]).timestamp =
        "2026-07-26T10:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp");
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    let node_id =
        derive_history_node_id(&state.session_id, &operation, 0).expect("derive golden node");
    let message = crate::Message {
        id: "payload-message-id".to_string(),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part::text(
            "payload-message-id.p0".to_string(),
            "hello".to_string(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    };
    let graph = GraphAppend::Extend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.clone(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:01.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Conversation(
                    crate::ConversationRecord::from_message(message),
                ),
            },
        }],
    };
    RuntimeCommit::persisted_state_with_graph_commit_and_operation(&state, graph, operation)
        .expect("build intent fixture")
}

#[test]
fn append_identity_refuses_non_append_operation_key() {
    let mut commit = intent_fixture();
    commit.turn_commit.append_request_identity = AppendRequestIdentity::Append {
        encoding_version: 2,
        request_hash: "request-hash".to_string(),
        requested_node_count: 1,
        requested_ancestor_node_id: None,
    };

    let error = commit
        .validate_operation_session()
        .expect_err("append identity on a plain commit operation must be refused");
    assert!(matches!(
        error,
        StoreError::Backend(ref message)
            if message == "append receipt identity metadata is invalid for operation `final`"
    ));
}

fn ingress_fixture(commit: &RuntimeCommit) -> IngressSettlement {
    IngressSettlement {
        run: crate::TurnId::from("root-turn"),
        completed_inputs: vec![crate::TurnInputCompletion {
            session_id: commit.session_id.clone(),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["input".into()],
                applications: Vec::new(),
            },
        }],
        completed_batches: vec![crate::QueuedWorkCompletion {
            session_id: commit.session_id.clone(),
            batch_ids: vec!["batch".into()],
        }],
        released: Vec::new(),
        dropped: Vec::new(),
    }
}

#[test]
fn ingress_settlement_refuses_a_row_named_twice() {
    let mut commit = intent_fixture();
    let mut ingress = ingress_fixture(&commit);
    ingress.released.push(IngressRowId::Batch("batch".into()));
    commit.ingress = Some(ingress);
    let error = commit
        .validate_ingress_settlement()
        .expect_err("a row completed and released must be refused");
    assert!(matches!(
        error,
        StoreError::IngressSettlementDuplicate { ref row, .. }
            if **row == IngressRowId::Batch("batch".into())
    ));
}

#[test]
fn ingress_settlement_refuses_a_completion_minted_for_another_session() {
    let mut commit = intent_fixture();
    let mut ingress = ingress_fixture(&commit);
    ingress.completed_inputs[0].session_id = SessionId::from("foreign-session");
    commit.ingress = Some(ingress);
    let error = commit
        .validate_ingress_settlement()
        .expect_err("a foreign completion must be refused");
    assert!(matches!(
        error,
        StoreError::IngressRowNotAdmitted { ref row, admitted_run: None, .. }
            if **row == IngressRowId::Input("input".into())
    ));
}

#[test]
fn legacy_hash_reproduces_random_committed_message_id_conflict() {
    let mut first = intent_fixture();
    let completion = crate::TurnInputCompletion {
        session_id: SessionId::from("golden-session"),
        data: crate::TurnInputCompletionData {
            input_ids: vec!["input-1".into()],
            applications: vec![crate::TurnInputApplication {
                input_id: "input-1".into(),
                source_key: None,
                turn_id: crate::TurnId::from("turn-42"),
                committed_message_id: "random-attempt-a".to_string(),
                checkpoint: None,
            }],
        },
    };
    first.ingress = Some(IngressSettlement {
        run: crate::TurnId::from("turn-42"),
        completed_inputs: vec![completion],
        completed_batches: Vec::new(),
        released: Vec::new(),
        dropped: Vec::new(),
    });
    let mut replay = first.clone();
    replay
        .ingress
        .as_mut()
        .expect("replay settles ingress")
        .completed_inputs[0]
        .data
        .applications[0]
        .committed_message_id = "random-attempt-b".to_string();

    assert_ne!(
        legacy_turn_commit_hash(&first),
        legacy_turn_commit_hash(&replay),
        "the pre-L2 hash exposes the random initial-input message id"
    );
    assert_ne!(
        first.turn_commit_hash().expect("first intent"),
        replay.turn_commit_hash().expect("replay intent"),
        "application evidence remains semantic and must not be excluded"
    );
    assert_eq!(
        crate::runtime::ingress_message_id("input-1"),
        "m_ingress_input-1"
    );
}

#[test]
fn intent_hash_golden_vector() {
    // Checkpoint manifest v3, explicit ambient tool access, and the config
    // revision are pinned in intent bytes.
    // FIG-3542: the frame-handoff batch list left the intent; a pending
    // follow-on enters it only when the commit leaves one on the head.
    // FIG-4236: the usage deltas left the intent (ADR 0125).
    // FIG-5172: the interrupted-turn closure left the intent (turn cancel
    // is session mail), and with it the always-present turn id field.
    let hash = intent_fixture().turn_commit_hash().expect("golden intent");
    assert_eq!(
        hash,
        include_str!("testdata/runtime_commit_intent.hex").trim()
    );
}

#[test]
#[ignore = "regenerates crates/lash-core-store/src/store/testdata/runtime_commit_intent.hex"]
#[expect(
    clippy::disallowed_methods,
    reason = "the opt-in test generator writes the corpus in the supplied workspace"
)]
fn regenerate_intent_hash_golden_vector() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
    let hash = intent_fixture().turn_commit_hash().expect("golden intent");
    std::fs::write(
        std::path::PathBuf::from(root)
            .join("crates/lash-core-store/src/store/testdata/runtime_commit_intent.hex"),
        format!("{hash}\n"),
    )
    .expect("write intent golden");
}

#[test]
fn failure_evidence_changes_intent_hash_from_current_shape() {
    let baseline = intent_fixture();
    let baseline_hash = baseline.turn_commit_hash().expect("baseline intent");
    assert_eq!(
        baseline_hash,
        include_str!("testdata/runtime_commit_intent.hex").trim()
    );

    let mut with_evidence = baseline;
    with_evidence.failure_evidence = vec![crate::TurnFailureEvidence {
        partial_output: None,
        billed_usage: crate::llm::types::LlmUsage::default(),
        refusal: crate::ChargeSafetyRefusalEvidence {
            denial_reason: crate::ChargeSafetyDenialReason::GuaranteeRequired,
            protocol_position: crate::ProtocolPosition::OutputStarted,
            attempt_number: 1,
            attempt_count: 1,
        },
    }];
    assert_ne!(
        with_evidence
            .turn_commit_hash()
            .expect("intent with failure evidence"),
        baseline_hash,
        "nonempty settlement evidence participates in commit identity"
    );
}

#[test]
fn session_head_meta_refuses_a_head_json_naming_another_session() {
    let error = SessionHeadMeta::assemble(
        &SessionId::from("keyed-session"),
        SessionHeadPayload {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: SessionId::from("impostor-session"),
            config: crate::PersistedSessionConfig::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
                crate::SessionToolAccess::ambient(),
            ),
        },
        7,
        None,
        None,
        None,
    )
    .expect_err("a head payload naming another session is corrupt stored data");

    assert!(
        matches!(
            &error,
            StoreError::StoredDataCorrupt { record_kind, message }
                if *record_kind == "SessionHeadMeta"
                    && message.contains("impostor-session")
                    && message.contains("keyed-session")
        ),
        "the refusal names both identities: {error:?}"
    );
}

#[test]
fn session_head_payload_excludes_the_leaf_derived_frame() {
    let meta = SessionHeadMeta::assemble(
        &SessionId::from("column-owned-head"),
        SessionHeadPayload {
            session_id: SessionId::from("column-owned-head"),
            ..Default::default()
        },
        41,
        Some(BlobRef("checkpoint".to_string())),
        Some("leaf".into()),
        Some(crate::FrameNodeId::new("frame").expect("frame id")),
    )
    .expect("assemble the leaf's derived frame");
    assert_eq!(meta.current_frame_node_id.as_deref(), Some("frame"));
    let payload = serde_json::to_value(meta.payload()).expect("serialize head");
    for column in [
        "current_frame_node_id",
        "leaf_node_id",
        "checkpoint_ref",
        "head_revision",
    ] {
        assert!(
            payload.get(column).is_none(),
            "{column} is not head payload"
        );
    }
}

#[test]
fn node_id_golden_vector() {
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    assert_eq!(
        derive_history_node_id(&SessionId::from("golden-session"), &operation, 3)
            .expect("golden node"),
        "n_f49e2d5bb98b94bf5530b52f34bf226d1742ed60a999302b542abf50a8d030c6"
    );
}

#[test]
fn frame_node_id_golden_vector() {
    assert_eq!(
        crate::frame_node_id(&SessionId::from("golden-session"), "frame-42").as_str(),
        "frame-node/v3/591a075378ab921fd73a0a4d1825ab1ebf32830427f41c332a3ae6807305c128"
    );
}

#[test]
fn intent_hash_is_independent_of_source_and_map_insertion_order() {
    #[derive(serde::Serialize)]
    struct FirstOuter {
        z: u8,
        a: u8,
    }
    #[derive(serde::Serialize)]
    struct FirstSource {
        outer: FirstOuter,
        beta: u8,
        alpha: u8,
    }
    #[derive(serde::Serialize)]
    struct SecondOuter {
        a: u8,
        z: u8,
    }
    #[derive(serde::Serialize)]
    struct SecondSource {
        alpha: u8,
        beta: u8,
        outer: SecondOuter,
    }

    let mut first = intent_fixture();
    let mut second = intent_fixture();
    let first_body = serde_json::to_value(FirstSource {
        outer: FirstOuter { z: 1, a: 2 },
        beta: 3,
        alpha: 4,
    })
    .expect("first body");
    let second_body = serde_json::to_value(SecondSource {
        alpha: 4,
        beta: 3,
        outer: SecondOuter { a: 2, z: 1 },
    })
    .expect("second body");
    let replace_payload = |commit: &mut RuntimeCommit, body| {
        let nodes = commit.graph.nodes_mut();
        nodes[0].payload = crate::SessionNodePayload::Plugin {
            plugin_type: "ordering".to_string(),
            body: crate::session_graph::SharedJsonValue::new(body),
        };
    };
    replace_payload(&mut first, first_body);
    replace_payload(&mut second, second_body);

    assert_eq!(
        first.turn_commit_hash().expect("first ordering hash"),
        second.turn_commit_hash().expect("second ordering hash")
    );
}

#[test]
fn intent_projection_keeps_payload_timestamp_but_excludes_node_timestamp() {
    let first = intent_fixture();
    let mut observed_later = first.clone();
    let nodes = observed_later.graph.nodes_mut();
    nodes[0].timestamp = "2027-01-01T00:00:00.000000000Z"
        .parse()
        .expect("canonical node timestamp");
    assert_eq!(
        first.turn_commit_hash().expect("first hash"),
        observed_later.turn_commit_hash().expect("later hash")
    );

    let mut payload_a = intent_fixture();
    let mut payload_b = intent_fixture();
    for (commit, timestamp) in [
        (&mut payload_a, "payload-time-a"),
        (&mut payload_b, "payload-time-b"),
    ] {
        let nodes = commit.graph.nodes_mut();
        nodes[0].payload = crate::SessionNodePayload::Plugin {
            plugin_type: "tool-result".to_string(),
            body: crate::session_graph::SharedJsonValue::new(
                serde_json::json!({"timestamp": timestamp}),
            ),
        };
    }
    assert_ne!(
        payload_a.turn_commit_hash().expect("payload a"),
        payload_b.turn_commit_hash().expect("payload b")
    );
}

#[test]
fn intent_projection_excludes_host_commit_budget() {
    let bounded = intent_fixture();
    let mut unbounded = bounded.clone();
    unbounded.commit_budget = crate::CommitBudget::new(
        crate::CommitBudgetLimit::Unbounded,
        crate::CommitBudgetLimit::Unbounded,
    );

    assert_eq!(
        bounded.turn_commit_hash().expect("bounded budget hash"),
        unbounded.turn_commit_hash().expect("unbounded budget hash")
    );
}

#[test]
fn derived_node_ids_are_session_operation_and_ordinal_scoped() {
    let first = OperationId::turn("session-a", "turn", "final");
    let other = OperationId::turn("session-a", "other-turn", "final");
    let id = derive_history_node_id(&SessionId::from("session-a"), &first, 0).expect("derive");
    assert_eq!(
        id,
        derive_history_node_id(&SessionId::from("session-a"), &first, 0).expect("rederive")
    );
    assert_ne!(
        id,
        derive_history_node_id(&SessionId::from("session-b"), &first, 0).expect("other session")
    );
    assert_ne!(
        id,
        derive_history_node_id(&SessionId::from("session-a"), &other, 0).expect("other operation")
    );
    assert_ne!(
        id,
        derive_history_node_id(&SessionId::from("session-a"), &first, 1).expect("other ordinal")
    );
}

#[test]
fn node_derivation_guard_rejects_rogue_ids() {
    let mut commit = intent_fixture();
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    commit.turn_commit = RuntimeTurnCommitStamp::new(operation);
    commit.validate_node_derivation().expect("derived proposal");

    let mut rogue = commit.clone();
    let nodes = rogue.graph.nodes_mut();
    nodes[0].node_id = "rogue".into();
    assert!(matches!(
        rogue.validate_node_derivation(),
        Err(StoreError::NodeIdDerivationMismatch { .. })
    ));
}

#[test]
fn node_derivation_guard_rejects_frame_open_rogue_id() {
    let mut commit = intent_fixture();
    let frame_key =
        crate::FrameKey::from_caller_material("frame-42").expect("non-empty frame material");
    let nodes = commit.graph.nodes_mut();
    nodes[0].payload = crate::SessionNodePayload::FrameOpen {
        frame_key,
        reason: crate::AgentFrameReason::initial(),
        assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )),
    };

    assert!(matches!(
        commit.validate_node_derivation(),
        Err(StoreError::NodeIdDerivationMismatch { .. })
    ));
}

#[test]
fn node_derivation_remaps_in_batch_parent_edges() {
    let operation = OperationId::turn("session", "turn", "final");
    let mut graph = GraphAppend::Extend {
        nodes: vec![
            crate::SessionNodeRecord {
                node_id: "draft-a".into(),
                parent_node_id: None,
                timestamp: "2026-07-26T10:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "first".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
            crate::SessionNodeRecord {
                node_id: "draft-b".into(),
                parent_node_id: Some("draft-a".into()),
                timestamp: "2026-07-26T10:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "second".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
        ],
    };
    graph
        .derive_node_ids(&SessionId::from("session"), &operation)
        .expect("derive node ids");
    let nodes = graph.nodes();
    assert_eq!(
        nodes[1].parent_node_id.as_deref(),
        Some(nodes[0].node_id.as_str())
    );
}

#[test]
fn append_chain_rejects_self_parent_cycles() {
    let graph = GraphAppend::Extend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "cycle".into(),
            parent_node_id: Some("cycle".into()),
            timestamp: "2026-07-26T10:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "cycle".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
            },
        }],
    };

    assert!(matches!(
        graph.validate_append_topology(),
        Err(StoreError::InvalidGraphParent {
            expected: None,
            actual: Some(parent),
            ..
        }) if parent == "cycle"
    ));
}

#[test]
// Architecture lint: lexical drift guard between the segment traits and the
// store's operation list, not a behavior proof. It also pins ADR 0112 §1's
// rule that no segment method has a default that answers for the backend.
fn decorator_surface_covers_every_component_trait_method() {
    /// Each method a trait declares, and whether it has a default body.
    fn declared_methods(
        source: &str,
        trait_name: &str,
    ) -> std::collections::BTreeMap<String, bool> {
        let start = source
            .find(&format!("pub trait {trait_name}"))
            .unwrap_or_else(|| panic!("`pub trait {trait_name}` is present in the scanned source"));
        let body = &source[start..];
        let end = body
            .find("\n}\n")
            .unwrap_or_else(|| panic!("the `{trait_name}` body closes at column zero"));
        let lines: Vec<&str> = body[..end].lines().collect();
        let mut methods = std::collections::BTreeMap::new();
        for (index, line) in lines.iter().enumerate() {
            let Some(rest) = line.strip_prefix("    ") else {
                continue;
            };
            if rest.starts_with(' ') {
                continue;
            }
            let rest = rest.strip_prefix("async ").unwrap_or(rest);
            let Some(rest) = rest.strip_prefix("fn ") else {
                continue;
            };
            let name = rest
                .split(['(', '<'])
                .next()
                .unwrap_or_default()
                .to_string();
            let has_default = lines[index..]
                .iter()
                .map(|line| line.trim_end())
                .find(|line| line.ends_with(';') || line.ends_with('{'))
                .is_some_and(|line| line.ends_with('{'));
            methods.insert(name, has_default);
        }
        methods
    }

    // Provided methods a backend overrides with stronger semantics, so a
    // decorator forwards them rather than composing them over its own
    // primitive: a backend's own single-statement probes.
    const FORWARDED_PROVIDED: &[&str] = &["admit_session_state", "enqueue_queued_work"];
    const UNCHANGED_SEGMENT_DEFAULTS: &[&str] = &[];

    let store_mod = include_str!("mod.rs");
    let mut declared = declared_methods(
        include_str!("attachment_referrers.rs"),
        "AttachmentReferrers",
    );
    declared.extend(declared_methods(
        include_str!("catalog.rs"),
        "SessionCatalogStore",
    ));
    declared.extend(declared_methods(
        include_str!("history.rs"),
        "SessionHistoryStore",
    ));
    for trait_name in [
        "SessionCommitStore",
        "TurnInputStore",
        "QueuedWorkStore",
        "StoreMaintenance",
    ] {
        declared.extend(declared_methods(store_mod, trait_name));
    }
    declared.extend(declared_methods(
        include_str!("session_fault.rs"),
        "SessionFaultStore",
    ));
    declared.extend(declared_methods(include_str!("run.rs"), "RunStore"));
    assert!(
        declared.contains_key("commit_runtime_state")
            && declared.contains_key("vacuum")
            && declared.contains_key("load_session_window")
            && declared.contains_key("admit_session"),
        "the segment-trait scan must cover every segment: {declared:?}"
    );

    let listed: std::collections::BTreeMap<&str, bool> =
        super::runtime_store_decorator::RUNTIME_STORE_OPERATIONS
            .iter()
            .map(|operation| (operation.name, operation.provided))
            .collect();
    for convenience in super::runtime_store_decorator::self_routed_conveniences() {
        assert!(
            declared.get(convenience) == Some(&true),
            "a self-routed convenience is a provided method of its segment: {convenience}"
        );
    }
    let missing: Vec<_> = declared
        .keys()
        .filter(|name| !listed.contains_key(name.as_str()))
        .collect();
    let extra: Vec<_> = listed
        .keys()
        .filter(|name| !declared.contains_key(**name))
        .collect();
    assert!(
        missing.is_empty(),
        "segment methods a decorator would silently resolve to the trait's own default instead \
         of forwarding to `inner()`; add them to `runtime_store_operations!`: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "`runtime_store_operations!` lists operations no segment declares: {extra:?}"
    );

    let defaulted: Vec<_> = declared
        .iter()
        .filter(|(name, has_default)| {
            **has_default
                && listed.get(name.as_str()) != Some(&true)
                && !FORWARDED_PROVIDED.contains(&name.as_str())
                && !UNCHANGED_SEGMENT_DEFAULTS.contains(&name.as_str())
        })
        .map(|(name, _)| name)
        .collect();
    assert!(
        defaulted.is_empty(),
        "segment methods with a default that is neither a listed composition nor a forwarded \
         provided method; make them required (ADR 0112 §1): {defaulted:?}"
    );

    // The `@inner` segment: the deployment's control-intent ledger.
    let ledger: std::collections::BTreeSet<String> =
        declared_methods(include_str!("control_intent.rs"), "ControlIntentStore")
            .into_keys()
            .collect();
    let listed_ledger: std::collections::BTreeSet<String> =
        super::runtime_store_decorator::CONTROL_INTENT_OPERATIONS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
    assert!(
        ledger.contains("load_intent"),
        "the ledger scan must reach the trait's last method: {ledger:?}"
    );
    assert_eq!(
        ledger, listed_ledger,
        "the `@inner` segment of `runtime_store_operations!` must list exactly the \
         `ControlIntentStore` operations"
    );
}

#[test]
// Architecture lint: the scripted store's operations are the ones a decorator
// can intercept, no more and no fewer, so a rule on any `StoreOp` can fire.
fn every_interceptable_operation_is_scriptable() {
    let interceptable: Vec<&str> = super::runtime_store_decorator::RUNTIME_STORE_OPERATIONS
        .iter()
        .filter(|operation| !operation.provided)
        .map(|operation| operation.name)
        .chain(
            super::runtime_store_decorator::CONTROL_INTENT_OPERATIONS
                .iter()
                .copied(),
        )
        .collect();
    let scriptable: Vec<&str> = StoreOp::ALL.iter().map(|op| op.name()).collect();
    assert_eq!(scriptable, interceptable);
    assert!(scriptable.contains(&"bind_run_inputs") && scriptable.contains(&"load_intent"));
    let distinct: std::collections::BTreeSet<&str> = scriptable.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        scriptable.len(),
        "one operation name, one `StoreOp`"
    );
}
