use super::super::*;
use super::harness::AgentScenarioRun;
use std::collections::BTreeSet;

#[derive(Debug)]
pub(super) struct GraphContract {
    graphs: Vec<GraphFact>,
    child_links: Vec<ChildLinkFact>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct GraphFact {
    graph_key: String,
    session_id: String,
    turn_id: Option<String>,
    subject_kind: String,
    subject_id: String,
    entry_kind: String,
    entry_name: String,
    status: crate::tracing::TraceLashlangStatus,
    nodes: Vec<NodeFact>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct NodeFact {
    graph_key: String,
    kind: String,
    label: String,
    label_title: Option<String>,
    status: crate::tracing::TraceLashlangNodeStatus,
    has_error: bool,
}

#[allow(dead_code)]
#[derive(Debug)]
struct ChildLinkFact {
    parent_graph_key: String,
    parent_entry_name: String,
    parent_node_kind: Option<String>,
    parent_node_label_title: Option<String>,
    child_graph_key: String,
    child_entry_name: Option<String>,
}

impl GraphContract {
    pub(super) fn from_graphs(graphs: &[crate::tracing::TraceLashlangGraph]) -> Self {
        let mut facts = Vec::new();
        let mut links = Vec::new();
        for graph in graphs {
            let (subject_kind, subject_id) = match &graph.subject {
                crate::tracing::TraceRuntimeSubject::Effect { effect_id, kind } => {
                    (format!("effect:{kind}"), effect_id.clone())
                }
                crate::tracing::TraceRuntimeSubject::Process { process_id } => {
                    ("process".to_string(), process_id.clone())
                }
            };
            facts.push(GraphFact {
                graph_key: graph.graph_key.clone(),
                session_id: graph.scope.session_id.clone(),
                turn_id: graph.scope.turn_id.clone(),
                subject_kind,
                subject_id,
                entry_kind: graph.entry_kind.clone(),
                entry_name: graph.entry_name.clone(),
                status: graph.status,
                nodes: graph
                    .nodes
                    .iter()
                    .map(|node| NodeFact {
                        graph_key: graph.graph_key.clone(),
                        kind: node.kind.clone(),
                        label: node.label.clone(),
                        label_title: node
                            .label_metadata
                            .as_ref()
                            .map(|label| label.title.clone()),
                        status: node.status,
                        has_error: node.latest_error.is_some(),
                    })
                    .collect(),
            });
            for child in &graph.children {
                let parent = graph
                    .nodes
                    .iter()
                    .find(|node| node.id == child.parent_node_id);
                links.push(ChildLinkFact {
                    parent_graph_key: child.parent_graph_key.clone(),
                    parent_entry_name: graph.entry_name.clone(),
                    parent_node_kind: parent.map(|node| node.kind.clone()),
                    parent_node_label_title: parent
                        .and_then(|node| node.label_metadata.as_ref())
                        .map(|label| label.title.clone()),
                    child_graph_key: child.child_graph_key.clone(),
                    child_entry_name: child.child_entry_name.clone(),
                });
            }
        }
        Self {
            graphs: facts,
            child_links: links,
        }
    }

    fn graph_keys(&self) -> BTreeSet<&str> {
        self.graphs
            .iter()
            .map(|graph| graph.graph_key.as_str())
            .collect()
    }

    fn nodes(&self) -> impl Iterator<Item = &NodeFact> {
        self.graphs.iter().flat_map(|graph| graph.nodes.iter())
    }
}

pub(super) fn assert_successful_agent_scenario(run: &AgentScenarioRun) {
    assert_no_unexpected_turn_errors(&run.streamed_events);
    assert_successful_lash_code_path(&run.streamed_events);
    assert_all_processes_terminal(&run.final_process_list);
    let output = run.turn_output.as_ref().expect("turn output");
    assert!(
        output.is_success(),
        "turn should have succeeded: {:?}",
        output.outcome
    );
    let contract = GraphContract::from_graphs(&run.graph_snapshots);
    assert_foreground_exec_graph_completed(run);
    assert_graph_lineage_connected(&contract, &run.final_process_list);
    assert_subagent_bridge_exec_graphs(run, crate::tracing::TraceLashlangStatus::Completed);
}

fn assert_no_unexpected_turn_errors(events: &[TurnActivity]) {
    assert_no_forbidden_error_text(events);
    assert!(
        !events.iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::Error { .. } | TurnEvent::CodeBlockCompleted { success: false, .. }
        )),
        "unexpected failed turn event: {events:#?}"
    );
}

pub(super) fn assert_no_forbidden_error_text(events: &[TurnActivity]) {
    let forbidden = [
        "Invalid process handle",
        "missing __handle__",
        "deployment effect-host fallback",
        "missing scoped controller",
    ];
    for activity in events {
        let text = format!("{:?}", activity.event);
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "unexpected error text `{needle}` in event: {activity:#?}"
            );
        }
    }
}

fn assert_successful_lash_code_path(events: &[TurnActivity]) {
    let code_started = events
        .iter()
        .position(|activity| {
            matches!(
                &activity.event,
                TurnEvent::CodeBlockStarted { language, .. } if language == "lashlang"
            )
        })
        .unwrap_or_else(|| panic!("missing Lashlang code start event: {events:#?}"));
    let code_completed = events
        .iter()
        .rposition(|activity| {
            matches!(
                &activity.event,
                TurnEvent::CodeBlockCompleted { language, success: true, .. } if language == "lashlang"
            )
        })
        .unwrap_or_else(|| panic!("missing successful Lashlang code completion: {events:#?}"));
    let terminal_output = events
        .iter()
        .position(|activity| {
            matches!(
                &activity.event,
                TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
            )
        })
        .unwrap_or_else(|| panic!("missing terminal output event: {events:#?}"));
    assert!(code_started < code_completed);
    assert!(code_completed < terminal_output);
    assert!(
        !events[code_completed + 1..].iter().any(|activity| {
            matches!(
                &activity.event,
                TurnEvent::ToolCallStarted { .. } | TurnEvent::ToolCallCompleted { .. }
            )
        }),
        "tool events should not be emitted after code completion: {events:#?}"
    );
}

pub(super) fn assert_failed_code_block_present(events: &[TurnActivity]) {
    assert!(
        events.iter().any(|activity| {
            matches!(
                &activity.event,
                TurnEvent::CodeBlockCompleted {
                    success: false,
                    error: Some(_),
                    ..
                }
            )
        }),
        "missing failed code block completion: {events:#?}"
    );
}

pub(super) fn assert_no_false_finishted_success(run: &AgentScenarioRun) {
    let output = run.turn_output.as_ref().expect("turn output");
    assert!(
        output.final_value().is_none(),
        "failure scenario produced a final value: {:?}",
        output.final_value()
    );
    assert!(
        !run.streamed_events
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::FinalValue { .. })),
        "failure scenario emitted finishted success: {:#?}",
        run.streamed_events
    );
}

pub(super) fn assert_all_processes_terminal(processes: &[lash_core::ProcessHandleSummary]) {
    assert!(
        processes.iter().all(|process| process.status.is_terminal()),
        "expected all visible process handles terminal: {processes:#?}"
    );
}

fn assert_foreground_exec_graph_completed(run: &AgentScenarioRun) {
    let output = run.turn_output.as_ref().expect("turn output");
    let session_id = &output.state.session_id;
    let graph = run
        .graph_snapshots
        .iter()
        .find(|graph| {
            graph.scope.session_id == *session_id
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
                )
        })
        .unwrap_or_else(|| {
            panic!(
                "missing foreground exec graph for {session_id}: {:#?}",
                GraphContract::from_graphs(&run.graph_snapshots)
            )
        });
    assert_eq!(
        graph.status,
        crate::tracing::TraceLashlangStatus::Completed,
        "foreground exec graph did not complete: {graph:#?}"
    );
}

pub(super) fn assert_graph_lineage_connected(
    contract: &GraphContract,
    processes: &[lash_core::ProcessHandleSummary],
) {
    let graph_keys = contract.graph_keys();
    let process_ids = processes
        .iter()
        .map(|process| process.process_id.as_str())
        .collect::<BTreeSet<_>>();
    for link in &contract.child_links {
        let linked_graph_exists = graph_keys.contains(link.child_graph_key.as_str());
        let linked_process_exists = link
            .child_graph_key
            .strip_prefix("process:")
            .is_some_and(|process_id| process_ids.contains(process_id));
        assert!(
            linked_graph_exists || linked_process_exists,
            "child link points nowhere: {link:#?}\ncontract={contract:#?}\nprocesses={processes:#?}"
        );
    }
}

pub(super) fn assert_labeled_resource_operation(
    contract: &GraphContract,
    title: &str,
    expected_status: crate::tracing::TraceLashlangNodeStatus,
) {
    let node = contract
        .nodes()
        .find(|node| {
            node.kind == "resource_operation"
                && node.label_title.as_deref() == Some(title)
                && node.status == expected_status
        })
        .unwrap_or_else(|| {
            panic!(
                "missing labeled resource operation `{title}` with status {expected_status:?}: {contract:#?}"
            );
        });
    assert_eq!(
        node.status, expected_status,
        "labeled resource operation `{title}` had wrong status: {node:#?}"
    );
    if expected_status == crate::tracing::TraceLashlangNodeStatus::Failed {
        assert!(
            node.has_error,
            "failed labeled resource operation should retain node error: {node:#?}"
        );
    }
}

pub(super) fn assert_labeled_node(
    contract: &GraphContract,
    title: &str,
    expected_status: crate::tracing::TraceLashlangNodeStatus,
) {
    let node = contract
        .nodes()
        .find(|node| node.label_title.as_deref() == Some(title) && node.status == expected_status)
        .unwrap_or_else(|| {
            panic!("missing labeled node `{title}` with status {expected_status:?}: {contract:#?}")
        });
    assert_eq!(
        node.status, expected_status,
        "labeled node `{title}` had wrong status: {node:#?}"
    );
}

pub(super) fn assert_no_duplicate_label_step(contract: &GraphContract, title: &str) {
    assert!(
        !contract
            .nodes()
            .any(|node| node.kind == "step" && node.label == title),
        "label `{title}` produced a duplicate standalone step: {contract:#?}"
    );
}

pub(super) fn assert_completed_process_graph(contract: &GraphContract, entry_name: &str) {
    assert!(
        contract.graphs.iter().any(|graph| {
            graph.entry_kind == "process"
                && graph.entry_name == entry_name
                && graph.subject_kind == "process"
                && graph.status == crate::tracing::TraceLashlangStatus::Completed
        }),
        "missing completed process graph `{entry_name}`: {contract:#?}"
    );
}

pub(super) fn assert_min_completed_process_graphs(contract: &GraphContract, expected_min: usize) {
    if expected_min == 0 {
        return;
    }
    let count = contract
        .graphs
        .iter()
        .filter(|graph| {
            graph.entry_kind == "process"
                && graph.subject_kind == "process"
                && graph.status == crate::tracing::TraceLashlangStatus::Completed
        })
        .count();
    assert!(
        count >= expected_min,
        "expected at least {expected_min} completed process graphs, got {count}: {contract:#?}"
    );
}

pub(super) fn assert_min_completed_child_session_exec_graphs(
    run: &AgentScenarioRun,
    root_session_id: &str,
    expected_min: usize,
) {
    if expected_min == 0 {
        return;
    }
    let count = run
        .graph_snapshots
        .iter()
        .filter(|graph| {
            graph.scope.session_id != root_session_id
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
                )
                && graph.status == crate::tracing::TraceLashlangStatus::Completed
        })
        .count();
    assert!(
        count >= expected_min,
        "expected at least {expected_min} child-session exec graphs, got {count}: {:#?}",
        GraphContract::from_graphs(&run.graph_snapshots)
    );
}

pub(super) fn assert_subagent_bridge_exec_graphs(
    run: &AgentScenarioRun,
    expected_status: crate::tracing::TraceLashlangStatus,
) {
    let subagent_process_ids = run
        .final_process_list
        .iter()
        .filter(|process| {
            process.kind == "subagent" || process.process_id.starts_with("process:subagent:")
        })
        .map(|process| process.process_id.as_str())
        .collect::<Vec<_>>();
    if subagent_process_ids.is_empty() {
        return;
    }
    for process_id in subagent_process_ids {
        assert!(
            run.graph_snapshots.iter().any(|graph| {
                graph.scope.turn_id.as_deref() == Some(process_id)
                    && matches!(
                        &graph.subject,
                        crate::tracing::TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
                    )
                    && graph.status == expected_status
            }),
            "missing {expected_status:?} child-session exec graph for subagent process {process_id}: {:#?}",
            GraphContract::from_graphs(&run.graph_snapshots)
        );
    }
}

pub(super) fn assert_session_turn_child_graph(
    run: &AgentScenarioRun,
    child_session_id: &str,
    process_id: &str,
) {
    let graph = run
        .graph_snapshots
        .iter()
        .find(|graph| {
            graph.scope.session_id == child_session_id
                && graph.scope.turn_id.as_deref() == Some(process_id)
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
                )
        })
        .unwrap_or_else(|| {
            panic!(
                "missing scoped session-turn child exec graph: {:#?}",
                GraphContract::from_graphs(&run.graph_snapshots)
            )
        });
    assert_eq!(graph.status, crate::tracing::TraceLashlangStatus::Completed);
}
