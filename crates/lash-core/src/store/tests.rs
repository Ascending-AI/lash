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
    semantic.expected_head_revision = None;
    semantic.session_execution_lease = None;
    semantic.release_session_execution_lease = None;
    semantic.turn_commit = None;
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
    state.agent_frames[0].created_at = "2026-07-26T10:00:00Z".to_string();
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    let node_id =
        derive_history_node_id("golden-session", &operation, 0).expect("derive golden node");
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
    let graph = GraphCommitDelta::Append {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.clone(),
            parent_node_id: None,
            caused_by: None,
            agent_frame_id: Some(state.current_agent_frame_id.clone()),
            timestamp: "2026-07-26T10:00:01Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Conversation(
                    crate::ConversationRecord::from_message(message),
                ),
            },
        }],
        leaf_node_id: Some(node_id),
    };
    RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[])
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
                caused_by: None,
                agent_frame_id: None,
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
    let expected = derive_history_node_id("first-commit", &operation, 0)
        .expect("derive expected first node id");

    let (commit, persisted_node_ids) =
        RuntimeCommit::persisted_state_with_operation(&mut state, &[], operation)
            .expect("build first append");
    let GraphCommitDelta::Append {
        nodes,
        leaf_node_id,
    } = commit.graph
    else {
        panic!("first graph write must be an append");
    };
    assert_eq!(persisted_node_ids, vec![expected.clone()]);
    assert_eq!(nodes[0].node_id, expected);
    assert_eq!(leaf_node_id, Some(nodes[0].node_id.clone()));
    assert_eq!(state.session_graph.nodes[0].node_id, nodes[0].node_id);
    assert_eq!(state.session_graph.leaf_node_id, leaf_node_id);
}

#[test]
fn legacy_hash_reproduces_created_at_replay_conflict() {
    let first = intent_fixture();
    let mut replay = first.clone();
    replay.agent_frames[0].created_at = "2026-07-26T10:00:09Z".to_string();

    assert_ne!(
        legacy_turn_commit_hash(&first),
        legacy_turn_commit_hash(&replay),
        "the pre-L2 scrubber hashes live frame creation time"
    );
    assert_eq!(
        first.turn_commit_hash().expect("first intent"),
        replay.turn_commit_hash().expect("replay intent"),
        "L2 excludes clock-derived frame time"
    );
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
        "fa9c57a925d8ac676acc4bd1a8405e8b69a5361823339eb354b24c56afc1286d"
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
        "n_1310484e23970c0e27cbb0934ec9615f021e546b3631f99541142ddea30d13c2"
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
        let GraphCommitDelta::Append { nodes, .. } = &mut commit.graph else {
            panic!("fixture is append");
        };
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
    let GraphCommitDelta::Append { nodes, .. } = &mut observed_later.graph else {
        panic!("fixture is append");
    };
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
        let GraphCommitDelta::Append { nodes, .. } = &mut commit.graph else {
            panic!("fixture is append");
        };
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
fn node_derivation_and_realization_digest_are_independent() {
    let mut commit = intent_fixture();
    let operation = OperationId::turn("golden-session", "turn-42", "final");
    let hash = commit.turn_commit_hash().expect("intent hash");
    commit.turn_commit = Some(RuntimeTurnCommitStamp::new(
        "golden-session",
        operation,
        hash,
    ));
    commit.validate_node_derivation().expect("derived proposal");

    let mut rogue = commit.clone();
    let GraphCommitDelta::Append { nodes, .. } = &mut rogue.graph else {
        panic!("fixture is append");
    };
    nodes[0].node_id = "rogue".to_string();
    assert!(matches!(
        rogue.validate_node_derivation(),
        Err(StoreError::NodeIdDerivationMismatch { .. })
    ));

    assert_ne!(
        graph_realization_digest(&commit.graph),
        graph_realization_digest(&rogue.graph),
        "realization digest must observe ids without invoking derivation"
    );
}

#[test]
fn node_derivation_remaps_in_batch_session_node_causes() {
    let operation = OperationId::turn("session", "turn", "final");
    let mut graph = GraphCommitDelta::Append {
        nodes: vec![
            crate::SessionNodeRecord {
                node_id: "draft-a".to_string(),
                parent_node_id: None,
                caused_by: None,
                agent_frame_id: None,
                timestamp: "2026-07-26T10:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Plugin {
                    plugin_type: "first".to_string(),
                    body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
                },
            },
            crate::SessionNodeRecord {
                node_id: "draft-b".to_string(),
                parent_node_id: Some("draft-a".to_string()),
                caused_by: Some(crate::CausalRef::SessionNode {
                    session_id: "session".to_string(),
                    node_id: "draft-a".to_string(),
                }),
                agent_frame_id: None,
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
    let GraphCommitDelta::Append { nodes, .. } = graph else {
        panic!("fixture is append");
    };
    assert_eq!(
        nodes[1].parent_node_id.as_deref(),
        Some(nodes[0].node_id.as_str())
    );
    assert!(matches!(
        &nodes[1].caused_by,
        Some(crate::CausalRef::SessionNode {
            session_id,
            node_id,
        }) if session_id == "session" && node_id == &nodes[0].node_id
    ));
}

fn local_liveness(
    host_id: &str,
    boot_id: &str,
    pid: u32,
    process_start: &str,
) -> LeaseOwnerLiveness {
    LeaseOwnerLiveness::local_process_for_test(host_id, boot_id, pid, process_start)
}

#[test]
fn lease_owner_identity_requires_same_incarnation() {
    let first = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
    let same = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
    let next = LeaseOwnerIdentity::opaque("owner", "incarnation-b");

    assert!(first.same_incarnation(&same));
    assert!(!first.same_incarnation(&next));
}

#[test]
fn local_liveness_only_proves_same_host_boot_dead_processes() {
    let holder = local_liveness(
        "host-a",
        "boot-a",
        std::process::id(),
        "not-the-current-process-start",
    );
    let same_host_boot = local_liveness("host-a", "boot-a", std::process::id(), "claimant");
    let other_host = local_liveness("host-b", "boot-a", std::process::id(), "claimant");
    let other_boot = local_liveness("host-a", "boot-b", std::process::id(), "claimant");

    assert!(holder.is_definitely_dead_for_claimant(&same_host_boot));
    assert!(!holder.is_definitely_dead_for_claimant(&other_host));
    assert!(!holder.is_definitely_dead_for_claimant(&other_boot));
    assert!(!holder.is_definitely_dead_for_claimant(&LeaseOwnerLiveness::Opaque));
    assert!(!LeaseOwnerLiveness::Opaque.is_definitely_dead_for_claimant(&same_host_boot));
}
