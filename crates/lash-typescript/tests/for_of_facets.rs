//! What the workflow-graph projection reports visible inside a `for...of`
//! body (FIG-3625): the loop binding, typed by its iterable, and only there.

use lash_typescript::workflow_graph::workflow_graph_from_source_with_facets;
use lashlang::testing::harness::test_environment;
use lashlang::{TypeExpr, WorkflowNode};

/// The binding a node writes, by its root name.
fn node_binding_root(node: &WorkflowNode) -> Option<String> {
    serde_json::to_value(&node.kind).ok()?["binding"]["root"]
        .as_str()
        .map(str::to_string)
}

/// The typed variables a node's facets report visible, by name.
fn facet_variables(node: &WorkflowNode) -> Vec<(String, TypeExpr)> {
    node.type_facets
        .as_ref()
        .map(|facets| {
            facets
                .available_variables
                .iter()
                .map(|variable| (variable.name.clone(), variable.ty.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// A `for...of` body sees the bindings before the loop, and the loop binding
/// typed as the iterable's element (FIG-3625: the loop walks its iterable
/// itself, so the element type is the list's, not an opaque copy's); nothing
/// after the loop sees the loop binding. The workflow-graph example's
/// `projected_available_vars_follow_ssa_and_nested_lexical_scope` checks the
/// same through its HTTP projection, in a manual target PR CI does not run.
#[test]
fn a_for_of_body_sees_its_loop_binding_typed_by_the_iterable() {
    let graph = workflow_graph_from_source_with_facets(
        "const scoped = async (record) => {\n  let state = { count: 0 };\n  let first = 1;\n  for (const item of [1]) {\n    let nested = first + item;\n  }\n  return state;\n};\n",
        Some(&test_environment()),
    )
    .expect("the scoped source projects with facets");
    let nested = graph
        .nodes()
        .find(|node| node_binding_root(node).as_deref() == Some("nested"))
        .expect("the loop body binds `nested`");
    let visible = facet_variables(nested);
    let names = visible
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["first", "item", "record", "state"]);
    assert_eq!(
        visible
            .iter()
            .find(|(name, _)| name == "item")
            .map(|(_, ty)| ty),
        Some(&TypeExpr::Int),
        "the loop binding is typed as `[1]`'s element"
    );
    for node in graph.nodes() {
        if node_binding_root(node).as_deref() == Some("nested") {
            continue;
        }
        assert!(
            facet_variables(node).iter().all(|(name, _)| name != "item"),
            "only the loop body sees `item`: {:?}",
            node.name
        );
    }
}

/// A loop binding holding an object is typed as the list's element, and an
/// element the body passes on is open (`dict`): the callee may keep it and
/// give it fields the linker never sees (FIG-3626), so the guard does not
/// trust its literal shape. The workflow-graph example's
/// `mocked_tool_schemas_project_into_seed_workflow_facets` checks the same
/// over a mocked tool schema, in a manual target.
#[test]
fn a_for_of_binding_passed_on_is_an_open_object() {
    let graph = workflow_graph_from_source_with_facets(
        "const rows = [{ id: 1 }];\nfor (const row of rows) {\n  const kept = [row];\n  const seen = row.id;\n}\n",
        Some(&test_environment()),
    )
    .expect("the loop projects with facets");
    let seen = graph
        .nodes()
        .find(|node| node_binding_root(node).as_deref() == Some("seen"))
        .expect("the loop body binds `seen`");
    assert_eq!(
        facet_variables(seen)
            .into_iter()
            .find(|(name, _)| name == "row")
            .map(|(_, ty)| ty),
        Some(TypeExpr::Dict)
    );
}
