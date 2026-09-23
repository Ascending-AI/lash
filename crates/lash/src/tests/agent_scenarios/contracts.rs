use super::super::*;
use super::harness::AgentScenarioRun;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
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
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    subject_kind: String,
    subject_id: String,
    entry_kind: String,
    entry_name: String,
    status: crate::tracing::TraceLanguageExecutionStatus,
    nodes: Vec<NodeFact>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct NodeFact {
    graph_key: String,
    kind: String,
    label: String,
    label_title: Option<String>,
    status: NodeStatusFact,
    has_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeStatusFact {
    Unobserved,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

#[allow(dead_code)]
#[derive(Debug)]
struct ChildLinkFact {
    parent_graph_key: String,
    parent_entry_name: String,
    parent_node_kind: Option<String>,
    parent_node_label_title: Option<String>,
    child_graph_key: Option<String>,
    child_process_id: lash_core::ProcessId,
    child_entry_name: Option<String>,
}

impl GraphContract {
    pub(super) fn from_graphs(graphs: &[crate::tracing::TraceLashlangGraph]) -> Self {
        let mut facts = Vec::new();
        let mut links = Vec::new();
        for graph in graphs {
            let (subject_kind, subject_id) = match &graph.subject {
                crate::tracing::TraceRuntimeSubject::Effect { effect_id, .. } => {
                    ("effect".to_string(), effect_id.clone())
                }
                crate::tracing::TraceRuntimeSubject::Process { process_id } => {
                    ("process".to_string(), process_id.to_string())
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
                    .map(|node| {
                        let (status, has_error) = match &node.observation {
                            crate::tracing::TraceLashlangNodeObservation::Unobserved => {
                                (NodeStatusFact::Unobserved, false)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Running { .. } => {
                                (NodeStatusFact::Running, false)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Waiting { .. } => {
                                (NodeStatusFact::Waiting, false)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Completed { .. } => {
                                (NodeStatusFact::Completed, false)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Failed { .. } => {
                                (NodeStatusFact::Failed, true)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Cancelled { .. } => {
                                (NodeStatusFact::Cancelled, false)
                            }
                            crate::tracing::TraceLashlangNodeObservation::Skipped { .. } => {
                                (NodeStatusFact::Skipped, false)
                            }
                        };
                        NodeFact {
                            graph_key: graph.graph_key.clone(),
                            kind: node.kind.to_string(),
                            label: node.label.clone(),
                            label_title: node
                                .label_metadata
                                .as_ref()
                                .map(|label| label.title.clone()),
                            status,
                            has_error,
                        }
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
                    parent_node_kind: parent.map(|node| node.kind.to_string()),
                    parent_node_label_title: parent
                        .and_then(|node| node.label_metadata.as_ref())
                        .map(|label| label.title.clone()),
                    child_graph_key: child.child_graph_key.clone(),
                    child_process_id: child.child_process_id.clone(),
                    child_entry_name: child.child_entry_name.clone(),
                });
            }
        }
        Self {
            graphs: facts,
            child_links: links,
        }
    }

    fn nodes(&self) -> impl Iterator<Item = &NodeFact> {
        self.graphs.iter().flat_map(|graph| graph.nodes.iter())
    }

    fn graph_keys(&self) -> BTreeSet<&str> {
        self.graphs
            .iter()
            .map(|graph| graph.graph_key.as_str())
            .collect()
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
    assert_subagent_bridge_exec_graphs(
        run,
        crate::tracing::TraceLanguageExecutionStatus::Completed,
    );
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
                TurnEvent::CodeBlockStarted { language, .. } if language == "typescript"
            )
        })
        .unwrap_or_else(|| panic!("missing TypeScript code start event: {events:#?}"));
    let code_completed = events
        .iter()
        .rposition(|activity| {
            matches!(
                &activity.event,
                TurnEvent::CodeBlockCompleted { language, success: true, .. } if language == "typescript"
            )
        })
        .unwrap_or_else(|| panic!("missing successful TypeScript code completion: {events:#?}"));
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

pub(super) fn assert_all_processes_terminal(processes: &[lash_core::ProcessHandleView]) {
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
            graph.scope.session_id.as_ref() == Some(session_id)
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { .. }
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
        crate::tracing::TraceLanguageExecutionStatus::Completed,
        "foreground exec graph did not complete: {graph:#?}"
    );
}

pub(super) fn assert_graph_lineage_connected(
    contract: &GraphContract,
    processes: &[lash_core::ProcessHandleView],
) {
    let graph_keys = contract.graph_keys();
    let process_ids = processes
        .iter()
        .map(|process| process.process_id.as_str())
        .collect::<BTreeSet<_>>();
    for link in &contract.child_links {
        let linked_graph_exists = link
            .child_graph_key
            .as_deref()
            .is_some_and(|graph_key| graph_keys.contains(graph_key));
        let linked_process_exists = process_ids.contains(link.child_process_id.as_str());
        assert!(
            linked_graph_exists || linked_process_exists,
            "child link points nowhere: {link:#?}\ncontract={contract:#?}\nprocesses={processes:#?}"
        );
    }
}

// A graph node carries a title only when the program named it, which in
// TypeScript is the `@label` doc comment on the statement (FIG-3047). These
// assertions read that title back off the executed graph, so they prove the
// whole path — doc comment, lowering, compilation, execution-site correlation
// — and not just that the lens can project one.

pub(super) fn assert_labeled_resource_operation(
    contract: &GraphContract,
    title: &str,
    expected_status: NodeStatusFact,
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
    if expected_status == NodeStatusFact::Failed {
        assert!(
            node.has_error,
            "failed labeled resource operation should retain node error: {node:#?}"
        );
    }
}

pub(super) fn assert_labeled_node(
    contract: &GraphContract,
    title: &str,
    expected_status: NodeStatusFact,
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

/// Every completed process graph is a lifted process body.
///
/// A process is an uncalled `const`-bound async arrow (FIG-2997/FIG-2999), and
/// the lifted declaration's name is a digest over the body and its AST path,
/// so an authored name is not a thing a scenario can pin. What stays
/// assertable is that each completed process graph came from a lift, and how
/// many did.
pub(super) fn assert_completed_lifted_process_graphs(contract: &GraphContract, expected: usize) {
    let lifted = contract
        .graphs
        .iter()
        .filter(|graph| {
            graph.entry_kind == "process"
                && graph.subject_kind == "process"
                && graph
                    .entry_name
                    .starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX)
                && graph.status == crate::tracing::TraceLanguageExecutionStatus::Completed
        })
        .count();
    assert_eq!(
        lifted, expected,
        "expected {expected} completed lifted process graphs, got {lifted}: {contract:#?}"
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
                && graph.status == crate::tracing::TraceLanguageExecutionStatus::Completed
        })
        .count();
    assert!(
        count >= expected_min,
        "expected at least {expected_min} completed process graphs, got {count}: {contract:#?}"
    );
}

pub(super) fn assert_min_completed_child_session_exec_graphs(
    run: &AgentScenarioRun,
    root_session_id: &SessionId,
    expected_min: usize,
) {
    if expected_min == 0 {
        return;
    }
    let count = run
        .graph_snapshots
        .iter()
        .filter(|graph| {
            graph.scope.session_id.as_ref() != Some(root_session_id)
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { .. }
                )
                && graph.status == crate::tracing::TraceLanguageExecutionStatus::Completed
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
    expected_status: crate::tracing::TraceLanguageExecutionStatus,
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
                        crate::tracing::TraceRuntimeSubject::Effect { .. }
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
    child_session_id: &SessionId,
    process_id: &ProcessId,
) {
    let graph = run
        .graph_snapshots
        .iter()
        .find(|graph| {
            graph.scope.session_id.as_ref() == Some(child_session_id)
                && graph.scope.turn_id.as_deref() == Some(process_id)
                && matches!(
                    &graph.subject,
                    crate::tracing::TraceRuntimeSubject::Effect { .. }
                )
        })
        .unwrap_or_else(|| {
            panic!(
                "missing scoped session-turn child exec graph: {:#?}",
                GraphContract::from_graphs(&run.graph_snapshots)
            )
        });
    assert_eq!(
        graph.status,
        crate::tracing::TraceLanguageExecutionStatus::Completed
    );
}
