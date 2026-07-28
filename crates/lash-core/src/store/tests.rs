use super::*;

fn legacy_turn_commit_hash(commit: &RuntimeCommit) -> String {
    fn scrub(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                let is_message = map.contains_key("role") && map.contains_key("parts");
                let is_message_part = map.contains_key("kind")
                    && map.contains_key("content")
                    && map.contains_key("prune_state");
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
    semantic.release_session_execution_lease = None;
    let mut value = serde_json::to_value(semantic).expect("serialize legacy commit");
    scrub(&mut value);
    crate::stable_hash::stable_json_sha256_hex(&value).expect("hash legacy commit")
}

fn intent_fixture() -> RuntimeCommit {
    let mut state = crate::RuntimeSessionState {
        session_id: "golden-session".to_string(),
        turn_index: 7,
        ..crate::RuntimeSessionState::default()
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:00Z".to_string();
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    let node_id =
        derive_history_node_id(&state.session_id, &operation, 0).expect("derive golden node");
    let message = crate::Message {
        id: "payload-message-id".to_string(),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part {
            id: "payload-message-id.p0".to_string(),
            kind: crate::PartKind::Text,
            content: "hello".to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: crate::PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: None,
    };
    let graph = GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.clone(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:01Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Conversation(
                    crate::ConversationRecord::from_message(message),
                ),
            },
        }],
        leaf_node_id: Some(node_id),
    };
    RuntimeCommit::persisted_state_with_graph_commit_and_operation(&state, graph, &[], operation)
        .expect("build intent fixture")
}

#[test]
fn first_persisted_state_commit_derives_and_installs_node_ids() {
    let placeholder = "draft-node/v2:first".to_string();
    let mut state = crate::RuntimeSessionState {
        session_id: "first-commit".to_string(),
        session_graph: crate::SessionGraph::from_nodes(
            vec![crate::SessionNodeRecord {
                node_id: placeholder.clone(),
                parent_node_id: None,
                timestamp: "2026-07-27T00:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "first-commit".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            }],
            Some(placeholder),
        ),
        ..crate::RuntimeSessionState::default()
    };
    let operation = OperationId::new(
        crate::ExecutionScope::runtime_operation("first-commit"),
        "initial",
    );
    let expected = derive_history_node_id(&state.session_id, &operation, 0)
        .expect("derive expected first node id");

    let (commit, persisted_node_ids) =
        RuntimeCommit::persisted_state_with_operation(&mut state, &[], operation)
            .expect("build first append");
    let GraphAppend {
        nodes,
        leaf_node_id,
    } = commit.graph;
    assert_eq!(persisted_node_ids, vec![expected.clone()]);
    assert_eq!(nodes[0].node_id, expected);
    assert_eq!(leaf_node_id, Some(nodes[0].node_id.clone()));
    assert_eq!(state.session_graph.nodes[0].node_id, nodes[0].node_id);
    assert_eq!(state.session_graph.leaf_node_id, leaf_node_id);
}

#[test]
fn with_operation_returns_the_append_id_mapping() {
    let commit = intent_fixture();
    let GraphAppend { nodes, .. } = &commit.graph;
    let old_node_id = nodes[0].node_id.clone();
    let operation = OperationId::turn("golden-session", "turn-43", "final");
    let expected_node_id = derive_history_node_id(&commit.session_id, &operation, 0)
        .expect("derive replacement node id");

    let (commit, mapping) = commit
        .with_operation(operation)
        .expect("derive and stamp commit");

    assert_eq!(
        mapping,
        vec![(old_node_id.clone(), expected_node_id.clone())]
    );
    let GraphAppend {
        nodes,
        leaf_node_id,
    } = commit.graph;
    assert_eq!(nodes[0].node_id, expected_node_id);
    assert_eq!(leaf_node_id, Some(expected_node_id));
}

#[test]
fn legacy_hash_reproduces_random_committed_message_id_conflict() {
    let mut first = intent_fixture();
    first.completed_turn_input_claims = vec![crate::TurnInputCompletion {
        session_id: "golden-session".to_string(),
        claim_id: "claim-a".to_string(),
        lease_token: "lease-a".to_string(),
        input_ids: vec!["input-1".to_string()],
        applications: vec![crate::TurnInputApplication {
            input_id: "input-1".to_string(),
            source_key: None,
            turn_id: "turn-42".to_string(),
            committed_message_id: "random-attempt-a".to_string(),
            checkpoint: None,
        }],
    }];
    let mut replay = first.clone();
    replay.completed_turn_input_claims[0].applications[0].committed_message_id =
        "random-attempt-b".to_string();

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
    assert_eq!(
        intent_fixture().turn_commit_hash().expect("golden intent"),
        "2c20265fafb2dac582ccba071cf210647dfd4fc15a90f679067fed8ca77d5030"
    );
}

#[test]
fn session_head_payload_bytes_match_the_legacy_meta_format() {
    #[allow(dead_code)]
    #[derive(serde::Serialize)]
    struct LegacySessionHeadMeta {
        schema_version: u32,
        #[serde(default = "super::default_root_session_id")]
        session_id: String,
        #[serde(skip)]
        head_revision: u64,
        config: crate::PersistedSessionConfig,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_frame_node_id: Option<String>,
        #[serde(skip)]
        checkpoint_ref: Option<BlobRef>,
        #[serde(skip)]
        leaf_node_id: Option<String>,
    }

    let legacy = LegacySessionHeadMeta {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: "column-owned-head".to_string(),
        head_revision: 41,
        config: crate::PersistedSessionConfig::default(),
        current_frame_node_id: None,
        checkpoint_ref: Some(BlobRef("checkpoint".to_string())),
        leaf_node_id: Some("leaf".to_string()),
    };
    let assembled = SessionHeadMeta::assemble(
        SessionHeadPayload {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: "column-owned-head".to_string(),
            config: crate::PersistedSessionConfig::default(),
            current_frame_node_id: None,
        },
        41,
        Some(BlobRef("checkpoint".to_string())),
        Some("leaf".to_string()),
    );
    let before = serde_json::to_vec(&legacy).expect("serialize legacy session head metadata");
    let after = serde_json::to_vec(&assembled.payload()).expect("serialize session head payload");

    assert_eq!(
        after, before,
        "the head_json payload must remain byte-identical"
    );
}

#[test]
fn operation_conflict_diagnostic_explains_identity_reuse() {
    let message = StoreError::RuntimeTurnCommitConflict {
        session_id: "root".to_string(),
        turn_id: "operation-key".to_string(),
    }
    .to_string();

    assert!(message.contains("runtime operation"));
    assert!(message.contains("different commit content"));
    assert!(message.contains("reuse an operation identity only"));
}

#[test]
fn node_id_golden_vector() {
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    assert_eq!(
        derive_history_node_id("golden-session", &operation, 3).expect("golden node"),
        "n_38b8d1d55ece31f85383dbdbba3e0d2dfa134bc6f9c9b7ada0847479edf63280"
    );
}

#[test]
fn frame_node_id_golden_vector() {
    assert_eq!(
        crate::frame_node_id("golden-session", "frame-42"),
        "frame-node/v2/dea90a18d10331ef6adf1493fb73edb50b45b8e51d375e4d524524928cfa18bc"
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
        let GraphAppend { nodes, .. } = &mut commit.graph;
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
    let GraphAppend { nodes, .. } = &mut observed_later.graph;
    nodes[0].timestamp = "2027-01-01T00:00:00Z".to_string();
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
        let GraphAppend { nodes, .. } = &mut commit.graph;
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
fn derived_node_ids_are_session_operation_and_ordinal_scoped() {
    let first = OperationId::turn("session-a", "turn", "final");
    let other = OperationId::turn("session-a", "other-turn", "final");
    let id = derive_history_node_id("session-a", &first, 0).expect("derive");
    assert_eq!(
        id,
        derive_history_node_id("session-a", &first, 0).expect("rederive")
    );
    assert_ne!(
        id,
        derive_history_node_id("session-b", &first, 0).expect("other session")
    );
    assert_ne!(
        id,
        derive_history_node_id("session-a", &other, 0).expect("other operation")
    );
    assert_ne!(
        id,
        derive_history_node_id("session-a", &first, 1).expect("other ordinal")
    );
}

#[test]
fn node_derivation_guard_rejects_rogue_ids() {
    let mut commit = intent_fixture();
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    commit.turn_commit = RuntimeTurnCommitStamp::new(operation);
    commit.validate_node_derivation().expect("derived proposal");

    let mut rogue = commit.clone();
    let GraphAppend { nodes, .. } = &mut rogue.graph;
    nodes[0].node_id = "rogue".to_string();
    assert!(matches!(
        rogue.validate_node_derivation(),
        Err(StoreError::NodeIdDerivationMismatch { .. })
    ));
}

#[test]
fn node_derivation_remaps_in_batch_parent_edges() {
    let operation = OperationId::turn("session", "turn", "final");
    let mut graph = GraphAppend {
        nodes: vec![
            crate::SessionNodeRecord {
                node_id: "draft-a".to_string(),
                parent_node_id: None,
                timestamp: "2026-07-26T10:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "first".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
            crate::SessionNodeRecord {
                node_id: "draft-b".to_string(),
                parent_node_id: Some("draft-a".to_string()),
                timestamp: "2026-07-26T10:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "second".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
        ],
        leaf_node_id: Some("draft-b".to_string()),
    };
    graph
        .derive_node_ids("session", &operation)
        .expect("derive node ids");
    let GraphAppend { nodes, .. } = graph;
    assert_eq!(
        nodes[1].parent_node_id.as_deref(),
        Some(nodes[0].node_id.as_str())
    );
}

#[test]
fn frame_node_identity_is_stable_across_operation_realization() {
    let operation = OperationId::turn("session", "turn", "final");
    let frame_node_id = crate::session_graph::frame_node_id("session", "initial-frame");
    let mut graph = GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: frame_node_id.clone(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::FrameOpen {
                frame_key: "initial-frame".to_string(),
                reason: crate::AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(
                    crate::SessionPolicy::default(),
                ),
                protocol_turn_options: crate::ProtocolTurnOptions::default(),
            },
        }],
        leaf_node_id: Some(frame_node_id.clone()),
    };

    graph
        .derive_node_ids("session", &operation)
        .expect("realize frame node");

    let GraphAppend {
        nodes,
        leaf_node_id,
    } = graph;
    assert_eq!(nodes[0].node_id, frame_node_id);
    assert_eq!(leaf_node_id, Some(frame_node_id));
}

#[test]
fn append_chain_rejects_self_parent_cycles() {
    let graph = GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "cycle".to_string(),
            parent_node_id: Some("cycle".to_string()),
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "cycle".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
            },
        }],
        leaf_node_id: Some("cycle".to_string()),
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
fn append_leaf_must_be_the_terminal_appended_node() {
    let graph = GraphAppend {
        nodes: vec![
            crate::SessionNodeRecord {
                node_id: "first".to_string(),
                parent_node_id: None,
                timestamp: "2026-07-27T00:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "first".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
            crate::SessionNodeRecord {
                node_id: "last".to_string(),
                parent_node_id: Some("first".to_string()),
                timestamp: "2026-07-27T00:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "last".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
        ],
        leaf_node_id: Some("first".to_string()),
    };
    assert!(matches!(
        graph.validate_append_topology(),
        Err(StoreError::InvalidGraphLeaf {
            leaf_node_id: Some(leaf)
        }) if leaf == "first"
    ));
}

#[test]
fn lease_owner_identity_requires_same_incarnation() {
    let first = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
    let same = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
    let next = LeaseOwnerIdentity::opaque("owner", "incarnation-b");

    assert!(first.same_incarnation(&same));
    assert!(!first.same_incarnation(&next));
}
