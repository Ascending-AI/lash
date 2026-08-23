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
fn storage_body_states_its_node_body_generation() {
    let node = SessionNodeRecord {
        node_id: "node-1".to_string(),
        parent_node_id: None,
        timestamp: "2026-08-18T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };

    let encoded = node.encode_storage_body().expect("encode storage body");
    let stamped: serde_json::Value = serde_json::from_str(&encoded).expect("stored body is JSON");

    assert_eq!(
        stamped
            .get("schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(u64::from(SESSION_NODE_BODY_SCHEMA_VERSION)),
    );
}

#[test]
fn unstamped_stored_bodies_keep_loading() {
    // Byte-for-byte a body written before the generation stamp existed.
    let legacy = r#"{"timestamp":"2026-07-27T00:00:00Z","kind":"plugin","plugin_type":"legacy","body":{"value":7}}"#;

    let decoded = SessionNodeRecord::decode_storage_body("node-1".to_string(), None, legacy)
        .expect("pre-stamp durable bodies must keep loading");

    assert_eq!(decoded.timestamp, "2026-07-27T00:00:00Z");
    match decoded.payload {
        SessionNodePayload::Plugin { plugin_type, body } => {
            assert_eq!(plugin_type, "legacy");
            assert_eq!(body.as_ref(), &serde_json::json!({"value": 7}));
        }
        other => panic!("unexpected payload: {other:?}"),
    }
}

/// The pre-stamp shape of the richest body the durable-read fixture carried,
/// frozen here because the fixture cannot hold it permanently.
///
/// Until FIG-1536 the fixture's own `graph_nodes` rows were unstamped, so the
/// default-on-read path had an incidental durable-read exercise. That was never
/// durable coverage: a fixture is regenerated by the current writer, so the
/// first regeneration after #486 re-stamped every row and the exercise
/// evaporated — which is exactly what happened. A frozen literal is where a
/// *legacy* shape can actually be kept, and this one is a conversation node with
/// message parts rather than the plugin node above, because that is the shape
/// whose fields the flattened payload family reaches furthest into.
#[test]
fn unstamped_conversation_bodies_keep_loading() {
    let legacy = r#"{"timestamp":"2023-11-14T22:13:20+00:00","kind":"event","event":{"Conversation":{"id":"m_legacy","role":"User","parts":[{"id":"m_legacy.p0","kind":"Text","content":"durable read user message","prune_state":"Intact"}]}}}"#;

    let decoded = SessionNodeRecord::decode_storage_body("node-1".to_string(), None, legacy)
        .expect("a pre-stamp conversation body must keep loading");

    assert_eq!(decoded.timestamp, "2023-11-14T22:13:20+00:00");
    // Re-encoding stamps it, which is the whole reason a fixture cannot keep an
    // unstamped row: the next writer to touch it makes it current.
    let restamped = decoded.encode_storage_body().expect("re-encode");
    let restamped: serde_json::Value =
        serde_json::from_str(&restamped).expect("re-encoded body is JSON");
    assert_eq!(
        restamped["schema_version"],
        serde_json::json!(SESSION_NODE_BODY_SCHEMA_VERSION),
        "an unstamped body re-encodes at this build's generation: {restamped}"
    );
}

#[test]
fn stored_bodies_from_a_newer_generation_are_refused() {
    let newer = serde_json::json!({
        "schema_version": SESSION_NODE_BODY_SCHEMA_VERSION + 1,
        "timestamp": "2026-08-18T00:00:00Z",
        "kind": "plugin",
        "plugin_type": "from-the-future",
        "body": {},
    })
    .to_string();

    let error = SessionNodeRecord::decode_storage_body("node-1".to_string(), None, &newer)
        .expect_err("a newer node-body generation must be refused");

    assert_eq!(
        error.to_string(),
        format!(
            "graph node body is schema version {}, but this build reads at most {}; remedy: \
             run a Lash build at or past that node-body generation",
            SESSION_NODE_BODY_SCHEMA_VERSION + 1,
            SESSION_NODE_BODY_SCHEMA_VERSION
        ),
    );
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
fn projection_and_replacement_retain_the_same_prefix() {
    let first = text_message("m1", MessageRole::User, "first");
    let second = text_message("m2", MessageRole::Assistant, "second");
    let mut transient = text_message("mt", MessageRole::User, "transient");
    transient.origin = Some(crate::MessageOrigin::Plugin {
        plugin_id: "prefix-test".to_string(),
        transient: true,
    });

    let mut graph = SessionGraph::default();
    graph.append_message(first.clone());
    graph.append_message(transient.clone());
    graph.append_protocol_event(protocol_event());
    graph.append_message(second.clone());
    let existing_ids = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<HashSet<_>>();
    let current_parts = graph
        .nodes
        .iter()
        .filter_map(|node| node.message().map(|message| message.parts))
        .collect::<Vec<_>>();

    let cases = [
        (
            "transient messages do not participate",
            vec![first.clone(), transient, second.clone()],
            2,
        ),
        (
            "a divergent second message stops after the first",
            vec![
                first.clone(),
                text_message("m2", MessageRole::Assistant, "changed"),
            ],
            1,
        ),
        (
            "a divergent first message retains nothing",
            vec![text_message("m1", MessageRole::User, "changed")],
            0,
        ),
        (
            "an exhausted target stops before the second message",
            vec![first],
            1,
        ),
    ];

    for (case, messages, expected) in cases {
        let replacement = build_active_read_replacement(
            graph.nodes.iter(),
            &existing_ids,
            "active-read-prefix-differential-test",
            &messages,
            "2026-08-20T00:00:00Z".to_string(),
        );
        let projection = build_active_read_projection(graph.nodes.iter(), &messages);
        let projection_retained_count = projection
            .active_messages
            .iter()
            .filter(|message| {
                current_parts
                    .iter()
                    .any(|parts| Arc::ptr_eq(parts, &message.parts))
            })
            .count();
        let replacement_retained_prefix_len = messages
            .iter()
            .filter(|message| !message.is_transient())
            .count()
            - replacement.new_tail_nodes.len();

        assert_eq!(projection_retained_count, expected, "{case}");
        assert_eq!(
            projection_retained_count, replacement_retained_prefix_len,
            "{case}"
        );
    }
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

/// The turn projection decides prefix agreement by comparing the `Arc` a read
/// model handed out, so two reads of the same frame on an unchanged graph must
/// hand out the *same* `Arc`, not two equal ones. Rebuilding the frame
/// projection per call is what forced the projection onto its whole-window
/// fallback on every turn boundary (FIG-1637).
#[test]
fn a_frame_read_model_is_shared_by_identity_until_the_active_path_moves() {
    let assignment = crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
    ));
    let mut graph = SessionGraph::default();
    let frame = frame_node_id("session", "frame");
    assert!(graph.append_frame_open_with_id_at(
        frame.clone(),
        "frame".to_string(),
        crate::AgentFrameReason::initial(),
        assignment,
        crate::ProtocolTurnOptions::default(),
        "2026-08-19T00:00:00Z".to_string(),
    ));
    graph.append_message(text_message("m1", MessageRole::User, "first"));

    let first = graph.read_model_for_frame(&frame);
    let second = graph.read_model_for_frame(&frame);
    assert!(
        Arc::ptr_eq(&first.messages, &second.messages),
        "repeated reads of one frame share the projected messages by identity"
    );
    assert!(Arc::ptr_eq(&first.active_events, &second.active_events));

    graph.append_message(text_message("m2", MessageRole::User, "second"));
    let after_append = graph.read_model_for_frame(&frame);
    assert!(
        !Arc::ptr_eq(&first.messages, &after_append.messages),
        "an append to the active path retires the memoized projection"
    );
    assert_eq!(after_append.messages.len(), 2);
}
