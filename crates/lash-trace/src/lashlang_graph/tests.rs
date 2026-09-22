use chrono::{TimeZone, Utc};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

use super::*;
use crate::{
    TraceBranchSelection, TraceContext, TraceLabelMetadata, TraceLanguageChildExecution,
    TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode, TraceRuntimeScope,
    TraceRuntimeSubject,
};

fn identity() -> LanguageIdentity {
    LanguageIdentity {
        scope: TraceRuntimeScope {
            session_id: Some(SessionId::from("session-1".to_string())),
            turn_id: Some(TurnId::from("turn-1")),
            turn_index: Some(0),
            protocol_iteration: Some(0),
        },
        subject: TraceRuntimeSubject::Effect {
            address: lash_sansio::EffectAddress::new(
                lash_sansio::ExecutionScope::turn("session-1", "turn-1"),
                "exec-replay-1",
            )
            .expect("valid trace test effect address"),
            effect_id: "exec-1".to_string(),
        },
        module_ref: "module-1".to_string(),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        restate_invocation_id: None,
        generation: None,
    }
}

const EFFECT_GRAPH_KEY: &str = r#"effect:{"version":2,"kind":"turn","session_id":"session-1","execution_id":"turn-1"}:"exec-replay-1""#;

fn record_at(event: TraceLanguageExecution, ms: i64) -> TraceRecord {
    TraceRecord::new_with_timestamp(
        TraceContext::default().for_session("session-1"),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event,
        },
        Utc.timestamp_millis_opt(ms).single().expect("timestamp"),
    )
}

fn append_at(store: &TraceLashlangGraphStore, event: TraceLanguageExecution, ms: i64) {
    store
        .append(&record_at(event, ms))
        .expect("append lashlang execution event");
}

fn started_event(event_key: &str) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: LanguageExecutionMap {
                nodes: vec![
                    TraceLanguageExecutionMapNode {
                        id: "branch".to_string(),
                        site: lash_sansio::WorkflowExecutionSite::new(
                            "main",
                            [0],
                            "branch",
                            "if ready",
                        ),
                        kind: "branch".to_string(),
                        label: "if ready".to_string(),
                        label_metadata: None,
                    },
                    TraceLanguageExecutionMapNode {
                        id: "then".to_string(),
                        site: lash_sansio::WorkflowExecutionSite::new(
                            "main",
                            [0, 1, 0],
                            "call",
                            "notify()",
                        ),
                        kind: "call".to_string(),
                        label: "notify()".to_string(),
                        label_metadata: None,
                    },
                    TraceLanguageExecutionMapNode {
                        id: "else".to_string(),
                        site: lash_sansio::WorkflowExecutionSite::new(
                            "main",
                            [0, 2, 0],
                            "call",
                            "skip()",
                        ),
                        kind: "call".to_string(),
                        label: "skip()".to_string(),
                        label_metadata: None,
                    },
                ],
                // `sequence` is what the producer emits for a control edge
                // (`WorkflowEdgeKind::Sequence`); the fixture used to
                // invent `then` / `else` labels so the deleted string
                // inference had something to match.
                edges: vec![
                    TraceLanguageExecutionMapEdge {
                        id: "then-edge".to_string(),
                        from: "branch".to_string(),
                        to: "then".to_string(),
                        label: "sequence".to_string(),
                    },
                    TraceLanguageExecutionMapEdge {
                        id: "else-edge".to_string(),
                        from: "branch".to_string(),
                        to: "else".to_string(),
                        label: "sequence".to_string(),
                    },
                ],
            },
        },
    }
}

fn node_started(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
    node_started_for(event_key, "branch", "branch", occurrence)
}

fn node_started_for(
    event_key: &str,
    node_id: &str,
    node_kind: &str,
    occurrence: u64,
) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::NodeStarted {
            node_id: node_id.to_string(),
            node_kind: node_kind.to_string(),
            label: node_id.to_string(),
            occurrence,
            call_id: None,
        },
    }
}

fn node_completed(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
    node_completed_for(event_key, "branch", "branch", occurrence)
}

fn node_completed_for(
    event_key: &str,
    node_id: &str,
    node_kind: &str,
    occurrence: u64,
) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::NodeCompleted {
            node_id: node_id.to_string(),
            node_kind: node_kind.to_string(),
            label: node_id.to_string(),
            occurrence,
            call_id: None,
        },
    }
}

fn node_failed(event_key: &str, occurrence: u64, error: &str) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::NodeFailed {
            node_id: "branch".to_string(),
            node_kind: "branch".to_string(),
            label: "if ready".to_string(),
            occurrence,
            call_id: None,
            error: error.to_string(),
        },
    }
}

fn execution_finished(event_key: &str, status: LanguageExecutionStatus) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::ExecutionFinished {
            status,
            error: None,
        },
    }
}

#[test]
fn graph_store_seeds_static_map_on_execution_start() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, started_event("start"), 1_000);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    assert_eq!(graph.status, LanguageExecutionStatus::Running);
    assert_eq!(
        graph.nodes[0].observation,
        TraceLashlangNodeObservation::Unobserved
    );
    assert_eq!(
        graph.edges[0].selection,
        TraceLashlangEdgeSelection::Unknown
    );
}

#[test]
fn graph_store_keeps_distinct_site_kinds_for_one_structural_node() {
    let store = TraceLashlangGraphStore::default();
    let mut event = started_event("start");
    if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map } = &mut event.payload {
        execution_map.nodes.push(TraceLanguageExecutionMapNode {
            id: "branch".to_string(),
            site: lash_sansio::WorkflowExecutionSite::new(
                "main",
                [0],
                "resource_operation",
                "condition",
            ),
            kind: "resource_operation".to_string(),
            label: "condition".to_string(),
            label_metadata: None,
        });
    }

    append_at(&store, event, 1_000);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    let sites = graph
        .nodes
        .iter()
        .filter(|node| node.id == "branch")
        .map(|node| node.kind.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(sites, BTreeSet::from(["branch", "resource_operation"]));
}

/// A TypeScript session's executions reduce into the same projection.
///
/// A record carries the dialect of the source that ran. Ignoring any
/// dialect other than Lashlang would empty every TypeScript session's
/// execution view, although both dialects run on the same VM.
#[test]
fn graph_store_reduces_every_dialects_execution_events() {
    let store = TraceLashlangGraphStore::default();
    store
        .append(&TraceRecord::new(
            TraceContext::default().for_session("session-1"),
            TraceEvent::LanguageExecution {
                language: "typescript".to_string(),
                event: started_event("start"),
            },
        ))
        .expect("append a TypeScript execution event");

    assert!(!store.graphs().is_empty());
}

#[test]
fn graph_store_preserves_static_label_metadata() {
    let store = TraceLashlangGraphStore::default();
    let mut event = started_event("start");
    if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map, .. } =
        &mut event.payload
    {
        execution_map.nodes[0].label_metadata = Some(TraceLabelMetadata {
            title: "Choose path".to_string(),
            description: Some("Branch detail".to_string()),
        });
    }

    append_at(&store, event, 1_000);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    assert_eq!(
        graph.nodes[0].label_metadata,
        Some(TraceLabelMetadata {
            title: "Choose path".to_string(),
            description: Some("Branch detail".to_string()),
        })
    );
}

#[test]
fn graph_store_deduplicates_by_logical_identity_not_event_key() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, node_started("same-key", 1), 1_000);
    append_at(&store, node_completed("same-key", 1), 1_250);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    assert!(matches!(
        graph.nodes[0].observation,
        TraceLashlangNodeObservation::Completed { occurrence: 1, .. }
    ));
}

#[test]
fn graph_store_updates_completed_node_duration() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, node_started("start-node", 1), 1_000);
    append_at(&store, node_completed("complete-node", 1), 1_750);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    let node = &graph.nodes[0];
    assert!(matches!(
        node.observation,
        TraceLashlangNodeObservation::Completed {
            occurrence: 1,
            duration_ms: Some(750),
            ..
        }
    ));
}

#[test]
fn graph_store_reentered_node_resets_error_and_measures_current_occurrence() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, node_started("first-start", 1), 1_000);
    append_at(
        &store,
        node_failed("first-failure", 1, "first failed"),
        1_250,
    );
    append_at(&store, node_started("second-start", 2), 2_000);
    append_at(&store, node_completed("second-complete", 2), 2_400);

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    let node = &graph.nodes[0];
    assert!(matches!(
        node.observation,
        TraceLashlangNodeObservation::Completed {
            occurrence: 2,
            duration_ms: Some(400),
            ..
        }
    ));
    let serialized = serde_json::to_value(node).expect("serialize completed node");
    assert_eq!(serialized.get("error"), None);
}

#[test]
fn graph_store_terminal_event_for_different_occurrence_uses_terminal_timestamp() {
    for terminal in [
        node_completed("complete-node", 2),
        node_failed("fail-node", 2, "failed"),
    ] {
        let store = TraceLashlangGraphStore::default();
        append_at(&store, node_started("start-node", 1), 1_000);
        append_at(&store, terminal, 1_750);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let (occurrence, start, _end, duration_ms) = match &graph.nodes[0].observation {
            TraceLashlangNodeObservation::Completed {
                occurrence,
                start,
                end,
                duration_ms,
            }
            | TraceLashlangNodeObservation::Failed {
                occurrence,
                start,
                end,
                duration_ms,
                ..
            } => (occurrence, start, end, duration_ms),
            observation => panic!("node was not terminal: {observation:#?}"),
        };
        assert_eq!(*occurrence, 2);
        assert_eq!(*start, None);
        assert_eq!(*duration_ms, None);
    }
}

#[test]
fn graph_store_branch_selection_completes_unstarted_node_with_zero_duration() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, started_event("start"), 1_000);
    append_at(
        &store,
        TraceLanguageExecution {
            event_key: "branch".to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::BranchSelected {
                node_id: "branch".to_string(),
                occurrence: 1,
                edge_id: "then-edge".to_string(),
                selected: TraceBranchSelection::Then,
            },
        },
        1_100,
    );

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == "branch")
        .expect("branch node");
    let TraceLashlangNodeObservation::Completed {
        occurrence,
        start,
        end: _,
        duration_ms,
    } = &node.observation
    else {
        panic!("branch node was not completed: {node:#?}");
    };
    assert_eq!(*occurrence, 1);
    assert_eq!(*start, None);
    assert_eq!(*duration_ms, None);

    let serialized = serde_json::to_value(node).expect("serialize branch node");
    assert_eq!(serialized["status"], "completed");
    assert!(serialized.get("duration_ms").is_none());
    assert!(serialized.get("observation").is_none());
}

#[test]
fn graph_store_records_the_typed_branch_arm_and_marks_the_selected_edge() {
    let store = TraceLashlangGraphStore::default();

    append_at(&store, started_event("start"), 1_000);
    append_at(
        &store,
        TraceLanguageExecution {
            event_key: "branch".to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::BranchSelected {
                node_id: "branch".to_string(),
                occurrence: 1,
                edge_id: "then-edge".to_string(),
                selected: TraceBranchSelection::Then,
            },
        },
        1_100,
    );

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    let branch = graph
        .nodes
        .iter()
        .find(|node| node.id == "branch")
        .expect("branch node");
    let serialized = serde_json::to_value(branch).expect("serialize branch node");
    assert_eq!(serialized["branch_selection"], serde_json::json!("then"));
    // The selection rides beside a flattened observation, so pin the
    // round trip rather than only the encode.
    assert_eq!(
        &serde_json::from_value::<TraceLashlangGraphNode>(serialized).expect("decode branch node"),
        branch,
    );
    assert_eq!(
        graph
            .edges
            .iter()
            .find(|edge| edge.id == "then-edge")
            .map(|edge| edge.selection),
        Some(TraceLashlangEdgeSelection::Selected)
    );
    // The sibling edge stays unmarked: no live producer labels a branch
    // arm, so the reducer cannot tell an unselected arm from an ordinary
    // sequencing or data-dependency edge leaving the same node (ADR 0037).
    assert_eq!(
        graph
            .edges
            .iter()
            .find(|edge| edge.id == "else-edge")
            .map(|edge| edge.selection),
        Some(TraceLashlangEdgeSelection::Unknown)
    );
}

#[test]
fn graph_store_records_child_links() {
    let store = TraceLashlangGraphStore::default();

    append_at(
        &store,
        TraceLanguageExecution {
            event_key: "child".to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::ChildStarted {
                parent_node_id: "spawn".to_string(),
                occurrence: 1,
                child: TraceLanguageChildExecution {
                    scope: TraceRuntimeScope::new("session-1"),
                    process_id: ProcessId::from("process:child".to_string()),
                    incarnation: 7,
                    attempt: Some(2),
                    module_ref: Some("module-1".to_string()),
                    entry_ref: Some("process:0".to_string()),
                    entry_name: Some("child".to_string()),
                },
            },
        },
        1_000,
    );

    let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
    assert_eq!(graph.children[0].parent_node_id, "spawn");
    assert_eq!(
        graph.children[0].child_graph_key.as_deref(),
        Some("process:process:child:incarnation:7:attempt:2")
    );
    assert_eq!(graph.children[0].child_entry_name.as_deref(), Some("child"));
}

#[test]
fn child_links_join_exact_attempts_and_reused_process_incarnations() {
    let store = TraceLashlangGraphStore::default();
    let child_link = |occurrence, incarnation, attempt| TraceLanguageExecution {
        event_key: format!("child-{incarnation}-{attempt}"),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id: "spawn".to_string(),
            occurrence,
            child: TraceLanguageChildExecution {
                scope: TraceRuntimeScope::new("session-1"),
                process_id: ProcessId::from("worker"),
                incarnation,
                attempt: Some(attempt),
                module_ref: Some("module-1".to_string()),
                entry_ref: Some("process:0".to_string()),
                entry_name: Some("worker".to_string()),
            },
        },
    };
    append_at(&store, child_link(1, 7, 2), 1_000);
    append_at(&store, child_link(2, 8, 1), 1_100);

    for (incarnation, attempt, timestamp) in [(7, 2, 2_000), (8, 1, 3_000)] {
        let mut child = started_event(&format!("child-start-{incarnation}-{attempt}"));
        child.identity.subject = TraceRuntimeSubject::Process {
            process_id: ProcessId::from("worker"),
        };
        child.identity.generation = Some(crate::TraceLanguageExecutionGeneration::new(
            attempt,
            incarnation,
        ));
        append_at(&store, child, timestamp);
    }

    let parent = store.graph(EFFECT_GRAPH_KEY).expect("parent graph");
    assert_eq!(parent.children.len(), 2);
    assert_eq!(
        parent.children[0].child_graph_key.as_deref(),
        Some("process:worker:incarnation:7:attempt:2")
    );
    assert_eq!(
        parent.children[1].child_graph_key.as_deref(),
        Some("process:worker:incarnation:8:attempt:1")
    );
}

#[test]
fn regression_fold_is_independent_of_the_three_temporal_arrival_orders() {
    let orders = [
        vec![
            (started_event("seed"), 900),
            (node_started("node-start", 1), 1_000),
            (node_completed("node-complete", 1), 1_250),
        ],
        vec![
            (node_started("node-start", 1), 1_000),
            (node_completed("node-complete", 1), 1_250),
            (started_event("seed"), 900),
        ],
        vec![
            (node_completed("node-complete", 1), 1_250),
            (node_started("node-start", 1), 1_000),
            (started_event("seed"), 900),
        ],
    ];
    let snapshots = orders.map(|events| {
        let store = TraceLashlangGraphStore::default();
        for (event, timestamp) in events {
            append_at(&store, event, timestamp);
        }
        serde_json::to_vec(&store.graph(EFFECT_GRAPH_KEY).expect("graph")).expect("serialize graph")
    });

    assert_eq!(snapshots[0], snapshots[1]);
    assert_eq!(snapshots[0], snapshots[2]);
}

#[test]
fn missing_and_late_seed_are_explicit_and_do_not_change_observations() {
    let events = [
        record_at(node_started("start", 1), 1_000),
        record_at(node_completed("complete", 1), 1_250),
    ];
    let missing = TraceLashlangGraphStore::fold(None, &events).expect("fold without map");
    assert_eq!(
        missing.completeness,
        TraceLashlangGraphCompleteness::IncompleteMap
    );

    let late =
        TraceLashlangGraphStore::fold(Some(&missing), &[record_at(started_event("seed"), 900)])
            .expect("late seed");
    assert_eq!(late.completeness, TraceLashlangGraphCompleteness::Complete);
    assert!(matches!(
        late.nodes[0].observation,
        TraceLashlangNodeObservation::Completed {
            duration_ms: Some(250),
            ..
        }
    ));
}

#[test]
fn identical_duplicate_is_a_noop_and_conflicting_duplicate_is_typed() {
    let start = record_at(node_started("publisher-a", 1), 1_000);
    let mut duplicate = start.clone();
    let TraceEvent::LanguageExecution { event, .. } = &mut duplicate.event else {
        unreachable!()
    };
    event.event_key = "publisher-b".to_string();
    let deduplicated =
        TraceLashlangGraphStore::fold(None, &[start.clone(), duplicate]).expect("duplicate");
    assert_eq!(deduplicated.history.len(), 1);
    assert!(deduplicated.conflicts.is_empty());

    let conflict = TraceLashlangGraphStore::fold(
        None,
        &[start, record_at(node_started("publisher-c", 1), 1_001)],
    )
    .expect("conflict");
    assert_eq!(conflict.conflicts.len(), 1);
    assert_eq!(
        conflict.conflicts[0].kind,
        TraceLashlangGraphConflictKind::ConflictingDuplicate
    );
    assert_eq!(conflict.conflicts[0].variants.len(), 2);
}

#[test]
fn terminal_occurrence_never_downgrades_and_next_occurrence_is_visible() {
    let graph = TraceLashlangGraphStore::fold(
        None,
        &[
            record_at(node_completed("complete-1", 1), 1_250),
            record_at(node_started("late-start-1", 1), 1_000),
            record_at(node_started("start-2", 2), 2_000),
        ],
    )
    .expect("fold occurrences");
    assert!(matches!(
        graph.nodes[0].observation,
        TraceLashlangNodeObservation::Running { occurrence: 2, .. }
    ));
    assert_eq!(graph.nodes[0].summary.retained_occurrences, 2);
    assert_eq!(graph.nodes[0].summary.terminal_count, 1);
}

#[test]
fn terminal_execution_status_beats_a_conflicting_running_status() {
    let events = [
        record_at(
            execution_finished("terminal", LanguageExecutionStatus::Completed),
            2_000,
        ),
        record_at(
            execution_finished("running", LanguageExecutionStatus::Running),
            1_000,
        ),
    ];
    for ordered in [events.clone(), [events[1].clone(), events[0].clone()]] {
        let graph = TraceLashlangGraphStore::fold(None, &ordered).expect("fold statuses");
        assert_eq!(graph.status, LanguageExecutionStatus::Completed);
        assert_eq!(graph.conflicts.len(), 1);
    }
}

#[test]
fn branch_selection_and_terminal_observation_are_distinct_facts() {
    let selection = TraceLanguageExecution {
        event_key: "branch".to_string(),
        identity: identity(),
        payload: TraceLanguageExecutionPayload::BranchSelected {
            node_id: "branch".to_string(),
            occurrence: 1,
            edge_id: "then-edge".to_string(),
            selected: TraceBranchSelection::Then,
        },
    };
    for terminal in [
        node_completed("complete", 1),
        node_failed("failed", 1, "branch failed"),
    ] {
        for ordered in [
            vec![
                record_at(started_event("seed"), 900),
                record_at(node_started("start", 1), 1_000),
                record_at(selection.clone(), 1_100),
                record_at(terminal.clone(), 1_200),
            ],
            vec![
                record_at(started_event("seed"), 900),
                record_at(node_started("start", 1), 1_000),
                record_at(terminal.clone(), 1_200),
                record_at(selection.clone(), 1_100),
            ],
        ] {
            let graph = TraceLashlangGraphStore::fold(None, &ordered).expect("fold branch facts");
            let branch = graph
                .nodes
                .iter()
                .find(|node| node.id == "branch")
                .expect("branch node");
            assert_eq!(branch.branch_selection, Some(TraceBranchSelection::Then));
            match (&terminal.payload, &branch.observation) {
                (
                    TraceLanguageExecutionPayload::NodeCompleted { .. },
                    TraceLashlangNodeObservation::Completed {
                        start,
                        end,
                        duration_ms,
                        ..
                    },
                ) => {
                    assert_eq!(start.map(|value| value.timestamp_millis()), Some(1_000));
                    assert_eq!(end.timestamp_millis(), 1_200);
                    assert_eq!(*duration_ms, Some(200));
                }
                (
                    TraceLanguageExecutionPayload::NodeFailed { .. },
                    TraceLashlangNodeObservation::Failed {
                        start,
                        end,
                        duration_ms,
                        error,
                        ..
                    },
                ) => {
                    assert_eq!(start.map(|value| value.timestamp_millis()), Some(1_000));
                    assert_eq!(end.timestamp_millis(), 1_200);
                    assert_eq!(*duration_ms, Some(200));
                    assert_eq!(error, "branch failed");
                }
                pair => panic!("explicit terminal did not dominate selection: {pair:?}"),
            }
        }
    }
}

#[test]
fn per_node_retention_preserves_evicted_terminal_and_smaller_node_progress() {
    let events = [
        record_at(node_started_for("a-start", "a", "call", 1), 1_000),
        record_at(node_completed_for("a-end", "a", "call", 1), 1_100),
        record_at(node_started_for("z-one", "z", "call", 1), 2_000),
        record_at(node_started_for("z-two", "z", "call", 2), 3_000),
        record_at(node_started_for("a-two", "a", "call", 2), 4_000),
    ];
    for with_seed in [false, true] {
        let mut input = events.to_vec();
        if with_seed {
            input.push(record_at(started_event("seed"), 900));
        }
        let batch = TraceLashlangGraphStore::fold_with_history_limit(None, &input, 1)
            .expect("bounded per-node batch");
        let a = batch.nodes.iter().find(|node| node.id == "a").expect("a");
        assert_eq!(a.summary.terminal_count, 1);
        assert!(matches!(
            a.observation,
            TraceLashlangNodeObservation::Running { occurrence: 2, .. }
        ));
        assert!(batch.history.iter().any(|item| {
            item.identity.node_id.as_deref() == Some("a") && item.identity.occurrence == Some(2)
        }));
        assert!(batch.history.iter().any(|item| {
            item.identity.node_id.as_deref() == Some("z") && item.identity.occurrence == Some(2)
        }));

        for ordered in test_permutations(&input) {
            let permuted = TraceLashlangGraphStore::fold_with_history_limit(None, &ordered, 1)
                .expect("permuted batch");
            assert_eq!(permuted, batch);
            for split in 1..ordered.len() {
                let first =
                    TraceLashlangGraphStore::fold_with_history_limit(None, &ordered[..split], 1)
                        .expect("first partition");
                let incremental = TraceLashlangGraphStore::fold_with_history_limit(
                    Some(&first),
                    &ordered[split..],
                    1,
                )
                .expect("second partition");
                assert_eq!(incremental, batch);
            }
        }
    }
}

#[test]
fn default_per_node_limit_retains_the_latest_occurrence_and_summary() {
    for with_seed in [false, true] {
        let mut records = (1..=DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT as u64 + 1)
            .map(|occurrence| {
                record_at(
                    node_started_for("start", "a", "call", occurrence),
                    occurrence as i64,
                )
            })
            .collect::<Vec<_>>();
        if with_seed {
            records.push(record_at(started_event("seed"), 0));
        }
        let graph = TraceLashlangGraphStore::fold(None, &records).expect("default limit");
        let node = graph.nodes.iter().find(|node| node.id == "a").expect("a");
        assert_eq!(
            node.summary.retained_occurrences,
            DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT as u64 + 1
        );
        assert!(matches!(
            node.observation,
            TraceLashlangNodeObservation::Running { occurrence, .. }
                if occurrence == DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT as u64 + 1
        ));
        assert_eq!(graph.node_retention[0].truncation_watermark, 1);
    }
}

#[test]
fn conflicting_identity_metadata_is_canonical_across_permutations_and_partitions() {
    let mut a = node_started("a", 1);
    a.identity.module_ref = "z-module".to_string();
    a.identity.entry_name = "z-entry".to_string();
    let mut b = node_started("b", 1);
    b.identity.module_ref = "a-module".to_string();
    b.identity.entry_name = "a-entry".to_string();
    let tail = node_started("tail", 2);
    let seed = started_event("seed");
    for events in [
        vec![a.clone(), b.clone(), tail.clone()],
        vec![seed, a, b, tail],
    ] {
        let records = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| record_at(event, 1_000 + index as i64))
            .collect::<Vec<_>>();
        let expected = TraceLashlangGraphStore::fold_with_history_limit(None, &records, 1)
            .expect("canonical batch");
        assert_eq!(expected.module_ref, "a-module");
        assert_eq!(expected.entry_name, "a-entry");
        assert_eq!(expected.node_retention[0].truncation_watermark, 1);
        for ordered in test_permutations(&records) {
            let actual = TraceLashlangGraphStore::fold_with_history_limit(None, &ordered, 1)
                .expect("permutation");
            assert_eq!(actual, expected);
            for boundaries in 0..(1_u32 << (ordered.len() - 1)) {
                let mut previous = None;
                let mut start = 0;
                for index in 0..ordered.len() {
                    if index + 1 == ordered.len() || boundaries & (1 << index) != 0 {
                        previous = Some(
                            TraceLashlangGraphStore::fold_with_history_limit(
                                previous.as_ref(),
                                &ordered[start..=index],
                                1,
                            )
                            .expect("incremental partition"),
                        );
                        start = index + 1;
                    }
                }
                let incremental = previous.expect("at least one partition");
                assert_eq!(incremental, expected);
            }
        }
    }
}

fn test_permutations(records: &[TraceRecord]) -> Vec<Vec<TraceRecord>> {
    if records.len() <= 1 {
        return vec![records.to_vec()];
    }
    let mut permutations = Vec::new();
    for index in 0..records.len() {
        let mut rest = records.to_vec();
        let head = rest.remove(index);
        for mut tail in test_permutations(&rest) {
            let mut permutation = vec![head.clone()];
            permutation.append(&mut tail);
            permutations.push(permutation);
        }
    }
    permutations
}

#[test]
fn late_seed_after_execution_finished_preserves_exact_terminal_status() {
    for status in [
        LanguageExecutionStatus::Completed,
        LanguageExecutionStatus::Failed,
        LanguageExecutionStatus::Cancelled,
    ] {
        let finished = record_at(execution_finished("finished", status), 2_000);
        let before_seed = TraceLashlangGraphStore::fold(None, &[finished]).expect("finished");
        let after_seed = TraceLashlangGraphStore::fold(
            Some(&before_seed),
            &[record_at(started_event("late-seed"), 1_000)],
        )
        .expect("late seed");
        assert_eq!(after_seed.status, status);
    }
}

#[test]
fn every_permutation_and_incremental_partition_is_byte_identical() {
    let events = [
        record_at(started_event("seed"), 900),
        record_at(node_started("start", 1), 1_000),
        record_at(node_completed("complete", 1), 1_250),
    ];
    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let expected = TraceLashlangGraphStore::fold(None, &events).expect("batch");
    let expected_bytes = serde_json::to_vec(&expected).expect("serialize batch");
    for permutation in permutations {
        let batch = permutation.map(|index| events[index].clone());
        let actual = TraceLashlangGraphStore::fold(None, &batch).expect("permuted batch");
        assert_eq!(
            serde_json::to_vec(&actual).expect("serialize permutation"),
            expected_bytes
        );
    }
    for split in 1..events.len() {
        let first = TraceLashlangGraphStore::fold(None, &events[..split]).expect("first partition");
        let second = TraceLashlangGraphStore::fold(Some(&first), &events[split..])
            .expect("second partition");
        assert_eq!(
            serde_json::to_vec(&second).expect("serialize incremental"),
            expected_bytes
        );
    }
}

#[test]
fn history_is_bounded_with_a_canonical_truncation_watermark() {
    let events = [
        record_at(node_started("one", 1), 1_000),
        record_at(node_started("two", 2), 2_000),
        record_at(node_started("three", 3), 3_000),
    ];
    let batch =
        TraceLashlangGraphStore::fold_with_history_limit(None, &events, 2).expect("bounded batch");
    assert_eq!(batch.history.len(), 2);
    assert_eq!(
        batch
            .node_retention
            .first()
            .map(|retention| retention.truncation_watermark),
        Some(1)
    );
    let first = TraceLashlangGraphStore::fold_with_history_limit(None, &events[1..], 2)
        .expect("high identities first");
    let incremental =
        TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &events[..1], 2)
            .expect("late truncated identity");
    assert_eq!(
        serde_json::to_vec(&incremental).expect("serialize incremental"),
        serde_json::to_vec(&batch).expect("serialize batch")
    );
}

#[test]
fn late_static_map_below_the_watermark_matches_batch_folding() {
    let seed = record_at(started_event("seed"), 900);
    let node_events = [
        record_at(node_started("one", 1), 1_000),
        record_at(node_started("two", 2), 2_000),
    ];
    let batch = TraceLashlangGraphStore::fold_with_history_limit(
        None,
        &[seed.clone(), node_events[0].clone(), node_events[1].clone()],
        1,
    )
    .expect("bounded batch");
    let first = TraceLashlangGraphStore::fold_with_history_limit(None, &node_events, 1)
        .expect("node events first");
    let incremental = TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &[seed], 1)
        .expect("late static map");

    assert_eq!(incremental, batch);
    assert_eq!(
        incremental.completeness,
        TraceLashlangGraphCompleteness::Complete
    );
}

#[test]
fn terminal_graph_status_survives_history_truncation() {
    let finished = record_at(
        execution_finished("finished", LanguageExecutionStatus::Completed),
        3_000,
    );
    let node_events = [
        record_at(node_started("one", 1), 1_000),
        record_at(node_started("two", 2), 2_000),
    ];
    let batch = TraceLashlangGraphStore::fold_with_history_limit(
        None,
        &[
            finished.clone(),
            node_events[0].clone(),
            node_events[1].clone(),
        ],
        1,
    )
    .expect("bounded batch");
    let first = TraceLashlangGraphStore::fold_with_history_limit(None, &node_events, 1)
        .expect("node events first");
    let incremental =
        TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &[finished], 1)
            .expect("late terminal status");

    assert_eq!(incremental, batch);
    assert_eq!(incremental.status, LanguageExecutionStatus::Completed);
}

#[test]
fn terminal_classification_is_exhaustive() {
    assert!(!LanguageExecutionStatus::Running.is_terminal());
    assert!(LanguageExecutionStatus::Completed.is_terminal());
    assert!(LanguageExecutionStatus::Failed.is_terminal());
    assert!(LanguageExecutionStatus::Cancelled.is_terminal());

    assert!(!TraceLashlangNodeObservation::Unobserved.is_terminal());
    assert!(
        !TraceLashlangNodeObservation::Running {
            occurrence: 1,
            start: Utc.timestamp_millis_opt(1).single().expect("timestamp"),
        }
        .is_terminal()
    );
    assert!(
        TraceLashlangNodeObservation::Completed {
            occurrence: 1,
            start: None,
            end: Utc.timestamp_millis_opt(2).single().expect("timestamp"),
            duration_ms: None,
        }
        .is_terminal()
    );
    assert!(
        TraceLashlangNodeObservation::Failed {
            occurrence: 1,
            start: None,
            end: Utc.timestamp_millis_opt(2).single().expect("timestamp"),
            duration_ms: None,
            error: "failed".to_string(),
        }
        .is_terminal()
    );
}

#[test]
fn graph_decode_checks_version_before_shape_and_tolerates_additive_fields() {
    let graph = TraceLashlangGraphStore::fold(None, &[record_at(node_started("start", 1), 1_000)])
        .expect("graph");
    let mut value = serde_json::to_value(&graph).expect("encode graph");
    value["future_field"] = serde_json::json!(true);
    assert_eq!(
        serde_json::from_value::<TraceLashlangGraph>(value.clone()).expect("additive field"),
        graph
    );
    value["schema_version"] = serde_json::json!(TRACE_SCHEMA_VERSION - 1);
    value["completeness"] = serde_json::json!("future_variant");
    let error = serde_json::from_value::<TraceLashlangGraph>(value)
        .expect_err("predecessor must be refused before shape");
    assert!(
        error
            .to_string()
            .contains("unsupported trace schema version")
    );
}

#[test]
fn graph_decode_refuses_unknown_closed_variant_at_the_current_version() {
    let graph = TraceLashlangGraphStore::fold(None, &[record_at(node_started("start", 1), 1_000)])
        .expect("graph");
    let value = serde_json::to_value(graph).expect("encode graph");
    for (field, changed) in [
        ("completeness", {
            let mut changed = value.clone();
            changed["completeness"] = serde_json::json!("future_variant");
            changed
        }),
        ("graph status", {
            let mut changed = value.clone();
            changed["status"] = serde_json::json!("future_variant");
            changed
        }),
        ("node observation status", {
            let mut changed = value.clone();
            changed["nodes"][0]["status"] = serde_json::json!("future_variant");
            changed
        }),
    ] {
        let error = serde_json::from_value::<TraceLashlangGraph>(changed)
            .expect_err(&format!("unknown {field} variant must be refused"));
        assert!(
            error.to_string().contains("future_variant"),
            "unexpected {field} error: {error}"
        );
    }
}
