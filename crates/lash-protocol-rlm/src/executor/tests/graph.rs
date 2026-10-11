//! The laws of a session's checkpoint graph (FIG-5887 §4, §6): saved
//! functions are self-contained immutable bundles, a working state is
//! adopted whole and a rolled-back cell never promotes what it captured,
//! and states restored from one graph share its immutable nodes but not
//! their heaps.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{NodeId, NodeKind};
use lash_core_store::execution_state::{ExecutionGraphCapture, ExecutionGraphPurpose};

use super::saved_functions::NoTools;
use super::{
    CodeModeExecutionState, SESSION, TURN, finish_of, open_host, typescript_services,
    typescript_state,
};
use crate::testing::{DurableHost, cell_context, run_cell};

async fn cell(
    state: &mut CodeModeExecutionState,
    host: &DurableHost,
    key: &'static str,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, Arc::new(NoTools)),
        &typescript_services(None),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// A fresh session restored from `graph`, which `host` holds.
async fn restored(
    host: &DurableHost,
    graph: &lash_core_store::execution_state::HydratedExecutionGraph,
) -> CodeModeExecutionState {
    host.install(graph).await;
    let mut state = typescript_state();
    state
        .restore_execution_graph(
            graph,
            lash_core_store::execution_state::ExecutionGraphPurpose::Final,
        )
        .expect("the graph restores");
    state
}

/// The nodes a capture sends, by kind.
fn sent(capture: &ExecutionGraphCapture) -> (NodeId, Vec<NodeKind>) {
    let ExecutionGraphCapture::Replace { root, nodes } = capture else {
        panic!("a session with bindings captures a root");
    };
    let kinds = nodes
        .values()
        .map(|node| node.envelope().expect("a staged node").kind)
        .collect();
    (*root, kinds)
}

/// PC-FUNCTION: functions that use one another share one bundle, an alias
/// names the bundle of the function it holds, and rebinding a variable a
/// function captured in a later cell leaves its bundle as it was. Restored
/// from the graph alone, in a session that never ran the cells, every
/// function runs with what it froze.
#[tokio::test(flavor = "multi_thread")]
async fn saved_functions_are_self_contained_bundles_a_later_rebinding_leaves_alone() {
    let host = open_host().await;
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        "let rate = 3;\nfunction isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); }\nfunction isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); }\nconst scale = (x: number) => x * rate;\nconst alias = scale;",
    )
    .await;
    let first = state.hydrated_graph();
    let bundles = CodeModeExecutionState::bundles(&first);
    assert_eq!(
        bundles["isEven"], bundles["isOdd"],
        "mutually recursive functions are members of one bundle"
    );
    assert_eq!(bundles["alias"], bundles["scale"]);
    assert_ne!(bundles["scale"], bundles["isEven"]);

    cell(&mut state, &host, "exec-code:1", "rate = 100;").await;
    let second = state.hydrated_graph();
    assert_ne!(second.root, first.root, "the rebinding is the session's");
    assert_eq!(
        CodeModeExecutionState::bundles(&second),
        bundles,
        "no bundle a later cell did not rebind changes"
    );
    drop(state);

    let mut cold = restored(&host, &second).await;
    let called = cell(
        &mut cold,
        &host,
        "exec-code:2",
        "await control.finish([isEven(4), isOdd(4), scale(2), alias === scale, rate]);",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!([true, false, 6, true, 100])
    );
}

/// PC-WORKING, adoption half: the state a cell left is installed whole into
/// a fresh executor before the next cell, its first capture then sends no
/// node, and a next cell that is cancelled after its state was captured
/// leaves the working state exactly as it was: its staged capture is
/// dropped, never promoted.
#[tokio::test(flavor = "multi_thread")]
async fn a_working_state_is_adopted_whole_and_a_cancelled_cell_never_promotes_its_capture() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        "const kept = [1, 2];\nfunction twice(x: number) { return 2 * x; }",
    )
    .await;
    let working = source.hydrated_graph();
    drop(source);

    let mut state = restored(&host, &working).await;
    let adopted = state
        .capture_execution_graph(ExecutionGraphPurpose::Working)
        .expect("capture");
    assert_eq!(
        adopted,
        ExecutionGraphCapture::Replace {
            root: working.root,
            nodes: Default::default(),
        },
        "the restored state is the working state, every node already installed"
    );
    state.acknowledge_execution_state_capture();

    let speculative = Box::pin(super::super::execute_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", Arc::new(NoTools)),
        lash_core::ExecRequest {
            code: "kept.push(3);\nconst extra = twice(21);".to_owned(),
        },
        &typescript_services(None),
        crate::projection::CodeModeProjectedBindings::default(),
    ))
    .await;
    assert!(speculative.error().is_none(), "{:?}", speculative.error());
    state.mark_code_execution_response_returned();
    let (root, _) = sent(
        &state
            .capture_execution_graph(ExecutionGraphPurpose::Working)
            .expect("capture the speculative state"),
    );
    assert_ne!(root, working.root, "the cell changed the heap");
    state.terminate_code_execution();

    assert_eq!(
        state
            .capture_execution_graph(ExecutionGraphPurpose::Working)
            .expect("capture"),
        ExecutionGraphCapture::Replace {
            root: working.root,
            nodes: Default::default(),
        },
        "the cancelled cell's captured heap is not the session's"
    );
    assert!(
        !state.binding_names().any(|name| name == "extra"),
        "the cancelled cell's binding is gone"
    );
    let read = cell(
        &mut state,
        &host,
        "exec-code:2",
        "await control.finish([kept, twice(4)]);",
    )
    .await;
    assert_eq!(finish_of(&read), serde_json::json!([[1, 2], 8]));
}

/// PC-FORK, mutable independence: two sessions restored from one graph
/// share its immutable nodes, its large text and its function bundles, but
/// each has a heap of its own: what one cell changes, the other never sees.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_restored_from_one_graph_share_its_nodes_but_not_their_heaps() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        &format!(
            "const notes = [\"{}\"];\nfunction label(x: string) {{ return 'n:' + x; }}",
            "x".repeat(1_000)
        ),
    )
    .await;
    let graph = source.hydrated_graph();
    drop(source);

    let mut left = restored(&host, &graph).await;
    let mut right = restored(&host, &graph).await;
    cell(
        &mut left,
        &host,
        "exec-code:1",
        "notes.push(label('more'));",
    )
    .await;
    let seen = cell(
        &mut right,
        &host,
        "exec-code:2",
        "await control.finish(notes.length);",
    )
    .await;
    assert_eq!(
        finish_of(&seen),
        serde_json::json!(1),
        "the right heap is its own"
    );
    let grown = cell(
        &mut left,
        &host,
        "exec-code:3",
        "await control.finish(notes[1]);",
    )
    .await;
    assert_eq!(finish_of(&grown), serde_json::json!("n:more"));

    let (left, right) = (left.hydrated_graph(), right.hydrated_graph());
    assert_ne!(left.root, right.root);
    assert_eq!(
        CodeModeExecutionState::bundles(&left)["label"],
        CodeModeExecutionState::bundles(&right)["label"],
        "the function's bundle is shared"
    );
    let shared: BTreeSet<&NodeId> = left
        .nodes
        .keys()
        .filter(|id| right.nodes.contains_key(id))
        .collect();
    assert!(
        shared.iter().any(|id| left.nodes[*id]
            .envelope()
            .is_ok_and(|envelope| envelope.kind == NodeKind::TEXT_LEAF)),
        "the large text is one leaf both states reach"
    );
}
