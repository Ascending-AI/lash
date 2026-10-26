// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::facade_support::AgentFrameReasonFacadeOps;
use crate::{GraphAppend, MessageRole, Part, shared_parts};

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
        reply_marker: None,
    }
}

#[test]
fn construction_enforces_structural_graph_integrity() {
    let node = |id: &str, parent: Option<&str>| SessionNodeRecord {
        node_id: NodeId::fixture(id.to_string()),
        parent_node_id: parent.map(lash_vocabulary::identity::NodeId::fixture),
        timestamp: "2026-08-08T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Plugin {
            plugin_type: "construction-integrity-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"id": id})),
        },
    };

    let mut blank_encoded = serde_json::to_value(SessionGraph::from_unchecked_nodes_for_testing(
        vec![node("only", None)],
        Some(NodeId::from("only")),
    ))
    .unwrap();
    blank_encoded["nodes"][0]["node_id"] = serde_json::Value::String(String::new());
    assert!(
        serde_json::from_value::<SessionGraph>(blank_encoded)
            .expect_err("a serialized graph with a blank node id decodes to no graph")
            .to_string()
            .contains("id must not be empty or whitespace-only")
    );
    assert!(matches!(
        SessionGraph::from_nodes(
            vec![node("duplicate", None), node("duplicate", None)],
            Some("duplicate".into()),
        ),
        Err(crate::StoreError::NodeIdCollision { node_id }) if node_id == "duplicate"
    ));
    assert!(matches!(
        SessionGraph::from_nodes(
            vec![node("orphan", Some("missing-parent"))],
            Some("orphan".into()),
        ),
        Err(crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        }) if node_id == "orphan" && parent == "missing-parent"
    ));
    assert!(matches!(
        SessionGraph::from_nodes(vec![node("present", None)], Some("missing-leaf".into())),
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
fn rejected_graph_appends_leave_nodes_leaf_and_cached_reads_unchanged() {
    let mut graph = SessionGraph::from_active_read_state(&[text_message(
        "resident-message",
        MessageRole::User,
        "resident",
    )]);
    let resident_leaf = graph.leaf_node_id.clone().expect("resident leaf");
    let before_graph = serde_json::to_value(&graph).expect("serialize graph preimage");
    let before_read = graph.read_model();
    let node = |node_id: &str, parent_node_id: &str| SessionNodeRecord {
        node_id: NodeId::fixture(node_id.to_string()),
        parent_node_id: Some(NodeId::fixture(parent_node_id.to_string())),
        timestamp: "2026-09-12T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Plugin {
            plugin_type: "atomic-append-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"node": node_id})),
        },
    };

    let duplicate = GraphAppend::Extend {
        nodes: vec![node(&resident_leaf, &resident_leaf)],
    };
    assert!(matches!(
        graph.apply_append(&duplicate),
        Err(crate::StoreError::NodeIdCollision { node_id }) if node_id == resident_leaf
    ));

    let duplicate_batch = GraphAppend::Extend {
        nodes: vec![
            node("duplicate-batch", &resident_leaf),
            node("duplicate-batch", "duplicate-batch"),
        ],
    };
    assert!(matches!(
        graph.apply_append(&duplicate_batch),
        Err(crate::StoreError::NodeIdCollision { node_id }) if node_id == "duplicate-batch"
    ));

    let invalid = GraphAppend::Extend {
        nodes: vec![node("invalid-child", "missing-parent")],
    };
    assert!(matches!(
        graph.apply_append(&invalid),
        Err(crate::StoreError::InvalidGraphParent {
            node_id,
            actual: Some(parent),
            ..
        }) if node_id == "invalid-child" && parent == "missing-parent"
    ));

    assert_eq!(
        serde_json::to_value(&graph).expect("serialize graph after refusals"),
        before_graph
    );
    let after_read = graph.read_model();
    assert!(lash_protocol_types::AppendVec::ptr_eq(
        &before_read.active_events,
        &after_read.active_events
    ));
    assert!(lash_protocol_types::AppendVec::ptr_eq(
        &before_read.messages,
        &after_read.messages
    ));
    assert!(std::sync::Arc::ptr_eq(
        &before_read.prompt_render_cache,
        &after_read.prompt_render_cache
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
                node_id: "cycle-a".into(),
                parent_node_id: Some("cycle-b".into()),
                timestamp: "2026-07-31T00:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cycle-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "cycle-b".into(),
                parent_node_id: Some("cycle-a".into()),
                timestamp: "2026-07-31T00:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cycle-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "b"})),
                },
            },
        ],
        Some("cycle-b".into()),
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
fn cache_build_rejects_cycles_in_inactive_components() {
    let plugin_node = |node_id: &str, parent_node_id: Option<&str>| SessionNodeRecord {
        node_id: NodeId::fixture(node_id.to_string()),
        parent_node_id: parent_node_id.map(lash_vocabulary::identity::NodeId::fixture),
        timestamp: "2026-07-31T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
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
        Some("active-leaf".into()),
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
                node_id: "nearest-a".into(),
                parent_node_id: Some("nearest-b".into()),
                timestamp: "2026-07-31T00:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "nearest-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "nearest-b".into(),
                parent_node_id: Some("nearest-a".into()),
                timestamp: "2026-07-31T00:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "nearest-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "b"})),
                },
            },
        ],
        Some("nearest-b".into()),
    );
    let by_id = graph_node_indices(&graph).expect("unique test ids");

    assert!(matches!(
        nearest_ancestor_index(
            &graph,
            |node_id| by_id.get(node_id).copied(),
            Some("nearest-b"),
            |_| false
        ),
        Err(crate::StoreError::InvalidGraphParent { .. })
    ));
}

fn protocol_event() -> ProtocolEvent {
    ProtocolEvent::typed("test_protocol", serde_json::json!({"step": "started"}))
        .expect("protocol event serializes")
}

#[test]
fn draft_node_ids_are_stable_per_boundary_and_distinct_across_boundaries() {
    let graph = SessionGraph::default();
    let message = text_message("same-message", MessageRole::User, "hello");
    let timestamp = "2026-07-26T10:00:00.000000000Z"
        .parse()
        .expect("canonical node timestamp");

    let mut first = graph.append_builder_in_namespace("turn:one");
    let first_id = first.append_messages_at([message.clone()], timestamp)[0]
        .node_id
        .clone();
    let mut replay = graph.append_builder_in_namespace("turn:one");
    let replay_id = replay.append_messages_at([message.clone()], timestamp)[0]
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
        node_id: "node-2".into(),
        parent_node_id: Some("node-1".into()),
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };

    let encoded = node
        .encode_storage_body(crate::store::FleetFormat::current())
        .expect("encode storage body");
    assert!(!encoded.contains("node_id"));
    assert!(!encoded.contains("parent_node_id"));
    let decoded = SessionNodeRecord::decode_storage_body(
        node.node_id.to_string(),
        node.parent_node_id
            .clone()
            .map(lash_vocabulary::identity::NodeId::into_inner),
        &encoded,
    )
    .expect("decode storage body");

    assert_eq!(decoded.node_id, node.node_id);
    assert_eq!(decoded.parent_node_id, node.parent_node_id);
    assert_eq!(decoded.timestamp, node.timestamp);
    assert!(matches!(decoded.payload, SessionNodePayload::Event { .. }));
}

#[test]
fn unstamped_stored_bodies_are_refused() {
    // Byte-for-byte a body written before the generation stamp existed.
    let legacy = r#"{"timestamp":"2026-07-27T00:00:00.000000000Z","kind":"plugin","plugin_type":"legacy","body":{"value":7}}"#;

    let error = SessionNodeRecord::decode_storage_body("node-1".to_string(), None, legacy)
        .expect_err("pre-stamp durable bodies are pre-cutover data and must be refused");

    assert_eq!(
        error.to_string(),
        format!(
            "graph node body carries no schema_version stamp; this build reads generation \
             {SESSION_NODE_BODY_SCHEMA_VERSION} and the fleet's recorded \
             {SESSION_NODE_BODY_SCHEMA_VERSION} (FIG-3796); remedy: the body is pre-cutover \
             data, so recreate the session store under this build"
        ),
    );
}

#[test]
fn stored_bodies_below_the_supported_generation_are_refused() {
    let fleet = crate::store::FleetFormat::current();
    let window = fleet.read_window(crate::surface_format!(SESSION_NODE_BODY_SCHEMA_VERSION));
    let unsupported_generation = window.oldest() - 1;
    assert!(!window.admits(unsupported_generation));
    let node = SessionNodeRecord {
        node_id: "node-1".into(),
        parent_node_id: None,
        timestamp: "2026-08-18T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };
    let encoded = node
        .encode_storage_body(crate::store::FleetFormat::current())
        .expect("encode storage body");
    let mut stamped: serde_json::Value =
        serde_json::from_str(&encoded).expect("stored body is JSON");
    stamped["schema_version"] = serde_json::json!(unsupported_generation);

    let error =
        SessionNodeRecord::decode_storage_body("node-1".to_string(), None, &stamped.to_string())
            .expect_err("a node-body generation below the reader window must be refused");

    assert_eq!(
        error.to_string(),
        format!(
            "graph node body is schema version {}, but this build reads generation {} and the \
             fleet's recorded {} (FIG-3796); remedy: the body is pre-cutover data, so recreate \
             the session store under this build",
            unsupported_generation,
            SESSION_NODE_BODY_SCHEMA_VERSION,
            SESSION_NODE_BODY_SCHEMA_VERSION
        ),
    );
}

#[test]
fn nearest_frame_is_derived_from_ancestry() {
    let assignment = crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    let mut graph = SessionGraph::default();
    let first_key =
        crate::FrameKey::from_caller_material("first-frame").expect("non-empty frame material");
    let first = frame_node_id(&SessionId::from("session"), first_key.as_str());
    assert!(
        graph.append_frame_open_with_id_at(
            first.clone(),
            first_key,
            crate::AgentFrameReason::initial(),
            assignment.clone(),
            "2026-07-27T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
        )
    );
    let first_message = graph.append_message(text_message("m1", MessageRole::User, "first"));
    let second_key =
        crate::FrameKey::from_caller_material("second-frame").expect("non-empty frame material");
    let second = frame_node_id(&SessionId::from("session"), second_key.as_str());
    assert!(
        graph.append_frame_open_with_id_at(
            second.clone(),
            second_key,
            crate::AgentFrameReason::continue_as(),
            assignment,
            "2026-07-27T00:00:01.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
        )
    );
    let second_message = graph.append_message(text_message("m2", MessageRole::User, "second"));

    assert_eq!(
        graph
            .nearest_frame_node_id(Some(&first_message))
            .map(lash_vocabulary::identity::NodeId::as_str),
        Some(first.as_str())
    );
    assert_eq!(
        graph
            .nearest_frame_node_id(Some(&second_message))
            .map(lash_vocabulary::identity::NodeId::as_str),
        Some(second.as_str())
    );
    assert_eq!(
        graph
            .nearest_frame_node_id(graph.leaf_node_id.as_deref())
            .map(lash_vocabulary::identity::NodeId::as_str),
        Some(second.as_str())
    );
}

#[test]
fn active_read_rewrite_preserves_draft_node_id_sequence() {
    let first = text_message("m1", MessageRole::User, "first");
    let mut graph = SessionGraph::default();
    graph.append_message(first.clone());

    let leaf_node_id = graph.leaf_node_id.clone().expect("initial leaf");
    let draft_namespace = format!("unscoped-replacement:{leaf_node_id}");
    let mut nodes = graph.nodes.clone();
    nodes.extend((0..2).map(|ordinal| {
        std::sync::Arc::new(SessionNodeRecord {
            node_id: draft_node_id(&draft_namespace, ordinal),
            parent_node_id: Some(leaf_node_id.clone()),
            timestamp: "2026-08-20T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: SessionNodePayload::Plugin {
                plugin_type: "pre-existing-draft".to_string(),
                body: SharedJsonValue::new(serde_json::json!({"ordinal": ordinal})),
            },
        })
    }));
    graph = SessionGraph::from_shared_nodes(nodes, Some(leaf_node_id))
        .expect("pre-existing draft branches are structurally valid");

    graph.rewrite_active_read_tail(&[
        first,
        text_message("m2", MessageRole::Assistant, "second"),
        text_message("m3", MessageRole::User, "third"),
    ]);

    let emitted_ids = graph
        .nodes
        .iter()
        .skip(3)
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        emitted_ids,
        vec![
            "draft-node/v3/f7ef57eb86b29cc4d182cf4c0d163df45d482bae218563bdbb1daeeaaa28a128"
                .to_string(),
            "draft-node/v3/9c31574ab8d7b250a1978293986829b6a999eb527b8ba2b4d1d15f598da0c487"
                .to_string(),
        ]
    );
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
            graph.nodes.iter().map(std::sync::Arc::as_ref),
            graph.append_builder_in_namespace("active-read-prefix-differential-test"),
            &messages,
            "2026-08-20T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
        );
        let projection =
            build_active_read_projection(graph.nodes.iter().map(std::sync::Arc::as_ref), &messages);
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

/// The tail rewrite covers the current frame only, like the projection.
#[test]
fn the_tail_rewrite_covers_only_the_current_frame() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    open_test_frame(
        &mut graph,
        &session,
        "frame-a",
        crate::AgentFrameReason::initial(),
    );
    graph.append_message(text_message("a", MessageRole::User, "a"));
    open_test_frame(
        &mut graph,
        &session,
        "frame-b",
        crate::AgentFrameReason::continue_as(),
    );
    let b1 = text_message("b1", MessageRole::User, "b1");
    graph.append_message(b1.clone());

    graph.rewrite_active_read_tail(&[b1, text_message("b2", MessageRole::Assistant, "b2")]);
    let read = graph.read_model();
    assert_eq!(
        read.messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["b1", "b2"]
    );
    assert!(
        graph
            .active_path_nodes()
            .iter()
            .any(|node| node.message_id() == Some("a")),
        "the earlier frame stays on the active path"
    );
}

fn open_test_frame(
    graph: &mut SessionGraph,
    session: &SessionId,
    key: &str,
    reason: crate::AgentFrameReason,
) -> crate::FrameNodeId {
    let assignment = crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    let frame_key = crate::FrameKey::from_caller_material(key).expect("non-empty material");
    let frame = frame_node_id(session, frame_key.as_str());
    assert!(
        graph.append_frame_open_with_id_at(
            frame.clone(),
            frame_key,
            reason,
            assignment,
            "2026-09-29T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
        )
    );
    frame
}

/// ADR 0112 §9: after a frame switch is durable, the resident graph keeps
/// only the new frame, anchored at its `FrameOpen`, and the frame records
/// continue from the old frame.
#[test]
fn retiring_below_the_current_frame_re_anchors_the_graph() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    let frame_a = open_test_frame(
        &mut graph,
        &session,
        "frame-a",
        crate::AgentFrameReason::initial(),
    );
    graph.append_message(text_message("a1", MessageRole::User, "a1"));
    graph.append_message(text_message("a2", MessageRole::Assistant, "a2"));
    let a_leaf = graph.leaf_node_id.clone().expect("leaf");
    let frame_b = open_test_frame(
        &mut graph,
        &session,
        "frame-b",
        crate::AgentFrameReason::continue_as(),
    );
    graph.append_message(text_message("b1", MessageRole::User, "b1"));
    let pending = graph.leaf_node_id.clone().expect("leaf");

    let durable = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .filter(|node_id| *node_id != pending)
        .collect::<Vec<_>>();
    graph.mark_durable(&durable);
    let retired = graph
        .retire_below_current_frame()
        .expect("the durable frame switch retires the old frame");
    assert_eq!(retired.len(), 3);
    assert_eq!(graph.durable_len(), 1, "the new FrameOpen stays durable");
    assert_eq!(graph.pending_nodes().len(), 1, "its child stays pending");
    assert_eq!(
        graph.nodes.len(),
        2,
        "the new FrameOpen and its pending child"
    );
    assert_eq!(graph.leaf_node_id.as_ref(), Some(&pending));
    let anchor = graph.anchor().expect("re-anchored").clone();
    assert_eq!(anchor.frame_node_id, frame_b);
    assert_eq!(anchor.generation, 3);
    assert_eq!(anchor.external_parent.as_ref(), Some(&a_leaf));
    assert_eq!(anchor.previous_frame_node_id.as_ref(), Some(&frame_a));
    graph
        .validate_resident_integrity()
        .expect("anchored graph is valid");
    let frames = graph.agent_frame_records(&session);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].previous_frame_node_id.as_ref(), Some(&frame_a));
    assert_eq!(graph.read_model().messages.len(), 1);

    // The base is current now: a second trim does nothing.
    assert!(graph.retire_below_current_frame().is_none());
}

#[test]
fn remap_node_ids_rewrites_parents_on_child_before_parent_layouts() {
    // A loaded graph carries no parent-before-child guarantee: a child can
    // sit ahead of its parent in the resident vector. Remapping the parent
    // must still rewrite the earlier child's parent id, on both the warm
    // cache index path and the cold fallback.
    for warm_cache in [true, false] {
        let record = |id: &str, parent: Option<&str>| SessionNodeRecord {
            node_id: NodeId::fixture(id.to_string()),
            parent_node_id: parent.map(lash_vocabulary::identity::NodeId::fixture),
            timestamp: "2026-08-08T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: SessionNodePayload::Plugin {
                plugin_type: "remap-child-before-parent".to_string(),
                body: SharedJsonValue::new(serde_json::json!({"id": id})),
            },
        };
        let mut graph = SessionGraph::from_nodes(
            vec![record("child", Some("root")), record("root", None)],
            Some("child".into()),
        )
        .expect("child-before-parent order is structurally valid");
        let snapshot = graph.clone();
        if warm_cache {
            assert!(graph.find_node("root").is_some());
        }

        let derived_root = lash_vocabulary::identity::NodeId::from("derived-root");
        graph.remap_node_ids(
            &lash_vocabulary::identity::SessionId::from("remap-test"),
            &[("root".into(), derived_root.clone())],
        );

        assert_eq!(graph.nodes[1].node_id, derived_root);
        assert_eq!(
            graph.nodes[0].parent_node_id.as_ref(),
            Some(&derived_root),
            "warm_cache={warm_cache}: the earlier child's parent must follow the remap"
        );
        assert_eq!(graph.leaf_node_id.as_deref(), Some("child"));
        // Both ancestors still resolve off the rewritten graph.
        assert!(graph.find_node("child").is_some());
        assert!(graph.find_node("derived-root").is_some());
        graph
            .validate_resident_integrity()
            .expect("remapped child-before-parent graph stays valid");

        // The snapshot keeps the original records and parent id untouched.
        assert_eq!(snapshot.nodes[0].parent_node_id.as_deref(), Some("root"));
        assert_eq!(snapshot.nodes[1].node_id.as_str(), "root");
        assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
        assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
    }
}

#[test]
fn event_only_appends_preserve_the_message_vec_and_render_cache() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "hello"));
    let before = graph.read_model();

    graph.append_protocol_event(protocol_event());
    let after = graph.read_model();

    assert!(lash_protocol_types::AppendVec::ptr_eq(
        &before.messages,
        &after.messages
    ));
    assert!(Arc::ptr_eq(
        &before.prompt_render_cache,
        &after.prompt_render_cache
    ));
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.active_events.len(), before.active_events.len() + 1);
    assert!(!lash_protocol_types::AppendVec::ptr_eq(
        &before.active_events,
        &after.active_events
    ));
}

mod window_anchor {
    use super::*;
    use crate::store::WindowAnchorViolation;

    fn frame_open(key: &str, parent: Option<&str>) -> SessionNodeRecord {
        let frame_key =
            crate::FrameKey::from_caller_material(key).expect("non-empty frame material");
        SessionNodeRecord {
            node_id: NodeId::fixture(
                frame_node_id(&SessionId::from("window"), frame_key.as_str()).into_inner(),
            ),
            parent_node_id: parent.map(lash_vocabulary::identity::NodeId::fixture),
            timestamp: "2026-09-29T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: SessionNodePayload::FrameOpen {
                frame_key,
                reason: crate::AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                )),
            },
        }
    }

    fn plugin(id: &str, parent: &str) -> SessionNodeRecord {
        SessionNodeRecord {
            node_id: id.parse().unwrap(),
            parent_node_id: Some(parent.parse().unwrap()),
            timestamp: "2026-09-29T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: SessionNodePayload::Plugin {
                plugin_type: "window-anchor-test".to_string(),
                body: SharedJsonValue::new(serde_json::json!({"id": id})),
            },
        }
    }

    fn anchor_at(base: &SessionNodeRecord, generation: u64) -> WindowAnchor {
        WindowAnchor {
            frame_node_id: crate::FrameNodeId::new(base.node_id.to_string()).expect("non-empty"),
            generation,
            external_parent: base.parent_node_id.clone(),
            previous_frame_node_id: base
                .parent_node_id
                .as_ref()
                .map(|_| crate::FrameNodeId::new("previous-frame").expect("non-empty")),
        }
    }

    /// A frame above generation 0: base `F` with external parent `outside`,
    /// then `a` and `b`.
    fn window() -> (Vec<SessionNodeRecord>, WindowAnchor) {
        let base = frame_open("second-frame", Some("outside"));
        let base_id = base.node_id.to_string();
        let anchor = anchor_at(&base, 40);
        (vec![base, plugin("a", &base_id), plugin("b", "a")], anchor)
    }

    fn violation(result: Result<SessionGraph, crate::StoreError>) -> WindowAnchorViolation {
        match result {
            Err(crate::StoreError::InvalidWindowAnchor { violation, .. }) => violation,
            other => panic!("expected an anchor violation, got {other:?}"),
        }
    }

    #[test]
    fn an_anchored_window_admits_exactly_its_base_parent() {
        let (nodes, anchor) = window();
        let graph = SessionGraph::from_window(nodes, "b".into(), anchor.clone())
            .expect("a well-formed window");
        assert_eq!(graph.anchor(), Some(&anchor));
        assert_eq!(graph.nodes.len(), 3);
        graph
            .validate_resident_integrity()
            .expect("the base's external parent is admitted");

        let encoded = serde_json::to_string(&graph).expect("encode window");
        let decoded: SessionGraph = serde_json::from_str(&encoded).expect("decode window");
        assert_eq!(decoded.anchor(), Some(&anchor));

        let (nodes, _) = window();
        assert!(matches!(
            SessionGraph::from_nodes(nodes, Some("b".into())),
            Err(crate::StoreError::InvalidGraphParent { .. })
        ));
    }

    #[test]
    fn a_base_that_is_not_a_frame_open_is_refused() {
        let (mut nodes, anchor) = window();
        nodes[0] = plugin(anchor.base_node_id(), "outside");
        assert_eq!(
            violation(SessionGraph::from_window(nodes, "b".into(), anchor)),
            WindowAnchorViolation::BaseNotFrameOpen
        );
    }

    #[test]
    fn a_base_other_than_the_leaf_frame_is_refused() {
        let (nodes, mut anchor) = window();
        anchor.frame_node_id = crate::FrameNodeId::new("another-frame").expect("non-empty");
        assert_eq!(
            violation(SessionGraph::from_window(nodes, "b".into(), anchor)),
            WindowAnchorViolation::BaseIsNotLeafFrame
        );
    }

    #[test]
    fn a_second_frame_open_inside_the_window_is_a_foreign_frame_pointer() {
        let (mut nodes, anchor) = window();
        nodes.push(frame_open("third-frame", Some("b")));
        let leaf = nodes[3].node_id.clone();
        assert_eq!(
            violation(SessionGraph::from_window(nodes, leaf, anchor)),
            WindowAnchorViolation::ForeignFramePointer
        );
    }

    #[test]
    fn external_parent_shape_follows_the_base_generation() {
        let (nodes, mut anchor) = window();
        anchor.generation = 0;
        assert_eq!(
            violation(SessionGraph::from_window(nodes, "b".into(), anchor)),
            WindowAnchorViolation::ExternalParentShape,
            "a generation-0 base with a parent"
        );

        let base = frame_open("second-frame", None);
        let base_id = base.node_id.to_string();
        let mut anchor = anchor_at(&base, 40);
        anchor.external_parent = None;
        anchor.previous_frame_node_id = None;
        assert_eq!(
            violation(SessionGraph::from_window(
                vec![base, plugin("a", &base_id)],
                "a".into(),
                anchor,
            )),
            WindowAnchorViolation::ExternalParentShape,
            "a base above generation 0 with no parent"
        );

        let (nodes, mut anchor) = window();
        anchor.external_parent = Some("elsewhere".into());
        assert_eq!(
            violation(SessionGraph::from_window(nodes, "b".into(), anchor)),
            WindowAnchorViolation::ExternalParentShape,
            "a base whose parent is not the anchor's"
        );
    }

    #[test]
    fn a_deleted_middle_row_leaves_an_inner_parent_outside_the_window() {
        let (mut nodes, anchor) = window();
        nodes.remove(1);
        assert_eq!(
            violation(SessionGraph::from_window(nodes, "b".into(), anchor)),
            WindowAnchorViolation::InnerParentOutsideWindow
        );
    }

    /// FIG-6889: a window proven once keeps its proof across appends that
    /// cannot break it, and validating it after any append refuses exactly
    /// what validating the same nodes from scratch refuses: the corruptions
    /// a store's rows or a commit can carry (a duplicate id, a dangling
    /// parent, a parent cycle, a node claiming the anchor's external
    /// parent) and well-formed batches alike.
    #[test]
    fn a_proven_window_validates_appends_as_a_fresh_graph_does() {
        let parentless = |id: &str| SessionNodeRecord {
            parent_node_id: None,
            ..plugin(id, "unused")
        };
        let cases: Vec<(&str, Vec<SessionNodeRecord>)> = vec![
            ("one well-formed node", vec![plugin("c", "b")]),
            (
                "a well-formed batch",
                vec![plugin("c", "b"), plugin("d", "c"), plugin("e", "a")],
            ),
            ("a parentless node", vec![parentless("c")]),
            ("a duplicate of a resident id", vec![plugin("a", "b")]),
            (
                "a duplicate inside the batch",
                vec![plugin("c", "b"), plugin("c", "c")],
            ),
            ("a dangling parent", vec![plugin("c", "missing")]),
            ("its own parent", vec![plugin("c", "c")]),
            (
                "a parent appended after it",
                vec![plugin("c", "d"), plugin("d", "b")],
            ),
            ("the anchor's external parent", vec![plugin("outside", "b")]),
        ];
        for (case, appended) in cases {
            let (nodes, anchor) = window();
            let mut graph =
                SessionGraph::from_window(nodes, "b".into(), anchor).expect("a well-formed window");
            graph
                .validate_resident_integrity()
                .expect("the window is proven before the append");
            graph.append_prebuilt_nodes(appended);
            let incremental = graph.validate_resident_integrity();
            let fresh = SessionGraph {
                inner: Arc::clone(&graph.inner),
                cache: Arc::default(),
                durable_len: 0,
            }
            .validate_resident_integrity();
            assert_eq!(
                format!("{incremental:?}"),
                format!("{fresh:?}"),
                "{case}: the proven window and a fresh graph disagree"
            );
            if fresh.is_ok() {
                assert!(
                    graph.cache().proven(),
                    "{case}: a well-formed append keeps the proof"
                );
            }
        }
    }

    #[test]
    fn the_last_window_row_must_be_the_leaf() {
        let (nodes, anchor) = window();
        assert!(matches!(
            SessionGraph::from_window(nodes, "a".into(), anchor),
            Err(crate::StoreError::InvalidGraphLeaf { .. })
        ));
    }
}

/// FIG-4059/FIG-4060: a turn's commit appends to the resident graph, derives
/// its drafts' ids and realizes their timestamps; readers hold what the
/// commit published (the live replay holds one per commit). Every held
/// reader keeps exactly what it saw, the frame's messages and nodes stay in a
/// bounded set of shared buffers however many readers are held, and the
/// commit's rewrites keep the cache instead of rebuilding it.
#[test]
fn held_readers_of_every_commit_share_the_frame_and_keep_what_they_saw() {
    const TURNS: usize = 200;
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    open_test_frame(
        &mut graph,
        &session,
        "frame-a",
        crate::AgentFrameReason::initial(),
    );
    let mut held = Vec::new();
    for turn in 0..TURNS {
        let draft = graph.append_message(text_message(
            &format!("m{turn}"),
            MessageRole::User,
            &format!("turn {turn}"),
        ));
        let derived = NodeId::fixture(format!("derived-{turn}"));
        let warm = graph.read_model();
        graph.remap_node_ids(&session, &[(draft, derived.clone())]);
        graph.apply_realized_node_timestamps(&[RealizedNodeTimestamp {
            node_id: derived.clone(),
            timestamp: format!("2026-09-29T00:00:{:02}.000000000Z", turn % 60)
                .parse()
                .expect("canonical node timestamp"),
        }]);
        let read = graph.read_model();
        assert!(
            lash_protocol_types::AppendVec::ptr_eq(&warm.messages, &read.messages)
                && Arc::ptr_eq(&warm.prompt_render_cache, &read.prompt_render_cache),
            "deriving ids and realizing timestamps keeps the warm cache"
        );
        assert_eq!(
            graph
                .find_node(derived.as_str())
                .map(|node| node.node_id.clone()),
            Some(derived)
        );
        held.push((read, graph.clone()));
    }

    for (turn, (read, snapshot)) in held.iter().enumerate() {
        assert_eq!(read.messages.len(), turn + 1);
        assert_eq!(read.messages[turn].id, format!("m{turn}"));
        let leaf = snapshot.leaf_node_id.clone().expect("leaf");
        assert_eq!(leaf.as_str(), format!("derived-{turn}"));
        let leaf_node = snapshot.find_node(leaf.as_str()).expect("leaf node");
        assert_eq!(
            leaf_node.timestamp.to_string(),
            format!("2026-09-29T00:00:{:02}.000000000Z", turn % 60)
        );
    }
    let message_buffers = held
        .iter()
        .map(|(read, _)| read.messages.as_ptr())
        .collect::<HashSet<_>>();
    let node_buffers = held
        .iter()
        .map(|(_, snapshot)| snapshot.nodes.as_ptr())
        .collect::<HashSet<_>>();
    assert!(
        message_buffers.len() <= 10,
        "{} message buffers for {TURNS} held readers",
        message_buffers.len()
    );
    assert!(
        node_buffers.len() <= 10,
        "{} node buffers for {TURNS} held readers",
        node_buffers.len()
    );

    // A reader held before a commit rewrites its tail keeps the draft.
    let draft = graph.append_message(text_message("late", MessageRole::User, "late"));
    let before_commit = graph.clone();
    graph.remap_node_ids(&session, &[(draft.clone(), NodeId::from("derived-late"))]);
    assert_eq!(before_commit.leaf_node_id.as_ref(), Some(&draft));
    assert!(before_commit.find_node(draft.as_str()).is_some());
    assert!(graph.find_node(draft.as_str()).is_none());
    assert!(graph.find_node("derived-late").is_some());

    // A frame change starts a new projection; readers of the old frame keep
    // theirs.
    let last_of_a = graph.read_model();
    open_test_frame(
        &mut graph,
        &session,
        "frame-b",
        crate::AgentFrameReason::continue_as(),
    );
    graph.append_message(text_message("b0", MessageRole::User, "b"));
    let first_of_b = graph.read_model();
    assert_eq!(first_of_b.messages.len(), 1);
    assert_eq!(first_of_b.messages[0].id, "b0");
    assert_eq!(last_of_a.messages.len(), TURNS + 1);
    assert_eq!(held[0].0.messages.len(), 1);
    assert_eq!(held[0].0.messages[0].id, "m0");
}

/// FIG-5052: durable node time admits only the fixed-width nanosecond UTC spelling.
#[test]
fn stored_node_timestamp_refuses_noncanonical_text() {
    for timestamp in [
        "yesterday",
        "",
        "2026-10-02T00:00:00.123456789+00:00",
        "2026-10-02T00:00:00Z",
        "2026-10-02T00:00:00.123Z",
        "2026-10-02t00:00:00.123456789z",
        "2026-02-30T00:00:00.123456789Z",
    ] {
        let body = serde_json::json!({
            "schema_version": SESSION_NODE_BODY_SCHEMA_VERSION,
            "timestamp": timestamp,
            "kind": "plugin",
            "plugin_type": "timestamp-law",
            "body": {}
        });
        assert!(
            SessionNodeRecord::decode_storage_body("timestamp-law".into(), None, &body.to_string())
                .is_err(),
            "noncanonical node timestamp was accepted: {timestamp}"
        );
    }
}

/// FIG-5052: every admitted node instant has a 30-byte wire spelling, including
/// zero fractions and sub-millisecond precision, without losing its instant.
#[test]
fn node_timestamp_round_trips_fixed_width_utc_instants() {
    for text in [
        "0000-01-01T00:00:00.000000000Z",
        "1970-01-01T00:00:00.000000001Z",
        "2026-10-02T00:00:00.123456789Z",
        "9999-12-31T23:59:59.999999999Z",
    ] {
        let timestamp: NodeTimestamp = text.parse().expect("canonical node time");
        assert_eq!(timestamp.to_string().len(), NodeTimestamp::WIDTH);
        assert_eq!(
            serde_json::to_value(timestamp).expect("serialize time"),
            text
        );
        let decoded: NodeTimestamp =
            serde_json::from_value(serde_json::json!(text)).expect("decode time");
        assert_eq!(decoded, timestamp);
    }
    assert_eq!(
        "yesterday".parse::<NodeTimestamp>(),
        Err(NodeTimestampError::Noncanonical)
    );
    let expanded_year = crate::testing::TestClock::new(253_402_300_800_000);
    assert_eq!(
        NodeTimestamp::new(crate::Clock::timestamp_datetime(&expanded_year)),
        Err(NodeTimestampError::YearOutOfRange)
    );
    let clock = crate::testing::TestClock::new(1_700_000_000_123);
    assert_eq!(clock.node_timestamp().timestamp_millis(), 1_700_000_000_123);
    assert_eq!(
        clock.node_timestamp().to_string(),
        "2023-11-14T22:13:20.123000000Z"
    );
}

fn project_stored_assistant(id: &str, parts: Vec<Part>) -> crate::transcript::SessionTranscript {
    let node = SessionNodeRecord {
        node_id: NodeId::fixture(id),
        parent_node_id: None,
        timestamp: "2026-10-08T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Conversation(crate::ConversationRecord::from_message(
                Message {
                    id: id.into(),
                    role: MessageRole::Assistant,
                    parts: shared_parts(parts),
                    origin: None,
                    reply_marker: None,
                },
            )),
        },
    };
    let body = node
        .encode_storage_body(crate::store::FleetFormat::current())
        .expect("encode the stored conversation node");
    let stored = SessionNodeRecord::decode_storage_body(id.into(), None, &body)
        .expect("decode the stored conversation node");
    crate::transcript::SessionTranscript::from_records(
        [&stored],
        &crate::transcript::TranscriptDecoders::default(),
    )
    .expect("decode the stored conversation node")
}

/// FIG-5291: opaque replay reasoning has no visible content, including whitespace.
#[test]
fn transcript_suppresses_blank_opaque_reasoning_as_empty_content() {
    for (index, text) in ["", " \n\t", "\u{2003}"].into_iter().enumerate() {
        let projection = project_stored_assistant(
            &format!("blank-reasoning-{index}"),
            vec![Part::reasoning(
                "reasoning".into(),
                text.into(),
                Some(lash_protocol_types::llm::types::ProviderReasoningReplay {
                    encrypted_content: Some("opaque-provider-replay".into()),
                    ..Default::default()
                }),
            )],
        );
        assert_eq!(projection.entries().len(), 1);
        assert_eq!(projection.visible().count(), 0);
        assert_eq!(
            projection.entries()[0].item,
            crate::transcript::TranscriptItem::Suppressed(
                crate::transcript::SuppressionReason::EmptyContent
            )
        );
    }
}

/// FIG-5291: a tool call and its reasoning are one assistant entry, in part order.
#[test]
fn transcript_reasoning_with_tool_call_is_one_assistant_entry() {
    let projection = project_stored_assistant(
        "reasoning-with-tool",
        vec![
            Part::reasoning("reasoning".into(), "Inspect the file first.".into(), None),
            Part::tool_call(
                "call".into(),
                "{\"path\":\"notes.txt\"}".into(),
                lash_protocol_types::ToolCallId::fixture("transcript-call"),
                "provider-call".into(),
                "read_file".into(),
                None,
            ),
        ],
    );
    assert_eq!(projection.entries().len(), 1);
    let entries = projection.visible().collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].item,
        crate::transcript::TranscriptItem::Message(crate::transcript::TranscriptMessage {
            role: crate::transcript::TranscriptRole::Assistant,
            blocks: vec![
                crate::transcript::TranscriptBlock::Reasoning {
                    text: "Inspect the file first.".into()
                },
                crate::transcript::TranscriptBlock::ToolCall {
                    call_id: lash_protocol_types::ToolCallId::fixture("transcript-call"),
                    tool_name: "read_file".into(),
                    arguments: "{\"path\":\"notes.txt\"}".into(),
                },
            ],
        })
    );
}

/// FIG-5291: exposed reasoning retains its text and drops blank sibling parts.
#[test]
fn transcript_nonempty_reasoning_remains_a_reasoning_entry() {
    let projection = project_stored_assistant(
        "nonempty-reasoning",
        vec![
            Part::reasoning("blank".into(), " \n\t".into(), None),
            Part::reasoning("reasoning".into(), "  Consider the options.\n".into(), None),
        ],
    );
    assert_eq!(projection.entries().len(), 1);
    let entries = projection.visible().collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].item,
        crate::transcript::TranscriptItem::Message(crate::transcript::TranscriptMessage {
            role: crate::transcript::TranscriptRole::Assistant,
            blocks: vec![crate::transcript::TranscriptBlock::Reasoning {
                text: "  Consider the options.\n".into()
            }],
        })
    );
}

/// FIG-6889: the entries a commit adds to the transcript, decoded from the
/// nodes after the base's leaf with the turn the base's path last named,
/// are exactly what decoding the head's whole active path and dropping the
/// base's entries yields: across protocol events that inherit a turn from
/// a message before the base, reply markers, plugin nodes, and a head that
/// branched away from the base's leaf.
#[test]
fn a_commits_added_transcript_equals_the_full_decode_less_the_base() {
    use lash_protocol_types::core_support::TurnReplyCoreSupport;
    let decoders = crate::transcript::TranscriptDecoders::default();
    let turn = |name: &str| lash_vocabulary::identity::TurnId::parse(name).expect("turn id");
    let message = |id: &str, role, origin| Message {
        origin,
        ..text_message(id, role, id)
    };
    let input = |id: &str, run: &str| {
        message(
            id,
            MessageRole::User,
            Some(crate::MessageOrigin::TurnInput {
                turn_id: turn(run),
                input_id: None,
            }),
        )
    };
    let output = |id: &str, run: &str| {
        message(
            id,
            MessageRole::Assistant,
            Some(crate::MessageOrigin::TurnOutput {
                turn_id: turn(run),
                source: crate::TurnOutputSource::Runtime,
                cell_id: None,
            }),
        )
    };
    let reply = |id: &str, run: &str| {
        let mut reply = message(id, MessageRole::Assistant, None);
        reply.reply_marker = Some(crate::TurnReply::mint(
            turn(run),
            reply.parts[0].id().to_owned(),
        ));
        reply
    };
    let event = || SessionNodeDraft::event(SessionHistoryRecord::Protocol(protocol_event()));
    let decode = |graph: &SessionGraph| {
        crate::transcript::SessionTranscript::from_records(graph.active_path_nodes(), &decoders)
            .expect("decode the active path")
            .into_entries()
    };
    let full_less_base = |base: &SessionGraph, head: &SessionGraph| {
        let held = decode(base)
            .into_iter()
            .map(|entry| entry.entry_id)
            .collect::<HashSet<_>>();
        decode(head)
            .into_iter()
            .filter(|entry| !held.contains(&entry.entry_id))
            .collect::<Vec<_>>()
    };
    let check = |case: &str, base: &SessionGraph, head: &SessionGraph| {
        let added = crate::transcript::SessionTranscript::added_since(base, head, &decoders)
            .expect("decode the commit")
            .into_entries();
        assert_eq!(added, full_less_base(base, head), "{case}");
    };

    let empty = SessionGraph::default();
    let mut graph = SessionGraph::default();
    graph.append_message(input("first-input", "turn-1"));
    check("a first commit", &empty, &graph);

    let base = graph.clone();
    graph.append_node_draft(event());
    graph.append_node_draft(event());
    check(
        "events inherit the turn of the base's last input",
        &base,
        &graph,
    );

    let base = graph.clone();
    graph.append_node_draft(event());
    graph.append_plugin("history-test", serde_json::json!({"n": 1}));
    graph.append_message(reply("first-reply", "turn-1"));
    graph.append_message(input("second-input", "turn-2"));
    check("a reply then a new turn's input", &base, &graph);

    let base = graph.clone();
    graph.append_message(output("second-output", "turn-2"));
    graph.append_node_draft(event());
    check("events after an output", &base, &graph);

    let base = graph.clone();
    graph.append_plugin("history-test", serde_json::json!({"n": 2}));
    graph.append_node_draft(event());
    check("only plugin nodes since the turn's message", &base, &graph);

    let branch_point = graph
        .active_path_nodes()
        .into_iter()
        .find(|node| node.message_id() == Some("first-reply"))
        .map(|node| node.node_id.clone())
        .expect("the first reply is on the path");
    let base = graph.clone();
    let mut graph = SessionGraph::from_nodes(
        graph
            .nodes
            .iter()
            .map(|node| node.as_ref().clone())
            .collect(),
        Some(branch_point),
    )
    .expect("the same nodes under an earlier leaf");
    graph.append_node_draft(event());
    graph.append_message(input("branch-input", "turn-3"));
    check("a head branched away from the base's leaf", &base, &graph);
}

#[test]
fn retained_graph_render_keeps_prefix_across_multiple_source_appends() {
    use lash_protocol_types::session_model::message::HistoryPresentation;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let policy = HistoryPresentation::new(move |_, part| {
        count.fetch_add(1, Ordering::SeqCst);
        part.content().into_owned()
    });
    let mut graph = SessionGraph::default();
    let mut held = Vec::<lash_protocol_types::llm::types::RequestMessages>::new();
    for turn in 0..64 {
        graph.append_message(text_message(
            &format!("u{turn}"),
            MessageRole::User,
            &format!("question {turn}"),
        ));
        graph.append_message(text_message(
            &format!("a{turn}"),
            MessageRole::Assistant,
            &format!("answer {turn}"),
        ));
        let read = graph.read_model();
        calls.store(0, Ordering::SeqCst);
        let rendered = read
            .prompt_render_cache
            .rendered_retained(read.messages.as_slice(), &policy);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "only the two new source parts render"
        );
        assert_eq!(
            rendered.iter().cloned().collect::<Vec<_>>(),
            lash_protocol_types::session_model::render_prompt(read.messages.as_slice()).messages
        );
        if let Some(previous) = held.last() {
            assert_eq!(
                previous.shared_prefix(&rendered),
                previous.len(),
                "every retained reader shares the complete prefix"
            );
        }
        held.push(rendered);
    }
}

/// FIG-7222: the plugin index preserves active ancestry, record order and
/// snapshot isolation through appends, branch selection, restore and trim.
#[test]
fn plugin_recovery_facts_follow_active_ancestry_and_snapshot_lifetime() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("plugin-index");
    open_test_frame(
        &mut graph,
        &session,
        "initial",
        crate::AgentFrameReason::initial(),
    );
    let pending = graph.append_plugin("recovery", serde_json::json!({"kind": "pending"}));
    let held = graph.clone();
    assert_eq!(held.active_plugin_nodes("recovery")[0].node_id, pending);
    let failed = graph.append_plugin(
        "recovery",
        serde_json::json!({"kind": "failed", "attempt": 1}),
    );
    graph.append_plugin("other", serde_json::json!({"unrelated": true}));
    let ids = |graph: &SessionGraph| {
        graph
            .active_plugin_nodes("recovery")
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&held), vec![pending.clone()]);
    assert_eq!(ids(&graph), vec![pending.clone(), failed]);
    let mut branch = held.append_builder_in_namespace("alternate-recovery");
    let nodes = branch.append_drafts_at(
        [SessionNodeDraft::plugin(
            "recovery",
            serde_json::json!({"kind": "completed"}),
        )],
        "2026-10-23T00:00:00.000000000Z".parse().expect("timestamp"),
    );
    let completed = nodes[0].node_id.clone();
    graph
        .apply_append(&crate::store::GraphAppend::Extend { nodes })
        .expect("select alternate branch");
    assert_eq!(ids(&graph), vec![pending.clone(), completed.clone()]);
    let mut restored: SessionGraph =
        serde_json::from_value(serde_json::to_value(&graph).unwrap()).unwrap();
    assert_eq!(ids(&restored), vec![pending, completed]);
    assert!(restored.active_plugin_nodes("absent").is_empty());
    open_test_frame(
        &mut restored,
        &session,
        "compacted",
        crate::AgentFrameReason::continue_as(),
    );
    let next = restored.append_plugin("recovery", serde_json::json!({"kind": "pending"}));
    assert_eq!(
        ids(&restored).len(),
        3,
        "pending frames keep the preceding path"
    );
    restored.mark_all_durable();
    restored
        .retire_below_current_frame()
        .expect("retire settled frame");
    assert_eq!(ids(&restored), vec![next]);
    assert_eq!(
        ids(&graph).len(),
        2,
        "trim leaves the prior snapshot intact"
    );
}
