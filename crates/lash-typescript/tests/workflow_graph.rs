//! The workflow lens over TypeScript.
//!
//! TypeScript is the only cell language, so the lens's canonical text is
//! TypeScript and these are the lens laws over it: GetPut (rendering a
//! projected graph reproduces its canonical source), PutGet (reprojecting
//! rendered source reproduces the graph), and the canonical fixpoint. The
//! estate this file replaces was authored in the retired Lashlang surface
//! (FIG-3033); every property it proved is proved here over TypeScript.

use lash_typescript::parse;
use lash_typescript::workflow_graph::{
    GraphRenderError, TypeScriptSourceError, WorkflowGraphBuildError, typescript_program_source,
    workflow_graph_from_source, workflow_graph_from_source_with_facets, workflow_graph_to_source,
};
use lashlang::{
    LashlangAbilities, LashlangExecutionSite, LashlangHostCatalog, LashlangHostEnvironment,
    TypeExpr, TypeField, VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION,
    WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowEdgeKind, WorkflowGraph, WorkflowListComprehensionClause, WorkflowNode, WorkflowNodeId,
    WorkflowNodeKind, WorkflowNodeNameSource, WorkflowSubgraph, WorkflowTypeDiagnostic,
    node_id_for_execution_site,
};

fn canonical(source: &str) -> String {
    typescript_program_source(&parse(source).expect("fixture parses"))
        .expect("a parsed fixture prints back as TypeScript")
}

/// Every lens law over one fixture.
fn assert_lens_laws(source: &str) {
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");
    let rendered = workflow_graph_to_source(&graph).expect("graph renders");
    assert_eq!(rendered, canonical, "GetPut");
    assert_eq!(
        parse(&rendered).expect("rendered source parses"),
        parse(&canonical).expect("canonical source parses"),
    );
    assert_eq!(
        workflow_graph_from_source(&rendered).expect("rendered source reprojects"),
        graph,
        "PutGet",
    );
}

const REPRESENTATIVE: &str = r#"const child = defineProcess({
  name: "child",
  run: async (input: unknown) => {
    let total = 0;
    for (const value of input.values) {
      await sleep(1);
    }
    const signal = await waitSignal("refresh");
    return total;
  }
});
const items = [1, 2, 3].filter((value) => value > 1).map((value) => value * 2);
if (items.length > 0) {
  console.log(items);
} else {
  console.log("empty");
}
finish(items);
"#;

#[test]
fn canonical_get_put_and_put_get() {
    assert_lens_laws(REPRESENTATIVE);
}

#[test]
fn nested_container_graphs_roundtrip_through_the_canonical_json_codec() {
    let source = r#"const items = [1, 2];
if (items.length > 0) {
  for (const item of items) {
    while (false) {
    }
  }
}
finish(items);
"#;
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");

    let json = serde_json::to_string(&graph).expect("workflow graph serializes to JSON text");
    let from_string: WorkflowGraph =
        serde_json::from_str(&json).expect("serialized workflow graph decodes from JSON text");
    assert_eq!(from_string, graph);

    let value = serde_json::to_value(&graph).expect("workflow graph serializes to a JSON value");
    let from_value: WorkflowGraph = serde_json::from_value(value.clone())
        .expect("serialized workflow graph decodes from a JSON value");
    assert_eq!(from_value, graph);

    let conditional = &value["main"]["nodes"][1]["kind"];
    assert_eq!(conditional["kind"], "container");
    assert_eq!(conditional["container_kind"], "if");
    assert_eq!(conditional["else_graph"]["nodes"], serde_json::json!([]));

    let for_loop = &conditional["then_graph"]["nodes"][0]["kind"];
    assert_eq!(for_loop["kind"], "container");
    assert_eq!(for_loop["container_kind"], "for");

    let while_loop = &for_loop["body"]["nodes"][0]["kind"];
    assert_eq!(while_loop["kind"], "container");
    assert_eq!(while_loop["container_kind"], "while");
    assert_eq!(while_loop["body"]["nodes"], serde_json::json!([]));

    let rendered = workflow_graph_to_source(&from_string).expect("decoded graph renders");
    assert_eq!(rendered, canonical);
    assert_eq!(
        workflow_graph_from_source(&rendered).expect("rendered source reprojects"),
        graph
    );

    let mut previous_schema = graph.clone();
    previous_schema.schema_version = WORKFLOW_GRAPH_SCHEMA_VERSION - 1;
    assert!(matches!(
        workflow_graph_to_source(&previous_schema),
        Err(GraphRenderError::UnsupportedSchemaVersion {
            found,
            expected: WORKFLOW_GRAPH_SCHEMA_VERSION,
        }) if found == WORKFLOW_GRAPH_SCHEMA_VERSION - 1
    ));

    let legacy_json = json.replacen("\"container_kind\":\"if\"", "\"kind\":\"if\"", 1);
    let legacy_error = serde_json::from_str::<WorkflowGraph>(&legacy_json)
        .expect_err("the colliding legacy container representation must stay refused");
    assert!(
        legacy_error.to_string().contains("duplicate field `kind`"),
        "unexpected legacy decode error: {legacy_error}"
    );
}

#[test]
fn expression_if_and_direct_else_if_obey_all_lens_laws() {
    let source = r#"const choice = true ? 1 : (false ? 2 : 3);
if (choice === 1) {
  console.log("one");
} else if (choice === 2) {
  console.log("two");
} else {
  console.log("other");
}
finish(choice);
"#;
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");

    let WorkflowNodeKind::Container(WorkflowContainer::If {
        then_is_block,
        else_is_block,
        ..
    }) = &graph.main.nodes[0].kind
    else {
        panic!("expected expression-if container")
    };
    assert!(!then_is_block);
    assert!(!else_is_block);

    let WorkflowNodeKind::Container(WorkflowContainer::If {
        then_is_block,
        else_is_block,
        else_graph,
        ..
    }) = &graph.main.nodes[1].kind
    else {
        panic!("expected statement-if container")
    };
    assert!(*then_is_block);
    assert!(!else_is_block);
    assert!(matches!(
        else_graph.nodes.as_slice(),
        [WorkflowNode {
            kind: WorkflowNodeKind::Container(WorkflowContainer::If {
                then_is_block: true,
                ..
            }),
            ..
        }]
    ));

    assert_lens_laws(source);
}

#[test]
fn canonicalization_discards_comments() {
    let graph = workflow_graph_from_source("// comment\nconst value = 1;\n")
        .expect("a commented module projects");
    let rendered = workflow_graph_to_source(&graph).expect("graph renders");
    assert!(
        !rendered.contains("comment"),
        "rendered source:\n{rendered}"
    );
    assert_eq!(rendered, "let value = 1;\n");
}

#[test]
fn invalid_graphs_are_refused() {
    let mut graph =
        workflow_graph_from_source("const value = 1;\nfinish(value);\n").expect("fixture projects");
    graph.main.edges.push(WorkflowEdge {
        id: "dangling".to_string(),
        from: graph.main.nodes[0].id.clone(),
        to: WorkflowNodeId::new("missing".to_string()),
        kind: WorkflowEdgeKind::Sequence,
    });
    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::UnknownNodeReference { .. })
    ));
}

#[test]
fn missing_and_null_container_children_fail_at_decode() {
    let empty = || Box::new(WorkflowSubgraph::default());
    let cases = [
        (
            WorkflowContainer::If {
                binding: None,
                condition: "true".to_string(),
                then_is_block: true,
                else_is_block: true,
                then_graph: empty(),
                else_graph: empty(),
            },
            "then_graph",
        ),
        (
            WorkflowContainer::If {
                binding: None,
                condition: "true".to_string(),
                then_is_block: true,
                else_is_block: true,
                then_graph: empty(),
                else_graph: empty(),
            },
            "else_graph",
        ),
        (
            WorkflowContainer::For {
                binding: "item".to_string(),
                iterable: "[]".to_string(),
                body: empty(),
            },
            "body",
        ),
        (
            WorkflowContainer::While {
                condition: "false".to_string(),
                body: empty(),
            },
            "body",
        ),
        (
            WorkflowContainer::ListComprehension {
                binding: None,
                clauses: vec![WorkflowListComprehensionClause::For {
                    binding: "item".to_string(),
                    iterable: "[]".to_string(),
                }],
                element: empty(),
            },
            "element",
        ),
    ];

    for (container, required_field) in cases {
        let encoded = serde_json::to_value(container).expect("container serializes");

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("container serializes as an object")
            .remove(required_field);
        let error = serde_json::from_value::<WorkflowContainer>(missing)
            .expect_err("omitting a required child must fail at decode");
        assert!(
            error.to_string().contains(required_field),
            "decode error for `{required_field}` should name the missing field: {error}"
        );

        let mut null = encoded;
        null.as_object_mut()
            .expect("container serializes as an object")
            .insert(required_field.to_string(), serde_json::Value::Null);
        serde_json::from_value::<WorkflowContainer>(null)
            .expect_err("a null required child must fail at decode");
    }
}

#[test]
fn while_and_path_assignment_are_structured_and_typed() {
    let source = r#"const state = { count: 0 };
state.count = 1;
while (state.count < 3) {
  state.count = state.count + 1;
}
finish(state);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    assert!(matches!(
        graph.main.nodes[1].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    let WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) =
        &graph.main.nodes[2].kind
    else {
        panic!("expected while container")
    };
    assert!(matches!(
        body.nodes[0].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    assert_eq!(
        graph.main.nodes[2].outputs,
        vec![VariableVersion {
            variable: "state".to_string(),
            version: 3,
        }]
    );
    assert!(graph.main.edges.iter().any(|edge| {
        edge.from == graph.main.nodes[2].id
            && edge.to == graph.main.nodes[3].id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency {
                    ref variable,
                    version: 3
                } if variable == "state"
            )
    }));
    assert_lens_laws(source);
}

#[test]
fn edited_expression_text_is_rendered_and_reprojected() {
    let source = r#"const child = defineProcess({
  name: "child",
  run: async () => {
    return 1;
  }
});
const workflow = defineProcess({
  name: "workflow",
  run: async () => {
    const state = { count: 0, other: 0 };
    while (state.count < 3) {
      state.count = state.count + 1;
    }
    state.count = 7;
    const runs = [start(child, {}), start(child, {})];
    return state;
  }
});
finish(1);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");

    let mut edited = graph.clone();
    let process = edited
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.name == "workflow" => Some(process),
            _ => None,
        })
        .expect("the workflow process is declared");
    let WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) =
        &mut process.body.nodes[1].kind
    else {
        panic!("expected while container")
    };
    *condition = "(state.count < 2)".to_string();
    let WorkflowNodeKind::StateUpdate { target, expression } = &mut process.body.nodes[2].kind
    else {
        panic!("expected state update")
    };
    *target = "state.other".to_string();
    *expression = "(state.count + 40)".to_string();
    let WorkflowNodeKind::Computation {
        binding,
        expression,
    } = &mut process.body.nodes[3].kind
    else {
        panic!("expected computation")
    };
    *binding = Some("started".to_string());
    *expression = "[start(child, {}), start(child, {}), start(child, {})]".to_string();

    let rendered = workflow_graph_to_source(&edited).expect("edited graph renders");
    assert!(
        rendered.contains("while ((state.count < 2))"),
        "rendered source:\n{rendered}"
    );
    assert!(rendered.contains("state.other = (state.count + 40);"));
    assert!(
        // `start(child, {})` and `start(child)` lower to the same start, and
        // the canonical spelling of an empty input is the shorter one.
        rendered.contains("started = [start(child), start(child), start(child)];"),
        "rendered source:\n{rendered}"
    );

    let reprojected = workflow_graph_from_source(&rendered).expect("edited source reprojects");
    let process = reprojected.process("workflow").expect("workflow process");
    assert!(matches!(
        &process.body.nodes[1].kind,
        WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. })
            if condition == "(state.count < 2)"
    ));
    assert!(matches!(
        &process.body.nodes[2].kind,
        WorkflowNodeKind::StateUpdate { target, expression }
            if target == "state.other" && expression == "(state.count + 40)"
    ));
    assert!(matches!(
        &process.body.nodes[3].kind,
        WorkflowNodeKind::Computation { binding, expression }
            if binding.as_deref() == Some("started")
                && expression == "[start(child), start(child), start(child)]"
    ));
    assert_eq!(
        workflow_graph_to_source(&reprojected).expect("reprojected graph renders"),
        rendered
    );
}

#[test]
fn invalid_edited_expression_returns_field_typed_error() {
    let mut graph = workflow_graph_from_source(
        "const workflow = defineProcess({ name: \"workflow\", run: async () => { while (true) { await sleep(1); } return null; } });\nfinish(1);\n",
    )
    .expect("fixture projects");
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .expect("the process is declared");
    let WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) =
        &mut process.body.nodes[0].kind
    else {
        panic!("expected while container")
    };
    *condition = "value <".to_string();

    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::InvalidExpression {
            field: "condition",
            ..
        })
    ));

    let mut graph = workflow_graph_from_source(
        "const workflow = defineProcess({ name: \"workflow\", run: async () => { const state = { count: 0 }; state.count = 1; return state; } });\nfinish(1);\n",
    )
    .expect("fixture projects");
    let state_update_id = graph
        .process("workflow")
        .expect("workflow process")
        .body
        .nodes[1]
        .id
        .clone();
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .expect("the process is declared");
    let node = process
        .body
        .nodes
        .iter_mut()
        .find(|node| node.id == state_update_id)
        .expect("the state-update node");
    let WorkflowNodeKind::StateUpdate { target, .. } = &mut node.kind else {
        panic!("expected state update")
    };
    *target = "state.".to_string();
    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::InvalidAssignmentTarget {
            field: "target",
            ..
        })
    ));
}

#[test]
fn all_container_expression_slots_accept_host_edits() {
    let source = r#"const workflow = defineProcess({
  name: "workflow",
  run: async () => {
    const values = [1, 2];
    if (true) {
      await sleep(1);
    } else {
      await sleep(2);
    }
    for (const value of values) {
      await sleep(value);
    }
    return values;
  }
});
finish(1);
"#;
    let mut graph = workflow_graph_from_source(source).expect("fixture projects");
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .expect("the process is declared");
    let WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. }) =
        &mut process.body.nodes[1].kind
    else {
        panic!("expected if container")
    };
    *condition = "false".to_string();
    let WorkflowNodeKind::Container(WorkflowContainer::For { iterable, .. }) =
        &mut process.body.nodes[2].kind
    else {
        panic!("expected for container")
    };
    *iterable = "[3, 4]".to_string();

    let rendered = workflow_graph_to_source(&graph).expect("edited graph renders");
    assert!(
        rendered.contains("if (false)"),
        "rendered source:\n{rendered}"
    );
    assert!(
        rendered.contains("for (const value of [3, 4])"),
        "rendered source:\n{rendered}"
    );

    let reprojected = workflow_graph_from_source(&rendered).expect("edited source reprojects");
    let process = reprojected.process("workflow").expect("workflow process");
    assert!(matches!(
        &process.body.nodes[1].kind,
        WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. })
            if condition == "false"
    ));
    assert!(matches!(
        &process.body.nodes[2].kind,
        WorkflowNodeKind::Container(WorkflowContainer::For { iterable, .. })
            if iterable == "[3, 4]"
    ));
    assert_eq!(
        workflow_graph_to_source(&reprojected).expect("reprojected graph renders"),
        rendered
    );
}

#[test]
fn execution_site_correlation_survives_edit_and_reprojection() {
    let source = r#"if (await tools.ready({ attempt: 1 })) {
  console.log("ready");
} else {
  console.log("not ready");
}
for (const value of await tools.values({ batch: 1 })) {
  console.log("loop");
}
finish(1);
"#;
    let mut graph = workflow_graph_from_source(source).expect("fixture projects");
    let original_sites = graph.main.nodes[..2]
        .iter()
        .map(|node| node.execution_sites.clone())
        .collect::<Vec<_>>();
    assert!(original_sites.iter().all(|sites| !sites.is_empty()));

    let WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. }) =
        &mut graph.main.nodes[0].kind
    else {
        panic!("expected if container")
    };
    *condition = "await tools.ready({ attempt: 2 })".to_string();

    let WorkflowNodeKind::Container(WorkflowContainer::For {
        binding, iterable, ..
    }) = &mut graph.main.nodes[1].kind
    else {
        panic!("expected for container")
    };
    *binding = "entry".to_string();
    *iterable = "await tools.values({ batch: 3 })".to_string();

    let rendered = workflow_graph_to_source(&graph).expect("edited graph renders");
    let reprojected = workflow_graph_from_source(&rendered).expect("edited source reprojects");

    for (node, sites) in reprojected.main.nodes[..2].iter().zip(original_sites) {
        assert_eq!(node.execution_sites, sites);
        for workflow_site in sites {
            let runtime_site = LashlangExecutionSite {
                node_id: "runtime-site".to_string(),
                node_kind: workflow_site.kind.clone(),
                label: workflow_site.label.clone(),
                branch: None,
                workflow_site,
            };
            let correlated = node_id_for_execution_site(&reprojected, &runtime_site)
                .expect("edited execution site should resolve after reprojection");
            assert_eq!(correlated, node.id);
            assert!(
                reprojected
                    .nodes()
                    .any(|candidate| candidate.id == correlated)
            );
        }
    }
}

#[test]
fn iteration_carried_reassignment_is_structured_state_update() {
    let source = "let total = 0;\nfor (const value of [1, 2]) {\n  total = total + value;\n}\nfinish(total);\n";
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) =
        &graph.main.nodes[1].kind
    else {
        panic!("expected for container")
    };
    assert!(matches!(
        body.nodes[0].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    assert_eq!(graph.main.nodes[1].outputs[0].version, 2);
    assert!(graph.main.edges.iter().any(|edge| {
        edge.from == graph.main.nodes[1].id
            && edge.to == graph.main.nodes[2].id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { version: 2, .. }
            )
    }));
    assert_lens_laws(source);
}

#[test]
fn loops_publish_path_and_loop_introduced_writes_once() {
    let source = r#"const state = { count: 0 };
let introduced = 0;
for (let item of [1, 2]) {
  state.count = item;
  introduced = item;
  item = item + 1;
}
finish([state, introduced]);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let loop_node = &graph.main.nodes[2];
    assert_eq!(
        loop_node.outputs,
        vec![
            VariableVersion {
                variable: "introduced".to_string(),
                version: 2,
            },
            VariableVersion {
                variable: "state".to_string(),
                version: 2,
            },
        ]
    );
    assert!(
        !loop_node
            .outputs
            .iter()
            .any(|output| output.variable == "item")
    );
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) = &loop_node.kind else {
        panic!("expected for container")
    };
    assert!(matches!(
        body.nodes[0].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    assert_lens_laws(source);
}

#[test]
fn scoped_loop_bindings_do_not_depend_on_or_replace_outer_versions() {
    let source = r#"const item = 99;
const items = [1, 2];
for (const entry of items) {
  console.log(entry);
}
finish(item);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let outer_item = &graph.main.nodes[0];
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) =
        &graph.main.nodes[2].kind
    else {
        panic!("expected for container")
    };
    let body_node = &body.nodes[0];
    assert!(!body.edges.iter().any(|edge| {
        edge.from == outer_item.id
            && edge.to == body_node.id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { ref variable, .. } if variable == "item"
            )
    }));
    assert!(!graph.main.edges.iter().any(|edge| {
        edge.from == outer_item.id
            && edge.to == graph.main.nodes[2].id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { ref variable, .. } if variable == "item"
            )
    }));
    assert!(graph.main.edges.iter().any(|edge| {
        edge.from == outer_item.id
            && edge.to == graph.main.nodes[3].id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { ref variable, version: 1 }
                    if variable == "item"
            )
    }));
    assert_lens_laws(source);
}

/// A loop binding that shadows an outer name has no canonical TypeScript.
///
/// The lowerer resolves the shadow by renaming the inner binding into its own
/// generated namespace, and a generated name is indistinguishable from one of
/// its own temporaries, so the lens cannot spell the loop back. It refuses the
/// program rather than emitting a name nobody wrote. Recovering the authored
/// spelling means the lowerer recording it, which is the same "the lowerer is
/// the one source of truth for what it generated" shape as FIG-3033.b.
#[test]
fn a_shadowed_loop_binding_is_refused_rather_than_spelled_as_generated() {
    let source = r#"const item = 99;
const items = [1, 2];
for (const item of items) {
  console.log(item);
}
finish(item);
"#;
    assert!(matches!(
        workflow_graph_from_source(source),
        Err(WorkflowGraphBuildError::CanonicalSource(
            TypeScriptSourceError::GeneratedBinding { .. }
        ))
    ));
}

#[test]
fn nodes_expose_stable_identifiers_available_before_their_execution() {
    let graph = workflow_graph_from_source(
        r#"const scoped = defineProcess({
  name: "scoped",
  run: async (record: unknown) => {
    const state = { count: 0 };
    const first = 1;
    for (const item of [1]) {
      const nested = first + item;
    }
    return state;
  }
});
finish(1);
"#,
    )
    .expect("fixture projects");
    let process = graph.process("scoped").expect("scoped process");
    assert_eq!(process.body.nodes[0].available_variables, ["record"]);
    assert_eq!(
        process.body.nodes[1].available_variables,
        ["record", "state"]
    );
    assert_eq!(
        process.body.nodes[2].available_variables,
        ["first", "record", "state"]
    );
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) =
        &process.body.nodes[2].kind
    else {
        panic!("expected for container")
    };
    assert_eq!(
        body.nodes[0].available_variables,
        ["first", "item", "record", "state"]
    );
    assert_eq!(
        process.body.nodes[3].available_variables,
        ["first", "nested", "record", "state"]
    );
}

fn facet_environment() -> LashlangHostEnvironment {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "lookup",
            "lookup",
            TypeExpr::Object(vec![TypeField {
                name: "query".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
            TypeExpr::Object(vec![TypeField {
                name: "answer".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
        )
        .expect("host catalog operation must not conflict");
    LashlangHostEnvironment::new(catalog, LashlangAbilities::all())
}

#[test]
fn catalog_projection_exposes_typed_facets_non_fatally() {
    let source = r#"const workflow = defineProcess({
  name: "workflow",
  run: async (name: string) => {
    const query = name;
    const result = await tools.lookup({ query: query });
    for (const item of "not a list") {
      const seen = item;
    }
    return result;
  }
});
finish(1);
"#;

    let graph = workflow_graph_from_source_with_facets(source, Some(&facet_environment()))
        .expect("a host-backed projection is best effort");
    assert_eq!(
        graph.facet_schema_version,
        Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let process = graph.process("workflow").expect("workflow process");

    let call_facets = process.body.nodes[1]
        .type_facets
        .as_ref()
        .expect("the call node has type facets");
    // `query` is `name`, and `name` is a `string` parameter of the run body.
    // The TypeScript front-end erases parameter type annotations before the
    // lowerer builds `ProcessParam`, so the facet analysis has nothing to
    // narrow it with and the variable is exposed untyped. Typing it means the
    // adapter carrying annotations through to `ProcessParam::ty`, which is a
    // front-end feature rather than part of the lens.
    assert!(
        call_facets
            .available_variables
            .iter()
            .any(|variable| variable.name == "query")
    );
    assert!(
        call_facets
            .expected_arguments
            .iter()
            .any(|argument| { argument.slot == "arg[0].query" && argument.ty == TypeExpr::Str })
    );

    let loop_facets = process.body.nodes[2]
        .type_facets
        .as_ref()
        .expect("the loop node has type facets");
    assert!(loop_facets.available_variables.iter().any(|variable| {
        variable.name == "result"
            && variable.ty
                == TypeExpr::Object(vec![TypeField {
                    name: "answer".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }])
    }));
}

#[test]
fn type_facets_are_ignored_by_put_and_canonicalization() {
    let source = "const value = \"text\";\nfinish(value);\n";
    let canonical = canonical(source);
    let environment =
        LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all());
    let mut graph = workflow_graph_from_source_with_facets(source, Some(&environment))
        .expect("a host-backed projection is best effort");
    graph.facet_schema_version = Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION + 99);
    let terminal_id = graph.main.nodes[1].id.clone();
    graph.main.nodes[1]
        .type_facets
        .as_mut()
        .expect("the terminal node has type facets")
        .diagnostics
        .push(WorkflowTypeDiagnostic {
            node_id: terminal_id,
            kind: "client_echo".to_string(),
            message: "must not become source".to_string(),
            span: None,
        });

    assert_eq!(
        workflow_graph_to_source(&graph).expect("graph renders"),
        canonical
    );
    let reprojected = workflow_graph_from_source(&canonical).expect("canonical source reprojects");
    assert_eq!(reprojected.facet_schema_version, None);
    assert!(reprojected.nodes().all(|node| node.type_facets.is_none()));
}

#[test]
fn standalone_pure_expressions_remain_computations() {
    let graph = workflow_graph_from_source("1 + 1;\n").expect("fixture projects");
    assert!(matches!(
        graph.main.nodes[0].kind,
        WorkflowNodeKind::Computation { ref expression, binding: None }
            if expression == "(1 + 1)"
    ));
    assert_lens_laws("1 + 1;\n");
}

#[test]
fn effectful_composites_are_typed_and_never_opaque() {
    let source = r#"const child = defineProcess({
  name: "child",
  run: async () => {
    return 1;
  }
});
const runs = [start(child, {}), start(child, {})];
const tupled = [await runs[0], await runs[1]];
const recorded = { value: await runs[0] };
const binary = (await runs[0]) + 1;
const unary = !(await runs[0]);
const field = (await runs[0]).value;
const indexed = (await runs)[0];
finish(indexed);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    assert!(
        graph.main.nodes[2..=7]
            .iter()
            .all(|node| matches!(node.kind, WorkflowNodeKind::Computation { .. })),
        "unexpected kinds: {:?}",
        graph.main.nodes[2..=7]
            .iter()
            .map(|node| &node.kind)
            .collect::<Vec<_>>()
    );
    assert!(
        !graph
            .nodes()
            .any(|node| matches!(node.kind, WorkflowNodeKind::Opaque { .. }))
    );
    assert_lens_laws(source);
}

#[test]
fn while_collects_condition_sites_without_duplicating_body_sites() {
    let source = r#"while (await tools.ready({})) {
  await tools.tick({});
}
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let node = &graph.main.nodes[0];
    let WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) = &node.kind else {
        panic!("expected while container")
    };
    assert_eq!(
        node.execution_sites
            .iter()
            .map(|site| site.label.as_str())
            .collect::<Vec<_>>(),
        vec!["ready"]
    );
    assert_eq!(body.nodes[0].execution_sites[0].label, "tick");
    assert_lens_laws(source);
}

#[test]
fn structured_loop_control_round_trips() {
    let source = "for (const value of [1, 2]) {\n  if (value === 1) {\n    continue;\n  } else {\n    break;\n  }\n}\n";
    assert_lens_laws(source);
}

#[test]
fn function_declarations_survive_the_graph_round_trip() {
    // A pure body contributes no steps, so a function is carried verbatim
    // rather than projected — projecting it would invent workflow structure
    // that never executes as its own node.
    assert_lens_laws(
        "function describe(name: string, count: number) {\n  return name + count;\n}\n\nconsole.log(describe(\"items\", 2));\nfinish(null);\n",
    );
}

#[test]
fn projection_covers_calls_containers_and_terminals() {
    // Moved from the Lashlang parser suite, which asserted graph projection
    // rather than parsing (FIG-3033).
    let source = r#"const triage = defineProcess({
  name: "triage",
  run: async (input: unknown) => {
    if (input.source === "gmail") {
      const message = await gmail.getMessage(input.messageId);
      return message;
    } else {
      return null;
    }
  }
});
const handle = start(triage, { input: 1 });
finish(handle);
"#;
    let graph = workflow_graph_from_source(source).expect("module should project");
    assert!(graph.process("triage").is_some());
    assert!(
        graph
            .nodes()
            .any(|node| matches!(node.kind, WorkflowNodeKind::Call { .. }))
    );
    assert!(graph.nodes().any(|node| matches!(
        node.kind,
        WorkflowNodeKind::Container(WorkflowContainer::If { .. })
    )));
    assert!(
        graph
            .nodes()
            .any(|node| matches!(node.kind, WorkflowNodeKind::Terminal { .. }))
    );
    assert!(
        graph
            .main
            .edges
            .iter()
            .any(|edge| matches!(edge.kind, WorkflowEdgeKind::Sequence))
    );
}

const LABELED: &str = r#"/** @label Lookup — Read the app's current state */
const value = await tools.app_lookup({});
/** @label Traffic lights */
const lights = defineProcess({
  name: "lights",
  run: async () => {
    /** @label Go — Turn the green light on */
    await display.set_light({ name: "green", state: "on" });
    return 0;
  },
});
"#;

/// A label is authored as a doc comment, and the whole point of the spelling
/// is that it survives the round trip a rename depends on: the projection
/// reads it, the renderer writes it back, and a reparse finds the same label
/// on the same node.
#[test]
fn label_doc_comments_name_nodes_through_every_lens_law() {
    assert_lens_laws(LABELED);
    let graph = workflow_graph_from_source(&canonical(LABELED)).expect("labeled source projects");
    let node = graph
        .main
        .nodes
        .iter()
        .find(|node| node.name_source == WorkflowNodeNameSource::Label)
        .expect("a labeled node in the module body");
    assert_eq!(node.name.as_str(), "Lookup");
    assert_eq!(
        node.description.as_deref(),
        Some("Read the app's current state")
    );

    let WorkflowDeclaration::Process(process) = graph
        .declarations
        .iter()
        .find(|declaration| matches!(declaration, WorkflowDeclaration::Process(_)))
        .expect("the declared process")
    else {
        unreachable!("filtered to processes")
    };
    assert_eq!(process.display_name.as_str(), "Traffic lights");
    assert_eq!(process.name_source, WorkflowNodeNameSource::Label);
    assert_eq!(
        process
            .body
            .nodes
            .iter()
            .filter(|node| node.name_source == WorkflowNodeNameSource::Label)
            .map(|node| node.name.to_string())
            .collect::<Vec<_>>(),
        vec!["Go".to_string()],
    );
}

/// The label is a name, not an instruction: it must not change what the
/// program compiles to.
#[test]
fn a_label_comment_changes_no_lowered_program() {
    let labeled = parse(LABELED).expect("labeled source parses");
    let bare = parse(
        &LABELED
            .lines()
            .filter(|line| !line.trim_start().starts_with("/**"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("unlabeled source parses");
    // Source spans move, because the comment occupies bytes and the label
    // node occupies an AST path. What the program *does* — its declarations
    // and its body — does not.
    for part in ["declarations", "main"] {
        assert_eq!(
            strip_labels(
                serde_json::to_value(&labeled).expect("labeled program serializes")[part].clone()
            ),
            strip_labels(
                serde_json::to_value(&bare).expect("bare program serializes")[part].clone()
            ),
            "the label is the only difference a label comment makes to `{part}`",
        );
    }
}

/// Drops every label the AST carries — the `LabelAnnotated` wrapper and a
/// process declaration's own title — so two programs can be compared on what
/// they *do*.
fn strip_labels(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(mut object) => {
            if let Some(annotated) = object.remove("LabelAnnotated") {
                let mut annotated = match annotated {
                    serde_json::Value::Object(annotated) => annotated,
                    other => return strip_labels(other),
                };
                return strip_labels(annotated.remove("expr").expect("annotated expression"));
            }
            object.remove("label");
            serde_json::Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| (key, strip_labels(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(strip_labels).collect())
        }
        other => other,
    }
}

/// Two names for one node is the author contradicting themselves, and no rule
/// for picking between them would be the one they meant.
#[test]
fn a_second_label_on_one_statement_is_refused() {
    let error = parse(
        "/** @label First */\n/** @label Second */\nconst value = await tools.app_lookup({});\n",
    )
    .expect_err("two labels on one statement");
    assert_eq!(error.code.as_str(), "TS_DUPLICATE_NODE_LABEL");
}

/// Everything that is not exactly the one-line form is ordinary trivia.
#[test]
fn comments_that_are_not_the_label_form_stay_trivia() {
    for source in [
        "// @label Line comment\nconst value = 1;\n",
        "/* @label Not a doc comment */\nconst value = 1;\n",
        "/**\n * @label Multi line\n */\nconst value = 1;\n",
        "/** Notes @label Not first */\nconst value = 1;\n",
        "/** @label */\nconst value = 1;\n",
    ] {
        let graph = workflow_graph_from_source(source).expect("a commented module projects");
        assert!(
            graph
                .main
                .nodes
                .iter()
                .all(|node| node.name_source == WorkflowNodeNameSource::Derived),
            "source named a node:\n{source}"
        );
        let rendered = workflow_graph_to_source(&graph).expect("graph renders");
        assert!(!rendered.contains("@label"), "rendered source:\n{rendered}");
    }
}

/// A title the renderer cannot write back is refused rather than mangled: a
/// rename that silently became a different name is worse than a typed error.
#[test]
fn a_label_with_no_spelling_is_refused_by_the_renderer() {
    let mut graph = workflow_graph_from_source(&canonical(LABELED)).expect("labeled source");
    let node = graph
        .main
        .nodes
        .iter_mut()
        .find(|node| node.name_source == WorkflowNodeNameSource::Label)
        .expect("a labeled node");
    node.name = "Close */ me".into();
    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::CanonicalSource(
            TypeScriptSourceError::UnrepresentableLabel { .. }
        ))
    ));
}
