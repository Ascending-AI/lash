use super::*;
use crate::facade_support::AgentFrameReasonFacadeOps;
use crate::{MessageRole, Part, shared_parts};

fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            content.to_string(),
            None,
        )]),
        origin: None,
    }
}

#[test]
fn construction_enforces_structural_graph_integrity() {
    let node = |id: &str, parent: Option<&str>| SessionNodeRecord {
        node_id: id.to_string(),
        parent_node_id: parent.map(str::to_string),
        timestamp: "2026-08-08T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "construction-integrity-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"id": id})),
        },
    };

    assert!(matches!(
        SessionGraph::from_nodes(
            vec![node("duplicate", None), node("duplicate", None)],
            Some("duplicate".to_string()),
        ),
        Err(crate::StoreError::NodeIdCollision { node_id }) if node_id == "duplicate"
    ));
    assert!(matches!(
        SessionGraph::from_nodes(
            vec![node("orphan", Some("missing-parent"))],
            Some("orphan".to_string()),
        ),
        Err(crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        }) if node_id == "orphan" && parent == "missing-parent"
    ));
    assert!(matches!(
        SessionGraph::from_nodes(vec![node("present", None)], Some("missing-leaf".to_string())),
        Err(crate::StoreError::InvalidGraphLeaf {
            leaf_node_id: Some(leaf)
        }) if leaf == "missing-leaf"
    ));
    assert!(matches!(
        SessionGraph::from_nodes(
            vec![
                node("cycle-a", Some("cycle-b")),
                node("cycle-b", Some("cycle-a")),
            ],
            None,
        ),
        Err(crate::StoreError::InvalidGraphParent { .. })
    ));

    let leafless = SessionGraph::from_nodes(
        vec![
            node("catalog-root", None),
            node("catalog-child", Some("catalog-root")),
        ],
        None,
    )
    .expect("structurally valid leafless catalogs are constructible");
    assert_eq!(leafless.nodes.len(), 2);
    assert!(matches!(
        leafless.validate_resident_integrity(),
        Err(crate::StoreError::InvalidGraphLeaf { leaf_node_id: None })
    ));
}

#[test]
fn cache_build_rejects_parent_cycles() {
    const CHILD_ENV: &str = "LASH_FIG843_CYCLE_CACHE_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        cache_build_rejects_parent_cycles_scenario();
        return;
    }

    crate::test_watchdog::assert_exact_test_completes(
        "session_graph::tests::cache_build_rejects_parent_cycles",
        CHILD_ENV,
        "session graph cycle check",
    );
}

fn cache_build_rejects_parent_cycles_scenario() {
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![
            SessionNodeRecord {
                node_id: "cycle-a".to_string(),
                parent_node_id: Some("cycle-b".to_string()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cycle-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "cycle-b".to_string(),
                parent_node_id: Some("cycle-a".to_string()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cycle-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "b"})),
                },
            },
        ],
        Some("cycle-b".to_string()),
    );

    let error = SessionGraphCache::build(&graph).expect_err("cycle must be rejected");
    assert!(matches!(
        error,
        crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        } if node_id == "cycle-b" && parent == "cycle-a"
    ));
}

#[test]
fn cache_build_rejects_duplicate_node_ids() {
    let node = SessionNodeRecord {
        node_id: "duplicate".to_string(),
        parent_node_id: None,
        timestamp: "2026-07-31T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "duplicate-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"value": 1})),
        },
    };
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![node.clone(), node],
        Some("duplicate".to_string()),
    );

    assert!(matches!(
        SessionGraphCache::build(&graph),
        Err(crate::StoreError::NodeIdCollision { node_id }) if node_id == "duplicate"
    ));
}

#[test]
fn cache_build_rejects_dangling_parents() {
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![SessionNodeRecord {
            node_id: "dangling-child".to_string(),
            parent_node_id: Some("missing-parent".to_string()),
            timestamp: "2026-07-31T00:00:00Z".to_string(),
            payload: SessionNodePayload::Plugin {
                plugin_type: "dangling-test".to_string(),
                body: SharedJsonValue::new(serde_json::json!({"value": 1})),
            },
        }],
        Some("dangling-child".to_string()),
    );

    assert!(matches!(
        graph.validate_resident_integrity(),
        Err(crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        }) if node_id == "dangling-child" && parent == "missing-parent"
    ));
}

#[test]
fn resident_integrity_rejects_missing_leaves() {
    let node = SessionNodeRecord {
        node_id: "existing-node".to_string(),
        parent_node_id: None,
        timestamp: "2026-07-31T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "leaf-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"value": 1})),
        },
    };
    let unknown_leaf = SessionGraph::from_unchecked_nodes_for_testing(
        vec![node.clone()],
        Some("missing-leaf".to_string()),
    );
    let absent_leaf = SessionGraph::from_unchecked_nodes_for_testing(vec![node], None);

    assert!(matches!(
        unknown_leaf.validate_resident_integrity(),
        Err(crate::StoreError::InvalidGraphLeaf {
            leaf_node_id: Some(leaf)
        }) if leaf == "missing-leaf"
    ));
    assert!(matches!(
        absent_leaf.validate_resident_integrity(),
        Err(crate::StoreError::InvalidGraphLeaf { leaf_node_id: None })
    ));
}

#[test]
fn cache_build_rejects_cycles_in_inactive_components() {
    let plugin_node = |node_id: &str, parent_node_id: Option<&str>| SessionNodeRecord {
        node_id: node_id.to_string(),
        parent_node_id: parent_node_id.map(str::to_string),
        timestamp: "2026-07-31T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "inactive-cycle-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"node": node_id})),
        },
    };
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![
            plugin_node("active-root", None),
            plugin_node("active-leaf", Some("active-root")),
            plugin_node("inactive-a", Some("inactive-b")),
            plugin_node("inactive-b", Some("inactive-a")),
        ],
        Some("active-leaf".to_string()),
    );

    assert!(matches!(
        graph.validate_resident_integrity(),
        Err(crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        }) if node_id == "inactive-b" && parent == "inactive-a"
    ));
}

#[test]
fn nearest_ancestor_walk_is_bounded_on_a_parent_cycle() {
    const CHILD_ENV: &str = "LASH_FIG843_NEAREST_ANCESTOR_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        nearest_ancestor_walk_is_bounded_scenario();
        return;
    }

    crate::test_watchdog::assert_exact_test_completes(
        "session_graph::tests::nearest_ancestor_walk_is_bounded_on_a_parent_cycle",
        CHILD_ENV,
        "nearest-ancestor cycle check",
    );
}

fn nearest_ancestor_walk_is_bounded_scenario() {
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![
            SessionNodeRecord {
                node_id: "nearest-a".to_string(),
                parent_node_id: Some("nearest-b".to_string()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "nearest-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "nearest-b".to_string(),
                parent_node_id: Some("nearest-a".to_string()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "nearest-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "b"})),
                },
            },
        ],
        Some("nearest-b".to_string()),
    );
    let by_id = graph_node_indices(&graph).expect("unique test ids");

    assert!(matches!(
        nearest_ancestor_index(&graph, &by_id, Some("nearest-b"), |_| false),
        Err(crate::StoreError::InvalidGraphParent { .. })
    ));
}

fn protocol_event() -> ProtocolEvent {
    ProtocolEvent::typed("test_protocol", serde_json::json!({"step": "started"}))
        .expect("protocol event serializes")
}

#[test]
fn draft_node_ids_are_opaque_distinct_and_ignore_message_ids() {
    let mut graph = SessionGraph::default();

    let message_id = graph.append_message(text_message("m1", MessageRole::User, "hello"));
    let protocol_id = graph.append_protocol_event(protocol_event());
    let plugin_id = graph.append_plugin("example", serde_json::json!({"ok": true}));

    assert_ne!(message_id, "m1");
    assert!(message_id.starts_with("draft-node/v2/"));
    assert!(protocol_id.starts_with("draft-node/v2/"));
    assert!(plugin_id.starts_with("draft-node/v2/"));
    assert_ne!(message_id, protocol_id);
    assert_ne!(protocol_id, plugin_id);
}

#[test]
fn draft_node_ids_are_stable_per_boundary_and_distinct_across_boundaries() {
    let graph = SessionGraph::default();
    let message = text_message("same-message", MessageRole::User, "hello");
    let timestamp = "2026-07-26T10:00:00Z".to_string();

    let mut first = graph.append_builder_in_namespace("turn:one");
    let first_id = first.append_messages_at([message.clone()], timestamp.clone())[0]
        .node_id
        .clone();
    let mut replay = graph.append_builder_in_namespace("turn:one");
    let replay_id = replay.append_messages_at([message.clone()], timestamp.clone())[0]
        .node_id
        .clone();
    let mut next_turn = graph.append_builder_in_namespace("turn:two");
    let next_turn_id = next_turn.append_messages_at([message], timestamp)[0]
        .node_id
        .clone();

    assert_eq!(first_id, replay_id);
    assert_ne!(first_id, next_turn_id);
}

#[test]
fn read_model_preserves_distinct_nodes_with_identical_messages() {
    let mut graph = SessionGraph::default();
    let message = text_message("same-message-id", MessageRole::User, "same content");

    let first = graph.append_message(message.clone());
    let second = graph.append_message(message);

    assert_ne!(first, second);
    let read = graph.read_model();
    assert_eq!(read.messages.len(), 2);
    assert_eq!(read.messages[0].id, "same-message-id");
    assert_eq!(read.messages[1].id, "same-message-id");
}

#[test]
fn storage_body_excludes_indexed_graph_identity_and_parent_edge() {
    let node = SessionNodeRecord {
        node_id: "node-2".to_string(),
        parent_node_id: Some("node-1".to_string()),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };

    let encoded = node.encode_storage_body().expect("encode storage body");
    assert!(!encoded.contains("node_id"));
    assert!(!encoded.contains("parent_node_id"));
    let decoded = SessionNodeRecord::decode_storage_body(
        node.node_id.clone(),
        node.parent_node_id.clone(),
        &encoded,
    )
    .expect("decode storage body");

    assert_eq!(decoded.node_id, node.node_id);
    assert_eq!(decoded.parent_node_id, node.parent_node_id);
    assert_eq!(decoded.timestamp, node.timestamp);
    assert!(matches!(decoded.payload, SessionNodePayload::Event { .. }));
}

#[test]
fn nearest_frame_is_derived_from_ancestry() {
    let assignment = crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
    ));
    let mut graph = SessionGraph::default();
    let first = frame_node_id("session", "first-frame");
    assert!(graph.append_frame_open_with_id_at(
        first.clone(),
        "first-frame".to_string(),
        crate::AgentFrameReason::initial(),
        assignment.clone(),
        crate::ProtocolTurnOptions::default(),
        "2026-07-27T00:00:00Z".to_string(),
    ));
    let first_message = graph.append_message(text_message("m1", MessageRole::User, "first"));
    let second = frame_node_id("session", "second-frame");
    assert!(graph.append_frame_open_with_id_at(
        second.clone(),
        "second-frame".to_string(),
        crate::AgentFrameReason::continue_as(),
        assignment,
        crate::ProtocolTurnOptions::default(),
        "2026-07-27T00:00:01Z".to_string(),
    ));
    let second_message = graph.append_message(text_message("m2", MessageRole::User, "second"));

    assert_eq!(
        graph.nearest_frame_node_id(Some(&first_message)),
        Some(first.as_str())
    );
    assert_eq!(
        graph.nearest_frame_node_id(Some(&second_message)),
        Some(second.as_str())
    );
    assert_eq!(
        graph.nearest_frame_node_id(graph.leaf_node_id.as_deref()),
        Some(second.as_str())
    );
}

#[test]
fn message_tree_marks_active_nodes_without_using_message_identity() {
    let mut graph = SessionGraph::default();
    let message = text_message("same-message-id", MessageRole::User, "same content");
    let root = graph.append_message(message.clone());
    let inactive = graph.append_message(message.clone());
    graph.set_leaf_node_id(Some(root));
    let active = graph.append_message(message);

    let tree = graph.message_tree();
    assert_eq!(tree.len(), 1);
    assert!(tree[0].active);
    assert_eq!(tree[0].children.len(), 2);
    assert_eq!(tree[0].children[0].node_id, inactive);
    assert!(!tree[0].children[0].active);
    assert_eq!(tree[0].children[1].node_id, active);
    assert!(tree[0].children[1].active);
}

#[test]
fn active_read_replacement_persists_messages_only() {
    let message = text_message("m1", MessageRole::User, "hello");
    let graph = SessionGraph::from_active_read_state(&[message]);

    assert_eq!(graph.nodes.len(), 1);
    assert!(matches!(
        graph.nodes[0].event(),
        Some(SessionHistoryRecord::Conversation(_))
    ));
}

#[test]
fn graph_writers_keep_payload_kind_out_of_draft_identity() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "hello"));
    graph.append_protocol_event(protocol_event());
    graph.append_plugin("example", serde_json::json!({"ok": true}));

    for node in &graph.nodes {
        assert!(node.node_id.starts_with("draft-node/v2/"), "{:?}", node);
    }
}
