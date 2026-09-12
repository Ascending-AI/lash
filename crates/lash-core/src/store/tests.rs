use super::*;
use crate::facade_support::AgentFrameReasonFacadeOps;

fn test_message(id: &str) -> crate::Message {
    crate::Message {
        id: id.to_string(),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part::text(
            format!("{id}.p0"),
            "test message".to_string(),
            None,
        )]),
        origin: None,
    }
}

fn state_with_persisted_initial_frame(session_id: &str) -> crate::RuntimeSessionState {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from(session_id),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state.mark_node_ids_persisted(
        state
            .session_graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>(),
    );
    state
}

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
        session_id: SessionId::from("golden-session"),
        turn_index: 7,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.data_mut().nodes[0].timestamp = "2026-07-26T10:00:00Z".to_string();
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

#[test]
fn claim_settlement_refuses_foreign_completions_in_both_directions() {
    let mut commit = intent_fixture();
    commit.completed_queue_claims = vec![crate::QueuedWorkCompletion {
        session_id: commit.session_id.clone(),
        claim_id: "foreign-queue".to_string(),
        lease_token: "token".to_string(),
        data: crate::QueuedWorkCompletionData {
            batch_ids: vec!["batch".to_string()],
        },
    }];
    let queue_error = commit
        .validate_claim_settlement(&[], &[])
        .expect_err("foreign queued-work completion must be refused");
    assert!(matches!(
        queue_error,
        StoreError::ForeignQueuedWorkCompletion { ref claim_id, .. }
            if claim_id == "foreign-queue"
    ));

    commit.completed_queue_claims.clear();
    commit.completed_turn_input_claims = vec![crate::TurnInputCompletion {
        session_id: commit.session_id.clone(),
        claim: Some(crate::TurnInputSettlementClaim {
            claim_id: "foreign-input".to_string(),
            lease_token: "token".to_string(),
        }),
        data: crate::TurnInputCompletionData {
            input_ids: vec!["input".to_string()],
            applications: Vec::new(),
        },
    }];
    let input_error = commit
        .validate_claim_settlement(&[], &[])
        .expect_err("foreign turn-input completion must be refused");
    assert!(matches!(
        input_error,
        StoreError::ForeignTurnInputCompletion { ref claim_id, .. }
            if claim_id == "foreign-input"
    ));
}

#[test]
fn claim_settlement_refuses_duplicate_completion_count() {
    let mut commit = intent_fixture();
    let completion = crate::QueuedWorkCompletion {
        session_id: commit.session_id.clone(),
        claim_id: "originating-queue".to_string(),
        lease_token: "token".to_string(),
        data: crate::QueuedWorkCompletionData {
            batch_ids: vec!["batch".to_string()],
        },
    };
    commit.completed_queue_claims = vec![completion.clone(), completion.clone()];

    let error = commit
        .validate_claim_settlement(&[completion], &[])
        .expect_err("duplicate queued-work completion must be refused");
    assert!(matches!(
        error,
        StoreError::ClaimSettlementCountMismatch {
            claim_kind: "queued-work",
            originating_count: 1,
            completed_count: 2,
        }
    ));
}

#[test]
fn first_persisted_state_commit_derives_and_installs_node_ids() {
    let placeholder = "draft-node/v2:first".to_string();
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from("first-commit"),
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
        )
        .expect("first-commit fixture graph is valid"),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
fn commit_frame_derivation_reads_resident_parent_for_temporary_append_nodes() {
    let mut state = state_with_persisted_initial_frame("temporary-frame-derivation");
    let current_frame_node_id = state
        .current_frame_node_id
        .clone()
        .expect("initial frame node id");
    let temporary_node_id = state
        .session_graph
        .append_message(test_message("temporary"));
    assert!(temporary_node_id.starts_with("draft-node/v3/"));
    let graph = state.pending_graph_commit();
    assert_eq!(graph.nodes[0].node_id, temporary_node_id);
    assert!(state.session_graph.find_node(&temporary_node_id).is_some());

    let commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        OperationId::turn(&state.session_id, "turn-1", "final"),
    )
    .expect("derive frame from resident parent");

    assert_eq!(commit.current_frame_node_id, Some(current_frame_node_id));
}

#[test]
fn commit_frame_derivation_reads_resident_parent_for_derived_append_nodes() {
    let mut state = state_with_persisted_initial_frame("derived-frame-derivation");
    let current_frame_node_id = state
        .current_frame_node_id
        .clone()
        .expect("initial frame node id");
    let temporary_node_id = state.session_graph.append_message(test_message("derived"));
    let operation = OperationId::turn(&state.session_id, "turn-1", "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive final append ids");
    assert_ne!(graph.nodes[0].node_id, temporary_node_id);
    assert!(state.session_graph.find_node(&temporary_node_id).is_some());
    assert!(
        state
            .session_graph
            .find_node(&graph.nodes[0].node_id)
            .is_none()
    );

    let commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        operation,
    )
    .expect("derive frame from resident parent");

    assert_eq!(commit.current_frame_node_id, Some(current_frame_node_id));
}

#[test]
fn commit_frame_derivation_uses_last_frame_boundary_inside_append() {
    let mut state = state_with_persisted_initial_frame("appended-frame-derivation");
    state
        .session_graph
        .append_message(test_message("before-boundary"));
    let frame_key = crate::FrameKey::from_caller_material("second-frame")
        .expect("non-empty frame key material");
    let appended_frame_node_id =
        crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    assert!(state.session_graph.append_frame_open_with_id_at(
        appended_frame_node_id.clone(),
        frame_key,
        crate::AgentFrameReason::continue_as(),
        crate::AgentFrameAssignment::from_policy(state.policy.clone()),
        state.protocol_turn_options.clone(),
        "2026-09-12T00:00:00Z".to_string(),
    ));
    state
        .session_graph
        .append_message(test_message("after-boundary"));

    let commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        state.pending_graph_commit(),
        &[],
        OperationId::turn(&state.session_id, "turn-1", "final"),
    )
    .expect("derive appended frame boundary");

    assert_eq!(commit.current_frame_node_id, Some(appended_frame_node_id));
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
        session_id: SessionId::from("golden-session"),
        claim: Some(crate::TurnInputSettlementClaim {
            claim_id: "claim-a".to_string(),
            lease_token: "lease-a".to_string(),
        }),
        data: crate::TurnInputCompletionData {
            input_ids: vec!["input-1".to_string()],
            applications: vec![crate::TurnInputApplication {
                input_id: "input-1".to_string(),
                source_key: None,
                turn_id: crate::TurnId::from("turn-42"),
                committed_message_id: "random-attempt-a".to_string(),
                checkpoint: None,
            }],
        },
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
    // Checkpoint manifest v3 and explicit ambient tool access are pinned in intent bytes.
    assert_eq!(
        intent_fixture().turn_commit_hash().expect("golden intent"),
        "d66a62e305da062c361f45ad0fdba8566cec0c85808a387c4f3b68a334a8f0fd"
    );
}

#[test]
fn failure_evidence_changes_intent_hash_without_changing_empty_legacy_hash() {
    let baseline = intent_fixture();
    let baseline_hash = baseline.turn_commit_hash().expect("baseline intent");
    assert_eq!(
        baseline_hash,
        "d66a62e305da062c361f45ad0fdba8566cec0c85808a387c4f3b68a334a8f0fd"
    );

    let mut with_evidence = baseline;
    with_evidence.failure_evidence = vec![crate::TurnFailureEvidence {
        partial_output: None,
        billed_usage: crate::llm::types::LlmUsage::default(),
        refusal: crate::ChargeSafetyRefusalEvidence {
            code: "unsafe_retry_after_output_started".to_string(),
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
fn session_head_payload_bytes_match_the_legacy_meta_format() {
    #[allow(dead_code)]
    #[derive(serde::Serialize)]
    struct LegacySessionHeadMeta {
        schema_version: u32,
        #[serde(default = "super::default_root_session_id")]
        session_id: SessionId,
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
        session_id: SessionId::from("column-owned-head"),
        head_revision: 41,
        config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
        current_frame_node_id: None,
        checkpoint_ref: Some(BlobRef("checkpoint".to_string())),
        leaf_node_id: Some("leaf".to_string()),
    };
    let assembled = SessionHeadMeta::assemble(
        SessionHeadPayload {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: SessionId::from("column-owned-head"),
            config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
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
        session_id: SessionId::from("root"),
        operation_key: "operation-key".to_string(),
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
    let GraphAppend { nodes, .. } = &mut rogue.graph;
    nodes[0].node_id = "rogue".to_string();
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
    let GraphAppend { nodes, .. } = &mut commit.graph;
    nodes[0].payload = crate::SessionNodePayload::FrameOpen {
        frame_key,
        reason: crate::AgentFrameReason::initial(),
        assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        )),
        protocol_turn_options: crate::ProtocolTurnOptions::default(),
    };

    assert!(matches!(
        commit.validate_node_derivation(),
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
        .derive_node_ids(&SessionId::from("session"), &operation)
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
    let frame_key =
        crate::FrameKey::from_caller_material("initial-frame").expect("non-empty frame material");
    let frame_node_id =
        crate::session_graph::frame_node_id(&SessionId::from("session"), frame_key.as_str());
    let mut graph = GraphAppend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: frame_node_id.to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::FrameOpen {
                frame_key,
                reason: crate::AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
                protocol_turn_options: crate::ProtocolTurnOptions::default(),
            },
        }],
        leaf_node_id: Some(frame_node_id.to_string()),
    };

    graph
        .derive_node_ids(&SessionId::from("session"), &operation)
        .expect("realize frame node");

    let GraphAppend {
        nodes,
        leaf_node_id,
    } = graph;
    assert_eq!(nodes[0].node_id, frame_node_id.as_str());
    assert_eq!(leaf_node_id.as_deref(), Some(frame_node_id.as_str()));
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
