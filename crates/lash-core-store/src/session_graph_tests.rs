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
    }
}

#[test]
fn construction_enforces_structural_graph_integrity() {
    let node = |id: &str, parent: Option<&str>| SessionNodeRecord {
        node_id: id.to_string().into(),
        parent_node_id: parent.map(crate::NodeId::from),
        timestamp: "2026-08-08T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "construction-integrity-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"id": id})),
        },
    };

    assert!(matches!(
        SessionGraph::from_nodes(vec![node("", None)], Some(String::new().into())),
        Err(crate::StoreError::InvalidGraphNodeId { node_id }) if node_id.is_empty()
    ));
    let invalid_encoded = serde_json::to_string(&SessionGraph::from_unchecked_nodes_for_testing(
        vec![node("", None)],
        Some(String::new().into()),
    ))
    .unwrap();
    assert!(
        serde_json::from_str::<SessionGraph>(&invalid_encoded)
            .expect_err("serialized graphs must reject empty node identities")
            .to_string()
            .contains("node id must not be empty")
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
        node_id: node_id.to_string().into(),
        parent_node_id: Some(parent_node_id.to_string().into()),
        timestamp: "2026-09-12T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "atomic-append-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"node": node_id})),
        },
    };

    let empty_node_id = GraphAppend::Extend {
        nodes: vec![node("", &resident_leaf)],
    };
    assert!(matches!(
        graph.apply_append(&empty_node_id),
        Err(crate::StoreError::InvalidGraphNodeId { node_id }) if node_id.is_empty()
    ));

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
    assert!(lash_sansio::AppendVec::ptr_eq(
        &before_read.active_events,
        &after_read.active_events
    ));
    assert!(lash_sansio::AppendVec::ptr_eq(
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
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cycle-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "cycle-b".into(),
                parent_node_id: Some("cycle-a".into()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
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
fn cache_build_rejects_duplicate_node_ids() {
    let node = SessionNodeRecord {
        node_id: "duplicate".into(),
        parent_node_id: None,
        timestamp: "2026-07-31T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "duplicate-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"value": 1})),
        },
    };
    let graph = SessionGraph::from_unchecked_nodes_for_testing(
        vec![node.clone(), node],
        Some("duplicate".into()),
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
            node_id: "dangling-child".into(),
            parent_node_id: Some("missing-parent".into()),
            timestamp: "2026-07-31T00:00:00Z".to_string(),
            payload: SessionNodePayload::Plugin {
                plugin_type: "dangling-test".to_string(),
                body: SharedJsonValue::new(serde_json::json!({"value": 1})),
            },
        }],
        Some("dangling-child".into()),
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
        node_id: "existing-node".into(),
        parent_node_id: None,
        timestamp: "2026-07-31T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "leaf-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"value": 1})),
        },
    };
    let unknown_leaf = SessionGraph::from_unchecked_nodes_for_testing(
        vec![node.clone()],
        Some("missing-leaf".into()),
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
        node_id: node_id.to_string().into(),
        parent_node_id: parent_node_id.map(crate::NodeId::from),
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
                timestamp: "2026-07-31T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "nearest-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "a"})),
                },
            },
            SessionNodeRecord {
                node_id: "nearest-b".into(),
                parent_node_id: Some("nearest-a".into()),
                timestamp: "2026-07-31T00:00:00Z".to_string(),
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
fn draft_node_ids_are_opaque_distinct_and_ignore_message_ids() {
    let mut graph = SessionGraph::default();

    let message_id = graph.append_message(text_message("m1", MessageRole::User, "hello"));
    let protocol_id = graph.append_protocol_event(protocol_event());
    let plugin_id = graph.append_plugin("example", serde_json::json!({"ok": true}));

    assert_ne!(message_id, "m1");
    assert!(message_id.starts_with("draft-node/v3/"));
    assert!(protocol_id.starts_with("draft-node/v3/"));
    assert!(plugin_id.starts_with("draft-node/v3/"));
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
        node_id: "node-2".into(),
        parent_node_id: Some("node-1".into()),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
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
        node.parent_node_id.clone().map(crate::NodeId::into_inner),
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
        node_id: "node-1".into(),
        parent_node_id: None,
        timestamp: "2026-08-18T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };

    let encoded = node
        .encode_storage_body(crate::store::FleetFormat::current())
        .expect("encode storage body");
    let stamped: serde_json::Value = serde_json::from_str(&encoded).expect("stored body is JSON");

    assert_eq!(
        stamped
            .get("schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(u64::from(SESSION_NODE_BODY_SCHEMA_VERSION)),
    );
}

#[test]
fn unstamped_stored_bodies_are_refused() {
    // Byte-for-byte a body written before the generation stamp existed.
    let legacy = r#"{"timestamp":"2026-07-27T00:00:00Z","kind":"plugin","plugin_type":"legacy","body":{"value":7}}"#;

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
fn unstamped_conversation_bodies_are_refused() {
    let legacy = r#"{"timestamp":"2023-11-14T22:13:20+00:00","kind":"event","event":{"Conversation":{"id":"m_legacy","role":"User","parts":[{"id":"m_legacy.p0","kind":"Text","content":"durable read user message"}]}}}"#;

    let error = SessionNodeRecord::decode_storage_body("node-1".to_string(), None, legacy)
        .expect_err("a pre-stamp conversation body must be refused");

    assert!(
        error
            .to_string()
            .contains("carries no schema_version stamp"),
        "the refusal must name the missing stamp: {error}"
    );
}

#[test]
fn stored_bodies_from_an_older_generation_are_refused() {
    let node = SessionNodeRecord {
        node_id: "node-1".into(),
        parent_node_id: None,
        timestamp: "2026-08-18T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Protocol(protocol_event()),
        },
    };
    let encoded = node
        .encode_storage_body(crate::store::FleetFormat::current())
        .expect("encode storage body");
    let mut stamped: serde_json::Value =
        serde_json::from_str(&encoded).expect("stored body is JSON");
    stamped["schema_version"] = serde_json::json!(SESSION_NODE_BODY_SCHEMA_VERSION - 1);

    let error =
        SessionNodeRecord::decode_storage_body("node-1".to_string(), None, &stamped.to_string())
            .expect_err("an older node-body generation must be refused");

    assert_eq!(
        error.to_string(),
        format!(
            "graph node body is schema version {}, but this build reads generation {} and the \
             fleet's recorded {} (FIG-3796); remedy: the body is pre-cutover data, so recreate \
             the session store under this build",
            SESSION_NODE_BODY_SCHEMA_VERSION - 1,
            SESSION_NODE_BODY_SCHEMA_VERSION,
            SESSION_NODE_BODY_SCHEMA_VERSION
        ),
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
            "graph node body is schema version {}, but this build reads generation {} and the \
             fleet's recorded {} (FIG-3796); remedy: run a Lash build at that node-body \
             generation",
            SESSION_NODE_BODY_SCHEMA_VERSION + 1,
            SESSION_NODE_BODY_SCHEMA_VERSION,
            SESSION_NODE_BODY_SCHEMA_VERSION
        ),
    );
}

#[test]
fn stored_frame_open_rejects_a_raw_frame_key() {
    let frame_key = crate::FrameKey::from_caller_material("strict-durable-frame")
        .expect("non-empty frame material");
    let node = SessionNodeRecord {
        node_id: frame_node_id(&SessionId::from("session"), frame_key.as_str())
            .into_inner()
            .into(),
        parent_node_id: None,
        timestamp: "2026-09-01T00:00:00Z".to_string(),
        payload: SessionNodePayload::FrameOpen {
            frame_key,
            reason: crate::AgentFrameReason::initial(),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
        },
    };
    let mut stored: serde_json::Value = serde_json::from_str(
        &node
            .encode_storage_body(crate::store::FleetFormat::current())
            .expect("encode current frame-open body"),
    )
    .expect("frame-open body is JSON");
    stored["frame_key"] = serde_json::json!("initial-frame");

    let error = SessionNodeRecord::decode_storage_body(
        node.node_id.to_string(),
        None,
        &serde_json::to_string(&stored).expect("encode malformed frame-open body"),
    )
    .expect_err("raw durable frame keys must fail the FrameKey gate");

    assert!(
        error
            .to_string()
            .contains("frame key must be derived by Lash"),
        "strict durable decode must surface the FrameKey refusal: {error}"
    );
}

#[test]
fn nearest_frame_is_derived_from_ancestry() {
    let assignment = crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
    ));
    let mut graph = SessionGraph::default();
    let first_key =
        crate::FrameKey::from_caller_material("first-frame").expect("non-empty frame material");
    let first = frame_node_id(&SessionId::from("session"), first_key.as_str());
    assert!(graph.append_frame_open_with_id_at(
        first.clone(),
        first_key,
        crate::AgentFrameReason::initial(),
        assignment.clone(),
        crate::ProtocolTurnOptions::default(),
        "2026-07-27T00:00:00Z".to_string(),
    ));
    let first_message = graph.append_message(text_message("m1", MessageRole::User, "first"));
    let second_key =
        crate::FrameKey::from_caller_material("second-frame").expect("non-empty frame material");
    let second = frame_node_id(&SessionId::from("session"), second_key.as_str());
    assert!(graph.append_frame_open_with_id_at(
        second.clone(),
        second_key,
        crate::AgentFrameReason::continue_as(),
        assignment,
        crate::ProtocolTurnOptions::default(),
        "2026-07-27T00:00:01Z".to_string(),
    ));
    let second_message = graph.append_message(text_message("m2", MessageRole::User, "second"));

    assert_eq!(
        graph
            .nearest_frame_node_id(Some(&first_message))
            .map(crate::NodeId::as_str),
        Some(first.as_str())
    );
    assert_eq!(
        graph
            .nearest_frame_node_id(Some(&second_message))
            .map(crate::NodeId::as_str),
        Some(second.as_str())
    );
    assert_eq!(
        graph
            .nearest_frame_node_id(graph.leaf_node_id.as_deref())
            .map(crate::NodeId::as_str),
        Some(second.as_str())
    );
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
            timestamp: "2026-08-20T00:00:00Z".to_string(),
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
            "2026-08-20T00:00:00Z".to_string(),
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

#[test]
fn graph_writers_keep_payload_kind_out_of_draft_identity() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "hello"));
    graph.append_protocol_event(protocol_event());
    graph.append_plugin("example", serde_json::json!({"ok": true}));

    for node in &graph.nodes {
        assert!(node.node_id.starts_with("draft-node/v3/"), "{:?}", node);
    }
}

/// The turn projection decides prefix agreement by comparing the `Arc` a read
/// model handed out, so two reads of the same frame on an unchanged graph must
/// hand out the *same* `Arc`, not two equal ones. Rebuilding the frame
/// projection per call is what forced the projection onto its whole-window
/// fallback on every turn boundary (FIG-1637).
#[test]
fn the_frame_read_model_is_shared_by_identity_until_an_append() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    open_test_frame(
        &mut graph,
        &session,
        "frame",
        crate::AgentFrameReason::initial(),
    );
    graph.append_message(text_message("m1", MessageRole::User, "first"));

    let first = graph.read_model();
    let second = graph.read_model();
    assert!(
        lash_sansio::AppendVec::ptr_eq(&first.messages, &second.messages),
        "repeated reads share the projected messages by identity"
    );
    assert!(lash_sansio::AppendVec::ptr_eq(
        &first.active_events,
        &second.active_events
    ));
    assert!(Arc::ptr_eq(
        &first.prompt_render_cache,
        &second.prompt_render_cache
    ));

    graph.append_message(text_message("m2", MessageRole::User, "second"));
    let after_append = graph.read_model();
    assert!(
        !lash_sansio::AppendVec::ptr_eq(&first.messages, &after_append.messages),
        "an append to the active path folds into a new projection"
    );
    assert_eq!(after_append.messages.len(), 2);
    let again = graph.read_model();
    assert!(
        lash_sansio::AppendVec::ptr_eq(&after_append.messages, &again.messages),
        "an append folds once"
    );
}

/// ADR 0112 §9: the one projection starts at the current frame, and a
/// pending `FrameOpen` moves its start before the frame is committed.
#[test]
fn a_pending_frame_open_moves_the_projection_to_the_new_frame() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    open_test_frame(
        &mut graph,
        &session,
        "frame-a",
        crate::AgentFrameReason::initial(),
    );
    graph.append_message(text_message("frame-a", MessageRole::User, "a"));
    assert_eq!(graph.read_model().messages.len(), 1);

    open_test_frame(
        &mut graph,
        &session,
        "frame-b",
        crate::AgentFrameReason::continue_as(),
    );
    assert!(
        graph.read_model().messages.is_empty(),
        "the new frame starts an empty projection"
    );
    graph.append_message(text_message("frame-b", MessageRole::User, "b"));
    let b = graph.read_model();
    assert_eq!(b.messages.len(), 1);
    assert_eq!(b.messages[0].id, "frame-b");

    // A cold cache projects the same span.
    let cold = SessionGraph::from_shared_nodes(graph.nodes.clone(), graph.leaf_node_id.clone())
        .expect("valid graph");
    let cold_read = cold.read_model();
    assert_eq!(cold_read.messages.len(), 1);
    assert_eq!(cold_read.messages[0].id, "frame-b");
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
    let assignment = crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
    ));
    let frame_key = crate::FrameKey::from_caller_material(key).expect("non-empty material");
    let frame = frame_node_id(session, frame_key.as_str());
    assert!(graph.append_frame_open_with_id_at(
        frame.clone(),
        frame_key,
        reason,
        assignment,
        crate::ProtocolTurnOptions::default(),
        "2026-09-29T00:00:00Z".to_string(),
    ));
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

    let mut durable = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .filter(|node_id| *node_id != pending)
        .collect::<HashSet<_>>();
    let retired = graph
        .retire_below_current_frame(&durable)
        .expect("the durable frame switch retires the old frame");
    assert_eq!(retired.len(), 3);
    for node_id in &retired {
        durable.remove(node_id);
    }
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
    assert!(graph.retire_below_current_frame(&durable).is_none());
}

/// A pending `FrameOpen` keeps everything below it until it is durable.
#[test]
fn a_pending_frame_switch_retires_nothing() {
    let mut graph = SessionGraph::default();
    let session = SessionId::from("session");
    open_test_frame(
        &mut graph,
        &session,
        "frame-a",
        crate::AgentFrameReason::initial(),
    );
    graph.append_message(text_message("a1", MessageRole::User, "a1"));
    let durable = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<HashSet<_>>();
    open_test_frame(
        &mut graph,
        &session,
        "frame-b",
        crate::AgentFrameReason::continue_as(),
    );
    assert!(graph.retire_below_current_frame(&durable).is_none());
    assert_eq!(graph.nodes.len(), 3);
    assert!(graph.anchor().is_none());
}

#[test]
fn graph_cow_after_snapshot_copies_pointers_not_records() {
    let mut graph = SessionGraph::default();
    let first = graph.append_message(text_message("m1", MessageRole::User, "one"));
    let second = graph.append_message(text_message("m2", MessageRole::Assistant, "two"));
    let snapshot = graph.clone();

    graph.append_message(text_message("m3", MessageRole::User, "three"));

    assert_eq!(snapshot.nodes.len(), 2);
    assert_eq!(snapshot.leaf_node_id.as_ref(), Some(&second));
    assert_eq!(graph.nodes.len(), 3);
    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
    assert_eq!(graph.nodes[0].node_id, first);
    assert_eq!(graph.nodes[1].node_id, second);
}

#[test]
fn apply_append_after_snapshot_shares_resident_records() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "one"));
    let resident_leaf = graph.leaf_node_id.clone().expect("resident leaf");
    let snapshot = graph.clone();

    graph
        .apply_append(&GraphAppend::Extend {
            nodes: vec![SessionNodeRecord {
                node_id: "appended".to_string().into(),
                parent_node_id: Some(resident_leaf.clone()),
                timestamp: "2026-09-12T00:00:00Z".to_string(),
                payload: SessionNodePayload::Plugin {
                    plugin_type: "cow-test".to_string(),
                    body: SharedJsonValue::new(serde_json::json!({"node": "appended"})),
                },
            }],
        })
        .expect("append after snapshot is valid");

    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.leaf_node_id.as_ref(), Some(&resident_leaf));
    assert_eq!(graph.nodes.len(), 2);
    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
}

#[test]
fn remap_node_ids_rewrites_only_mapped_records() {
    let mut graph = SessionGraph::default();
    let first = graph.append_message(text_message("m1", MessageRole::User, "one"));
    let second = graph.append_message(text_message("m2", MessageRole::Assistant, "two"));
    let snapshot = graph.clone();
    // Warm the by_id cache so the remap resolves positions through it.
    assert!(graph.find_node(first.as_str()).is_some());

    let first_derived = crate::NodeId::from("derived-first".to_string());
    let second_derived = crate::NodeId::from("derived-second".to_string());
    graph.remap_node_ids(
        &crate::SessionId::from("remap-test"),
        &[
            (first.clone(), first_derived.clone()),
            (second.clone(), second_derived.clone()),
        ],
    );

    assert_eq!(graph.nodes[0].node_id, first_derived);
    assert_eq!(graph.nodes[1].node_id, second_derived);
    // The mapped parent id is rewritten through the same table.
    assert_eq!(graph.nodes[1].parent_node_id.as_ref(), Some(&first_derived));
    assert_eq!(graph.leaf_node_id.as_ref(), Some(&second_derived));

    // The snapshot keeps the original records untouched.
    assert_eq!(snapshot.nodes[0].node_id, first);
    assert_eq!(snapshot.nodes[1].node_id, second);
    assert_eq!(snapshot.leaf_node_id.as_ref(), Some(&second));
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
}

#[test]
fn remap_node_ids_keeps_unmapped_records_shared() {
    let mut graph = SessionGraph::default();
    let first = graph.append_message(text_message("m1", MessageRole::User, "one"));
    let second = graph.append_message(text_message("m2", MessageRole::Assistant, "two"));
    let snapshot = graph.clone();
    assert!(graph.find_node(first.as_str()).is_some());

    let second_derived = crate::NodeId::from("derived-second".to_string());
    graph.remap_node_ids(
        &crate::SessionId::from("remap-test"),
        &[(second.clone(), second_derived.clone())],
    );

    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
    // An unmapped parent id is left alone.
    assert_eq!(graph.nodes[1].parent_node_id.as_ref(), Some(&first));
    assert_eq!(graph.leaf_node_id.as_ref(), Some(&second_derived));
}

#[test]
fn remap_node_ids_rewrites_mapped_parents_on_unmapped_records() {
    let mut graph = SessionGraph::default();
    let first = graph.append_message(text_message("m1", MessageRole::User, "one"));
    let second = graph.append_message(text_message("m2", MessageRole::Assistant, "two"));
    graph.append_message(text_message("m3", MessageRole::User, "three"));
    let snapshot = graph.clone();
    assert!(graph.find_node(first.as_str()).is_some());

    // Remap only the middle node: the unmapped child must follow its remapped
    // parent, so its record is unshared even though its own id is untouched.
    let second_derived = crate::NodeId::from("derived-second".to_string());
    graph.remap_node_ids(
        &crate::SessionId::from("remap-test"),
        &[(second.clone(), second_derived.clone())],
    );

    assert_eq!(graph.nodes[0].node_id, first);
    assert_eq!(graph.nodes[1].node_id, second_derived);
    assert_eq!(
        graph.nodes[2].parent_node_id.as_ref(),
        Some(&second_derived)
    );
    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[2], &graph.nodes[2]));
    // The snapshot keeps the original parent id.
    assert_eq!(snapshot.nodes[2].parent_node_id.as_ref(), Some(&second));
}

#[test]
fn remap_node_ids_rewrites_parents_on_child_before_parent_layouts() {
    // A loaded graph carries no parent-before-child guarantee: a child can
    // sit ahead of its parent in the resident vector. Remapping the parent
    // must still rewrite the earlier child's parent id, on both the warm
    // cache index path and the cold fallback.
    for warm_cache in [true, false] {
        let record = |id: &str, parent: Option<&str>| SessionNodeRecord {
            node_id: id.to_string().into(),
            parent_node_id: parent.map(crate::NodeId::from),
            timestamp: "2026-08-08T00:00:00Z".to_string(),
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

        let derived_root = crate::NodeId::from("derived-root".to_string());
        graph.remap_node_ids(
            &crate::SessionId::from("remap-test"),
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
fn apply_realized_node_timestamps_rewrites_only_realized_records() {
    let mut graph = SessionGraph::default();
    let first = graph.append_message(text_message("m1", MessageRole::User, "one"));
    graph.append_message(text_message("m2", MessageRole::Assistant, "two"));
    let snapshot = graph.clone();
    assert!(graph.find_node(first.as_str()).is_some());

    graph.apply_realized_node_timestamps(&[crate::session_graph::RealizedNodeTimestamp {
        node_id: first.clone(),
        timestamp: "2026-09-12T00:00:00Z".to_string(),
    }]);

    assert_eq!(graph.nodes[0].timestamp, "2026-09-12T00:00:00Z");
    assert_ne!(graph.nodes[0].timestamp, snapshot.nodes[0].timestamp);
    assert!(!std::sync::Arc::ptr_eq(&snapshot.nodes[0], &graph.nodes[0]));
    assert!(std::sync::Arc::ptr_eq(&snapshot.nodes[1], &graph.nodes[1]));
}

#[test]
fn shared_records_serialize_with_the_unchanged_durable_shape() {
    let node = |node_id: &str, parent_node_id: Option<&str>| SessionNodeRecord {
        node_id: node_id.to_string().into(),
        parent_node_id: parent_node_id.map(Into::into),
        timestamp: "2026-09-12T00:00:00Z".to_string(),
        payload: SessionNodePayload::Plugin {
            plugin_type: "shape-test".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"node": node_id})),
        },
    };
    let graph = SessionGraph::from_nodes(
        vec![node("root", None), node("child", Some("root"))],
        Some("child".to_string().into()),
    )
    .expect("fixture graph is valid");

    let encoded = serde_json::to_value(&graph).expect("serialize graph");
    let nodes = encoded["nodes"].as_array().expect("nodes is an array");
    assert_eq!(nodes.len(), graph.nodes.len());
    // `Arc<SessionNodeRecord>` serializes as the record itself: no wrapper and
    // no key change versus the previous `Vec<SessionNodeRecord>` encoding.
    for (index, node) in graph.nodes.iter().enumerate() {
        assert_eq!(
            &nodes[index],
            &serde_json::to_value(node.as_ref()).expect("serialize record")
        );
    }
    assert_eq!(encoded["leaf_node_id"], serde_json::json!("child"));

    let decoded: SessionGraph =
        serde_json::from_str(&serde_json::to_string(&graph).unwrap()).expect("decode graph");
    assert_eq!(
        serde_json::to_string(&decoded).unwrap(),
        serde_json::to_string(&graph).unwrap()
    );
}

#[test]
fn event_only_appends_preserve_the_message_vec_and_render_cache() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "hello"));
    let before = graph.read_model();

    graph.append_protocol_event(protocol_event());
    let after = graph.read_model();

    assert!(lash_sansio::AppendVec::ptr_eq(
        &before.messages,
        &after.messages
    ));
    assert!(Arc::ptr_eq(
        &before.prompt_render_cache,
        &after.prompt_render_cache
    ));
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.active_events.len(), before.active_events.len() + 1);
    assert!(!lash_sansio::AppendVec::ptr_eq(
        &before.active_events,
        &after.active_events
    ));
}

#[test]
fn held_readers_isolate_folded_pending_tails() {
    let mut graph = SessionGraph::default();
    graph.append_message(text_message("m1", MessageRole::User, "hello"));
    graph.append_protocol_event(protocol_event());
    let held = graph.read_model();

    graph.append_protocol_event(protocol_event());
    let latest = graph.read_model();

    assert_eq!(latest.active_events.len(), held.active_events.len() + 1);
    assert!(!lash_sansio::AppendVec::ptr_eq(
        &held.active_events,
        &latest.active_events
    ));

    graph.append_message(text_message("m2", MessageRole::Assistant, "reply"));
    let with_message = graph.read_model();
    assert_eq!(with_message.messages.len(), 2);
    assert!(!Arc::ptr_eq(
        &with_message.prompt_render_cache,
        &latest.prompt_render_cache
    ));
}

mod window_anchor {
    use super::*;
    use crate::store::WindowAnchorViolation;

    fn frame_open(key: &str, parent: Option<&str>) -> SessionNodeRecord {
        let frame_key =
            crate::FrameKey::from_caller_material(key).expect("non-empty frame material");
        SessionNodeRecord {
            node_id: frame_node_id(&SessionId::from("window"), frame_key.as_str())
                .into_inner()
                .into(),
            parent_node_id: parent.map(crate::NodeId::from),
            timestamp: "2026-09-29T00:00:00Z".to_string(),
            payload: SessionNodePayload::FrameOpen {
                frame_key,
                reason: crate::AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
                protocol_turn_options: crate::ProtocolTurnOptions::default(),
            },
        }
    }

    fn plugin(id: &str, parent: &str) -> SessionNodeRecord {
        SessionNodeRecord {
            node_id: id.into(),
            parent_node_id: Some(parent.into()),
            timestamp: "2026-09-29T00:00:00Z".to_string(),
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
    fn a_root_frame_window_has_no_external_parent() {
        let base = frame_open("first-frame", None);
        let base_id = base.node_id.to_string();
        let anchor = anchor_at(&base, 0);
        let graph =
            SessionGraph::from_window(vec![base, plugin("a", &base_id)], "a".into(), anchor)
                .expect("a root window");
        assert_eq!(graph.anchor().map(|anchor| anchor.generation), Some(0));
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
        let derived = NodeId::from(format!("derived-{turn}"));
        let warm = graph.read_model();
        graph.remap_node_ids(&session, &[(draft, derived.clone())]);
        graph.apply_realized_node_timestamps(&[RealizedNodeTimestamp {
            node_id: derived.clone(),
            timestamp: format!("2026-09-29T00:00:{:02}Z", turn % 60),
        }]);
        let read = graph.read_model();
        assert!(
            lash_sansio::AppendVec::ptr_eq(&warm.messages, &read.messages)
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
            leaf_node.timestamp,
            format!("2026-09-29T00:00:{:02}Z", turn % 60)
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

/// The render cache a fold makes extends the previous render instead of
/// re-rendering the frame, and gives exactly the render of the whole frame.
#[test]
fn an_extended_render_cache_equals_a_fresh_render_of_the_frame() {
    const TURNS: usize = 64;
    let mut graph = SessionGraph::default();
    let mut renders = Vec::new();
    for turn in 0..TURNS {
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
        let rendered = read
            .prompt_render_cache
            .rendered(read.messages.as_slice())
            .clone();
        assert_eq!(
            rendered.as_slice(),
            lash_sansio::session_model::render_prompt(read.messages.as_slice())
                .messages
                .as_slice()
        );
        renders.push(rendered);
    }
    let buffers = renders
        .iter()
        .map(|rendered| rendered.as_ptr())
        .collect::<HashSet<_>>();
    assert!(
        buffers.len() <= 8,
        "{} render buffers for {TURNS} turns: each render extends the last",
        buffers.len()
    );
}
