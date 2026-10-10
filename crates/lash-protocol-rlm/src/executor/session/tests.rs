//! The laws of a session's stored state: one complete graph per capture,
//! each node sent until a commit installs it, under the dialect that
//! recorded it.

use std::collections::BTreeSet;

use lash_core_store::checkpoint_graph::{NodeId, NodeKind};
use lash_core_store::execution_state::{ExecutionGraphCapture, ExecutionGraphPurpose};
use lash_kernel_doc::NumberPolicy;
use lash_rlm_types::RlmGlobalsPatchPluginBody;

use super::CodeModeExecutionState;

fn bind(name: &str, value: serde_json::Value) -> RlmGlobalsPatchPluginBody {
    RlmGlobalsPatchPluginBody {
        set_default: [(name.to_string(), value)].into_iter().collect(),
    }
}

/// A value whose text is placed in a leaf of its own.
fn large(tag: &str) -> serde_json::Value {
    serde_json::json!([format!("{tag}-{}", "x".repeat(2_000))])
}

/// The root a capture names and the nodes it sends, with the kind of each.
fn sent(capture: &ExecutionGraphCapture) -> (NodeId, Vec<(NodeId, NodeKind)>) {
    let ExecutionGraphCapture::Replace { root, nodes } = capture else {
        panic!("a session with bindings captures a root");
    };
    let nodes = nodes
        .iter()
        .map(|(id, node)| (*id, node.envelope().expect("a staged node").kind))
        .collect();
    (*root, nodes)
}

fn leaves(nodes: &[(NodeId, NodeKind)]) -> BTreeSet<NodeId> {
    nodes
        .iter()
        .filter(|(_, kind)| *kind == NodeKind::TEXT_LEAF)
        .map(|(id, _)| *id)
        .collect()
}

fn capture(state: &mut CodeModeExecutionState) -> (NodeId, Vec<(NodeId, NodeKind)>) {
    sent(
        &state
            .capture_execution_graph(ExecutionGraphPurpose::Final)
            .expect("capture the session's state"),
    )
}

/// Every capture names the complete state, and sends each node until a
/// commit that installs it is acknowledged: a refused commit's nodes are
/// sent again, an unchanged state's staged capture is the same capture, and
/// once installed a large value is never sent again however its neighbours
/// change.
#[tokio::test]
async fn a_capture_sends_each_node_until_a_commit_installs_it() {
    let mut state = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    let none = BTreeSet::new();
    for (name, tag) in [("first", "a"), ("second", "b"), ("third", "c")] {
        state
            .patch_globals(&bind(name, large(tag)), &none)
            .await
            .expect("bind a session variable");
    }
    let (root, nodes) = capture(&mut state);
    let placed = leaves(&nodes);
    assert_eq!(placed.len(), 3, "each large text is a leaf");
    assert!(nodes.iter().any(|(id, _)| *id == root), "the root is sent");

    state.abort_execution_state_capture();
    assert_eq!(
        capture(&mut state),
        (root, nodes.clone()),
        "a refused commit's nodes are sent again"
    );
    assert_eq!(
        capture(&mut state),
        (root, nodes),
        "an unchanged state stages the same capture"
    );
    state.acknowledge_execution_state_capture();
    assert_eq!(capture(&mut state), (root, Vec::new()), "nothing changed");
    state.acknowledge_execution_state_capture();

    state
        .prune_protected_globals(&BTreeSet::from(["second".to_string()]))
        .await
        .expect("unbind one session variable");
    state
        .patch_globals(&bind("second", large("changed")), &none)
        .await
        .expect("rebind one session variable");
    let (changed, nodes) = capture(&mut state);
    assert_ne!(changed, root);
    let new_leaves = leaves(&nodes);
    assert_eq!(new_leaves.len(), 1, "only the rebound value's text is sent");
    assert!(new_leaves.is_disjoint(&placed));
    state.acknowledge_execution_state_capture();

    let saved = state
        .hydrated_execution_state()
        .await
        .expect("the saved session state");
    let mut reloaded = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    reloaded
        .restore_execution_state(&saved)
        .await
        .expect("load the saved bindings");
    assert_eq!(
        reloaded.binding_names().collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );
    assert_eq!(
        reloaded.bound_variable_values(&none),
        state.bound_variable_values(&none)
    );
    assert_eq!(
        capture(&mut reloaded),
        (changed, Vec::new()),
        "a restored state's nodes are the store's"
    );
}

/// A session reads its dialect from its record: bindings recorded by a
/// session of one dialect are refused by a state of another.
#[tokio::test]
async fn bindings_recorded_in_another_dialect_are_refused() {
    let mut state = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    state
        .patch_globals(&bind("answer", serde_json::json!(42)), &BTreeSet::new())
        .await
        .expect("bind a session variable");
    capture(&mut state);
    state.acknowledge_execution_state_capture();
    let saved = state
        .hydrated_execution_state()
        .await
        .expect("the saved session state");

    let error = CodeModeExecutionState::new("python", NumberPolicy::BySpelling)
        .restore_execution_state(&saved)
        .await
        .expect_err("another dialect's bindings are refused");
    assert!(
        matches!(
            &error,
            super::CodeModeSnapshotError::DialectMismatch { expected, found }
                if expected == "python" && found == "typescript"
        ),
        "{error}"
    );
}

/// A saved function's token holds the function itself and constant data
/// about it, and nothing else (FIG-5826): a seed whose token is a bare
/// datum, or names a function in place of the one it saves, is refused.
#[tokio::test]
async fn a_seeded_token_that_is_no_callable_wrapper_is_refused() {
    let none = BTreeSet::new();
    for token in [
        serde_json::json!({ "int": "7" }),
        serde_json::json!({ "tuple": [{ "text": "ts.function" }, { "function": "other" }] }),
    ] {
        let mut function = saved("f", lash_kernel_doc::KERNEL_VERSION);
        function["token"] = token.clone();
        let functions = [("f".to_string(), function)].into_iter().collect();
        let error = CodeModeExecutionState::new("typescript", NumberPolicy::Float)
            .seed_functions(&functions, &none)
            .await
            .expect_err("the token is refused");
        assert!(
            error.to_string().contains("is not a saved function"),
            "{token}: {error}"
        );
    }
}

/// What a session keeps of a function a cell bound is what the cell's
/// dialect declared its function values are, never the dialect's name
/// (FIG-5826): a cell whose dialect declared none keeps no function, and
/// says so of the binding, in a session recorded as TypeScript.
#[test]
fn a_cell_whose_dialect_declares_no_function_values_keeps_no_function() {
    let document = lash_kernel_doc::Document::new(NumberPolicy::Float, Vec::new());
    let name = lash_kernel_doc::Name::new("f");
    let mut bindings = super::SessionBindings::default();
    let changes = bindings.settle(super::CellLeft {
        document: &document,
        annotations: None,
        bindings: lash_kernel_vm::Bindings::default(),
        not_carried: vec![name.clone()],
        closures: lash_kernel_vm::Bindings {
            variables: [(
                name.clone(),
                lash_kernel_doc::Value::Closure(lash_kernel_doc::ObjectId(0)),
            )]
            .into(),
            objects: Default::default(),
        },
        controls: Default::default(),
        function_values: None,
    });
    assert_eq!(changes.not_carried, ["f"]);
    assert_eq!(
        bindings.not_carried().get(&name),
        Some(&lash_kernel_dialect::NotSaved::Dialect)
    );
    assert!(bindings.held_functions().is_empty());
}

/// A saved function whose code is `return 1`, written for kernel version
/// `kernel`.
fn saved(name: &str, kernel: u32) -> serde_json::Value {
    let mut document = lash_kernel_doc::Document::new(NumberPolicy::Float, Vec::new());
    document.manifest.kernel = kernel;
    document.functions.insert(
        lash_kernel_doc::Name::new(name),
        lash_kernel_doc::Function {
            params: Vec::new(),
            body: Vec::new(),
        },
    );
    serde_json::to_value(lash_kernel_dialect::SavedFunction {
        name: lash_kernel_doc::Name::new(name),
        document,
        captures: Default::default(),
        written: None,
        token: None,
    })
    .expect("the saved function encodes")
}

/// Kernel spec §6: a saved function is carried to this build's kernel
/// version when its session is seeded or restored, and one the migration
/// refuses is not held. The session lists its name with the refusal, as it
/// lists a binding that was not saved, and keeps the functions it carries.
#[tokio::test]
async fn a_saved_function_the_migration_refuses_is_listed_and_not_held() {
    let none = BTreeSet::new();
    let unknown = lash_kernel_doc::KERNEL_VERSION + 98;
    let listed = |state: &CodeModeExecutionState| {
        let why = state
            .bindings
            .not_carried()
            .get(&lash_kernel_doc::Name::new("old"))
            .cloned();
        assert!(
            matches!(
                &why,
                Some(lash_kernel_dialect::NotSaved::NotMigrated { from, problem })
                    if *from == unknown && problem.contains("kernel version")
            ),
            "{why:?}"
        );
        let held: Vec<String> = state
            .bindings
            .functions()
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(held, ["kept"]);
    };

    // Seeded: the session is created with both.
    let mut seeded = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    let functions = [
        (
            "kept".to_string(),
            saved("kept", lash_kernel_doc::KERNEL_VERSION),
        ),
        ("old".to_string(), saved("old", unknown)),
    ]
    .into_iter()
    .collect();
    seeded
        .seed_functions(&functions, &none)
        .await
        .expect("a refused function does not refuse the seed");
    listed(&seeded);

    // Restored: a stored state holds both, one of them written in a
    // kernel version this build does not carry.
    let mut stored = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    let kept = [(
        "kept".to_string(),
        saved("kept", lash_kernel_doc::KERNEL_VERSION),
    )]
    .into_iter()
    .collect();
    stored
        .seed_functions(&kept, &none)
        .await
        .expect("seed the stored session");
    let old: lash_kernel_dialect::SavedFunction =
        serde_json::from_value(saved("old", unknown)).expect("a saved function");
    stored.bindings.functions.insert(
        lash_kernel_doc::Name::new("old"),
        super::HeldFunction {
            function: old,
            cell: None,
            aliases: BTreeSet::new(),
        },
    );
    let saved_state = stored
        .hydrated_execution_state()
        .await
        .expect("the saved session state");
    let mut restored = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    restored
        .restore_execution_state(&saved_state)
        .await
        .expect("a refused function does not refuse the restore");
    listed(&restored);
    assert!(
        restored.execution_state_dirty(),
        "what the restore carried and dropped is stored by the next capture"
    );
}

/// A global holding a host resource handle is disclosed as one: listed as
/// an opaque handle of its type, never as a record whose fields a cell
/// might try to read (FIG-1438).
#[test]
fn a_handle_binding_is_disclosed_as_an_opaque_handle() {
    let mut state = CodeModeExecutionState::new("typescript", NumberPolicy::Float);
    state.bindings.variables.insert(
        lash_kernel_doc::Name::from("job"),
        lash_kernel_doc::Value::Handle(std::sync::Arc::new(lash_kernel_doc::Handle {
            kind: "process".to_string(),
            id: "p-7".to_string(),
        })),
    );
    let kinds = state.bound_variable_values(&BTreeSet::new());
    assert_eq!(
        kinds,
        [(
            "job".to_string(),
            crate::session_globals::SessionGlobalKind::Handle {
                kind: "process".to_string(),
                id: "p-7".to_string(),
            }
        )]
    );
    let (text, _) = crate::rlm_support::render_bound_variables(
        &mut Default::default(),
        kinds
            .into_iter()
            .map(|(name, kind)| crate::session_globals::SessionGlobal {
                name,
                cell: None,
                kind,
            })
            .collect(),
        &crate::dialect::TypescriptPrompts,
        &crate::render::BuiltinCodeRenderer,
        &lash_render::RenderParams::preview(),
        crate::RlmPresentationConfig::standard().max_inline_keys,
        usize::MAX,
    );
    assert!(
        text.contains(
            "- `job`: opaque `process` handle `p-7`; pass it whole, it has no fields to read"
        ),
        "{text}"
    );
    assert!(!text.contains("$kind"), "{text}");
}
