//! The workflow lens over TypeScript.
//!
//! TypeScript is the only cell language, so the lens's canonical text is
//! TypeScript and these are the lens laws over it: GetPut (rendering a
//! projected graph reproduces its canonical source), PutGet (reprojecting
//! rendered source reproduces the graph), and the canonical fixpoint. The
//! estate this file replaces was authored in the retired Lashlang surface
//! (FIG-3033); every property it proved is proved here over TypeScript.

use std::collections::BTreeSet;

use lash_typescript::parse;
use lash_typescript::workflow_graph::{
    GraphRenderError, TypeScriptSourceError, WorkflowGraphBuildError,
    parse_typescript_assign_target, parse_typescript_expression, typescript_program_source,
    workflow_graph_from_program, workflow_graph_from_source,
    workflow_graph_from_source_with_facets, workflow_graph_to_source,
};
use lashlang::{
    LashlangAbilities, LashlangExecutionSite, LashlangHostCatalog, LashlangHostEnvironment,
    TypeExpr, TypeField, VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION,
    WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowArgument, WorkflowContainer, WorkflowDeclaration,
    WorkflowDiagnosticClass, WorkflowDiagnosticKind, WorkflowEdge, WorkflowEdgeKind,
    WorkflowExpectedArgument, WorkflowGraph, WorkflowListComprehensionClause, WorkflowNode,
    WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowNodeTypeFacets,
    WorkflowSlotPath, WorkflowSlotPathSegment, WorkflowSubgraph, WorkflowTypeDiagnostic,
    node_id_for_execution_site, workflow_call_to_ir, workflow_slot_value,
};

/// The one process a fixture lifts.
///
/// A process literal's declaration is named by the linker's lift digest, so a
/// fixture pins "the process this module lifted", never a spelled-out name.
fn only_process(graph: &WorkflowGraph) -> &lashlang::WorkflowProcess {
    let mut processes = graph.declarations.iter().filter_map(|declaration| {
        let WorkflowDeclaration::Process(process) = declaration else {
            return None;
        };
        Some(process)
    });
    let process = processes.next().expect("the module lifts one process");
    assert!(
        processes.next().is_none(),
        "this fixture lifts exactly one process"
    );
    process
}

fn canonical(source: &str) -> String {
    typescript_program_source(&parse(source).expect("fixture parses"))
        .expect("a parsed fixture prints back as TypeScript")
}

fn ir(text: &str) -> lashlang::Expr {
    let globals = [
        "state",
        "started",
        "processes",
        "child",
        "tools",
        "value",
        "values",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<BTreeSet<_>>();
    parse_typescript_expression(text, &globals, &BTreeSet::new())
        .expect("fixture expression parses")
}

fn ir_target(text: &str) -> lashlang::AssignTarget {
    let globals = ["state", "started"]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    parse_typescript_assign_target(text, &globals, &BTreeSet::new()).expect("fixture target parses")
}

fn replace_number(expression: &mut lashlang::Expr, replacement: f64) {
    fn replace_first(expression: &mut lashlang::Expr, replacement: f64) -> bool {
        if let lashlang::Expr::Number(value) = expression {
            *value = replacement;
            return true;
        }
        expression
            .children_mut()
            .any(|child| replace_first(child, replacement))
    }
    assert!(
        replace_first(expression, replacement),
        "fixture expression has a numeric descendant"
    );
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

const REPRESENTATIVE: &str = r#"const child = async (input: unknown) => {
    let total = 0;
    for (const value of input.values) {
      await sleep(1);
    }
    const signal = await waitSignal("refresh");
    return total;
  };
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
fn workflow_graph_ir_json_golden_is_exact() {
    let graph =
        workflow_graph_from_source("await tools.lookup({ query: \"x\" });\nawait sleep(\"1s\");\n")
            .expect("fixture projects");
    assert_eq!(graph.schema_version, 12);
    let kinds = serde_json::Value::Array(
        graph
            .main
            .nodes
            .iter()
            .map(|node| serde_json::to_value(&node.kind).expect("node kind serializes"))
            .collect(),
    );
    assert_eq!(
        kinds,
        serde_json::json!([
            {
                "kind": "call",
                "receiver": {
                    "ResourceRef": {
                        "path": ["tools"],
                        "resource_type": "",
                        "alias": ""
                    }
                },
                "operation": "lookup",
                "arguments": [{
                    "kind": "named",
                    "fields": [["query", { "String": "x" }]]
                }],
                "result_steps": ["unwrap_result", "await"]
            },
            {
                "kind": "effect",
                "effect": "sleep_for",
                "arguments": [{
                    "kind": "positional",
                    "value": { "String": "1s" }
                }]
            }
        ])
    );
}

#[test]
fn workflow_type_facet_slot_json_golden_is_exact() {
    let argument = WorkflowExpectedArgument {
        slot: WorkflowSlotPath(vec![
            WorkflowSlotPathSegment::Call(1),
            WorkflowSlotPathSegment::Arg(0),
            WorkflowSlotPathSegment::Field("a.b".into()),
            WorkflowSlotPathSegment::Index(2),
        ]),
        ty: TypeExpr::Str,
    };

    assert_eq!(
        serde_json::to_value(argument).expect("facet argument serializes"),
        serde_json::json!({
            "slot": [
                { "call": 1 },
                { "arg": 0 },
                { "field": "a.b" },
                { "index": 2 }
            ],
            "ty": "Str"
        })
    );
}

#[test]
fn workflow_graph_refuses_unknown_type_expr_variant() {
    let graph = WorkflowGraph {
        schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
        facet_schema_version: None,
        declarations: vec![WorkflowDeclaration::Type(lashlang::TypeDecl {
            name: "Name".into(),
            ty: TypeExpr::Str,
        })],
        main: WorkflowSubgraph::default(),
    };
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    assert_eq!(value["declarations"][0]["ty"], "Str");
    value["declarations"][0]["ty"] = serde_json::json!("FutureType");

    let error = serde_json::from_value::<WorkflowGraph>(value)
        .expect_err("an unknown TypeExpr variant must be refused");
    assert!(error.to_string().contains("unknown variant `FutureType`"));
}

#[test]
fn facet_reader_refuses_unknown_type_expr_variant() {
    let facets = WorkflowNodeTypeFacets {
        available_variables: vec![lashlang::WorkflowTypedVariable {
            name: "value".to_string(),
            ty: TypeExpr::Str,
        }],
        ..WorkflowNodeTypeFacets::default()
    };
    let mut value = serde_json::to_value(facets).expect("facets serialize");
    value["available_variables"][0]["ty"] = serde_json::json!("FutureType");

    let error = serde_json::from_value::<WorkflowNodeTypeFacets>(value)
        .expect_err("an unknown TypeExpr variant must be refused");
    assert!(error.to_string().contains("unknown variant `FutureType`"));
}

#[test]
fn workflow_graph_refuses_unknown_fields_inside_type_expr_payloads() {
    let graph = WorkflowGraph {
        schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
        facet_schema_version: None,
        declarations: vec![WorkflowDeclaration::Type(lashlang::TypeDecl {
            name: "Record".into(),
            ty: TypeExpr::Object(vec![TypeField {
                name: "value".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
        })],
        main: WorkflowSubgraph::default(),
    };
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    value["declarations"][0]["ty"]["Object"][0]["future"] = serde_json::json!(true);

    let error = serde_json::from_value::<WorkflowGraph>(value)
        .expect_err("an unknown TypeField member must be refused inside the graph carrier");
    assert!(error.to_string().contains("unknown field `future`"));
}

#[test]
fn facet_reader_refuses_unknown_fields_inside_type_expr_payloads() {
    let facets = WorkflowNodeTypeFacets {
        available_variables: vec![lashlang::WorkflowTypedVariable {
            name: "value".to_string(),
            ty: TypeExpr::Object(vec![TypeField {
                name: "field".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
        }],
        ..WorkflowNodeTypeFacets::default()
    };
    let mut value = serde_json::to_value(facets).expect("facets serialize");
    value["available_variables"][0]["ty"]["Object"][0]["future"] = serde_json::json!(true);

    let error = serde_json::from_value::<WorkflowNodeTypeFacets>(value)
        .expect_err("an unknown TypeField member must be refused inside the facet carrier");
    assert!(error.to_string().contains("unknown field `future`"));
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
                condition: ir("true"),
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
                condition: ir("true"),
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
                iterable: ir("[]"),
                body: empty(),
            },
            "body",
        ),
        (
            WorkflowContainer::While {
                condition: ir("false"),
                body: empty(),
            },
            "body",
        ),
        (
            WorkflowContainer::ListComprehension {
                binding: None,
                clauses: vec![WorkflowListComprehensionClause::For {
                    binding: "item".to_string(),
                    iterable: ir("[]"),
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
fn edited_expression_ir_is_rendered_and_reprojected() {
    // A process body lifts out of the module as a derived declaration, so the
    // editable statements a host reaches are the module's own: every slot below
    // lives in `main`, and the one process literal stays a value it names.
    let source = r#"const child = async () => {
    return 1;
  };
const state = { count: 0, other: 0 };
while (state.count < 3) {
  state.count = state.count + 1;
}
state.count = 7;
const runs = [await processes.start({ definition: child }), await processes.start({ definition: child })];
finish(1);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");

    let mut edited = graph.clone();
    let WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) =
        &mut edited.main.nodes[2].kind
    else {
        panic!("expected while container")
    };
    *condition = ir("state.count < 2");
    let WorkflowNodeKind::StateUpdate { target, expression } = &mut edited.main.nodes[3].kind
    else {
        panic!("expected state update")
    };
    *target = ir_target("state.other");
    *expression = ir("state.count + 40");
    let WorkflowNodeKind::Computation {
        binding,
        expression,
    } = &mut edited.main.nodes[4].kind
    else {
        panic!("expected computation")
    };
    *binding = Some(ir_target("started"));
    let lashlang::Expr::List(items) = expression else {
        panic!("expected a list computation");
    };
    items.push(items[0].clone());
    let edited_runs = expression.clone();

    let rendered = workflow_graph_to_source(&edited).expect("edited graph renders");
    assert!(
        rendered.contains("while ((state.count < 2))"),
        "rendered source:\n{rendered}"
    );
    assert!(rendered.contains("state.other = (state.count + 40);"));
    assert!(
        rendered.contains("started = [await (processes.start({ definition: child })), await (processes.start({ definition: child })), await (processes.start({ definition: child }))];"),
        "rendered source:\n{rendered}"
    );

    let reprojected = workflow_graph_from_source(&rendered).expect("edited source reprojects");
    assert!(matches!(
        &reprojected.main.nodes[2].kind,
        WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. })
            if condition == &ir("state.count < 2")
    ));
    assert!(matches!(
        &reprojected.main.nodes[3].kind,
        WorkflowNodeKind::StateUpdate { target, expression }
            if target == &ir_target("state.other") && expression == &ir("state.count + 40")
    ));
    assert!(matches!(
        &reprojected.main.nodes[4].kind,
        WorkflowNodeKind::Computation { binding, expression }
            if binding.as_ref() == Some(&ir_target("started"))
                && expression == &edited_runs
    ));
    assert_eq!(
        workflow_graph_to_source(&reprojected).expect("reprojected graph renders"),
        rendered
    );
}

#[test]
fn invalid_host_edited_text_is_refused_before_it_enters_the_graph() {
    let globals = BTreeSet::from(["value".to_string(), "state".to_string()]);
    assert!(parse_typescript_expression("value <", &globals, &BTreeSet::new()).is_err());
    assert!(parse_typescript_assign_target("state.", &globals, &BTreeSet::new()).is_err());
}

#[test]
fn all_container_expression_slots_accept_host_edits() {
    let source = r#"const values = [1, 2];
if (true) {
  await sleep(1);
} else {
  await sleep(2);
}
for (const value of values) {
  await sleep(value);
}
finish(1);
"#;
    let mut graph = workflow_graph_from_source(source).expect("fixture projects");
    let WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. }) =
        &mut graph.main.nodes[1].kind
    else {
        panic!("expected if container")
    };
    *condition = ir("false");
    let WorkflowNodeKind::Container(WorkflowContainer::For { iterable, .. }) =
        &mut graph.main.nodes[2].kind
    else {
        panic!("expected for container")
    };
    *iterable = ir("[3, 4]");

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
    assert!(matches!(
        &reprojected.main.nodes[1].kind,
        WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. })
            if condition == &ir("false")
    ));
    assert!(matches!(
        &reprojected.main.nodes[2].kind,
        WorkflowNodeKind::Container(WorkflowContainer::For { iterable, .. })
            if iterable == &ir("[3, 4]")
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
    replace_number(condition, 2.0);

    let WorkflowNodeKind::Container(WorkflowContainer::For {
        binding, iterable, ..
    }) = &mut graph.main.nodes[1].kind
    else {
        panic!("expected for container")
    };
    *binding = "entry".to_string();
    replace_number(iterable, 3.0);

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
        r#"const scoped = async (record: unknown) => {
    const state = { count: 0 };
    const first = 1;
    for (const item of [1]) {
      const nested = first + item;
    }
    return state;
  };
finish(1);
"#,
    )
    .expect("fixture projects");
    let process = only_process(&graph);
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

fn source_slice<'a>(source: &'a str, node: &WorkflowNode) -> &'a str {
    let span = node
        .source_span
        .expect("a projected node with canonical text carries a source span");
    source
        .get(span.start..span.end)
        .expect("the source span addresses canonical UTF-8 boundaries")
}

#[test]
fn canonical_source_spans_cover_bound_and_inline_process_bodies_without_shape_matching() {
    let authored = r#"const worker=async()=>{await tools.echo({value:"same"});await tools.echo({value:"same"});return "done";};
await triggers.register({source:{expr:"0 8 * * *"},target:async(event)=>{await tools.echo({value:"inline"});return event;}});
"#;
    let canonical = canonical(authored);
    let graph = workflow_graph_from_source(authored).expect("formatted source projects");
    assert_eq!(
        graph,
        workflow_graph_from_source(&canonical).expect("canonical source projects"),
        "formatting-only changes resolve to the same canonical spans"
    );
    let mut processes = graph.declarations.iter().filter_map(|declaration| {
        let WorkflowDeclaration::Process(process) = declaration else {
            return None;
        };
        Some(process)
    });
    let bound = processes.next().expect("the bound process projects");
    let inline = processes.next().expect("the inline process projects");
    assert!(processes.next().is_none(), "exactly two processes project");

    let repeated = &bound.body.nodes[..2];
    assert_eq!(
        repeated
            .iter()
            .map(|node| source_slice(&canonical, node))
            .collect::<Vec<_>>(),
        [
            "await (tools.echo({ value: \"same\" }))",
            "await (tools.echo({ value: \"same\" }))",
        ]
    );
    assert!(
        repeated[0].source_span.expect("first span").start
            < repeated[1].source_span.expect("second span").start,
        "identical expressions retain their distinct canonical positions"
    );
    assert_eq!(
        source_slice(
            &canonical,
            bound.body.nodes.last().expect("bound return node")
        ),
        "return \"done\";"
    );
    assert_eq!(
        source_slice(&canonical, &inline.body.nodes[0]),
        "await (tools.echo({ value: \"inline\" }))"
    );
    assert_eq!(
        source_slice(
            &canonical,
            inline.body.nodes.last().expect("inline return node")
        ),
        "return event;"
    );
}

#[test]
fn cloned_do_while_conditions_keep_provenance_for_every_destination_path() {
    for (source, expected_condition_paths) in [
        ("do { continue; } while (false);", 2),
        ("do { continue; continue; } while (false);", 3),
    ] {
        let program = parse(source).expect("a do-while with continues must lower without panic");
        let condition_paths = program
            .spans
            .iter()
            .filter_map(|(path, span)| {
                (source.get(span.start..span.end) == Some("false")).then_some(path)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            condition_paths.len(),
            expected_condition_paths,
            "every cloned condition keeps the marker's source span: {condition_paths:?}; all spans: {:?}",
            program
                .spans
                .iter()
                .map(|(path, span)| (path, source.get(span.start..span.end), span))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn program_projection_rebuilds_canonical_spans_for_lifted_processes() {
    let authored = "const worker=async()=>{await sleep(1);return 1;};";
    let program = parse(authored).expect("compact process source parses");
    let canonical = typescript_program_source(&program).expect("program prints canonically");
    assert_ne!(authored, canonical, "the fixture must change formatting");

    let graph = workflow_graph_from_program(&program);
    let process = only_process(&graph);
    assert_eq!(
        process
            .body
            .nodes
            .iter()
            .map(|node| source_slice(&canonical, node))
            .collect::<Vec<_>>(),
        ["sleep(1)", "return 1;"]
    );
    assert_eq!(
        graph,
        workflow_graph_from_source(authored).expect("source projection succeeds"),
        "the public program entry point must derive the same canonical provenance as the source entry point"
    );
}

#[test]
fn unprintable_program_projection_exposes_no_source_spans() {
    use lashlang::testing::ast_builders as b;

    let mut program = b::program(vec![b::assign(
        "joined",
        b::builtin("join", vec![b::list(vec![]), b::string(",")]),
    )]);
    program.spans.insert(
        lashlang::AstPath::main(vec![0]),
        lashlang::Span { start: 0, end: 1 },
    );

    let graph = workflow_graph_from_program(&program);
    assert!(
        graph.nodes().all(|node| node.source_span.is_none()),
        "an IR with no canonical TypeScript text must not expose unrelated offsets"
    );
}

#[test]
fn canonical_span_goldens_cover_every_textual_node() {
    let fixtures: Vec<(&str, &str, &[&str])> = vec![
        (
            "named-nested-repeated",
            r#"const worker=async()=>{await tools.echo({value:"same"});await tools.echo({value:"same"});if(true){for(const value of [1]){while(false){await sleep(value);}}}return "done";};"#,
            &[
                r#"const worker = async () => {
  await (tools.echo({ value: "same" }));
  await (tools.echo({ value: "same" }));
  if (true) {
    for (const value of [1]) {
      while (false) {
        await sleep(value);
      }
    }
  }
  return "done";
};"#,
                r#"await (tools.echo({ value: "same" }))"#,
                r#"await (tools.echo({ value: "same" }))"#,
                "if (true) {\n    for (const value of [1]) {\n      while (false) {\n        await sleep(value);\n      }\n    }\n  }",
                "for (const value of [1]) {\n      while (false) {\n        await sleep(value);\n      }\n    }",
                "while (false) {\n        await sleep(value);\n      }",
                "sleep(value)",
                r#"return "done";"#,
            ],
        ),
        (
            "lifted-inline",
            r#"await triggers.register({source:{expr:"0 8 * * *"},target:async(event)=>{await tools.echo({value:"inline"});return event;}});"#,
            &[
                "await (triggers.register({ source: { expr: \"0 8 * * *\" }, target: async (event) => {\n  await (tools.echo({ value: \"inline\" }));\n  return event;\n} }))",
                r#"await (tools.echo({ value: "inline" }))"#,
                "return event;",
            ],
        ),
    ];
    assert!(
        !fixtures.is_empty(),
        "the span golden corpus must not be empty"
    );

    for (name, source, expected) in fixtures {
        assert!(
            !expected.is_empty(),
            "the `{name}` oracle must not be empty"
        );
        let canonical = canonical(source);
        let graph = workflow_graph_from_source(source).expect("span golden projects");
        let actual = graph
            .nodes()
            .map(|node| source_slice(&canonical, node))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "exact canonical slices for `{name}`");
    }
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

fn slot_path_environment() -> LashlangHostEnvironment {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            TypeExpr::Str,
            TypeExpr::Str,
        )
        .expect("echo operation is unique");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "shape_text",
            "shape_text",
            TypeExpr::Object(vec![TypeField {
                name: "text".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
            TypeExpr::Str,
        )
        .expect("shape-text operation is unique");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "compose",
            "compose",
            TypeExpr::Object(vec![
                TypeField {
                    name: "query".into(),
                    ty: TypeExpr::Enum(vec!["ok".into()]),
                    optional: false,
                },
                TypeField {
                    name: "items".into(),
                    ty: TypeExpr::List(Box::new(TypeExpr::Str)),
                    optional: false,
                },
            ]),
            TypeExpr::Str,
        )
        .expect("compose operation is unique");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "address",
            "address",
            TypeExpr::Object(vec![
                TypeField {
                    name: "a.b".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "a".into(),
                    ty: TypeExpr::Object(vec![TypeField {
                        name: "b".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    }]),
                    optional: false,
                },
                TypeField {
                    name: "items[0]".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "items".into(),
                    ty: TypeExpr::List(Box::new(TypeExpr::Str)),
                    optional: false,
                },
                TypeField {
                    name: "\"".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
            ]),
            TypeExpr::Str,
        )
        .expect("address operation is unique");
    catalog
        .add_module_operation(
            ["tools"],
            "Tools",
            "pair_text",
            "pair_text",
            TypeExpr::Object(vec![
                TypeField {
                    name: "first".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "second".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
            ]),
            TypeExpr::Str,
        )
        .expect("pair-text operation is unique");
    LashlangHostEnvironment::new(catalog, LashlangAbilities::all())
}

#[test]
fn facet_slot_paths_are_injective_for_hostile_record_keys() {
    let source = r#"await tools.address({
  "a.b": "literal dot",
  a: { b: "nested" },
  "items[0]": "literal brackets",
  items: ["indexed"],
  "\"": "quote",
  "": "empty"
});
"#;
    let graph = workflow_graph_from_source_with_facets(source, Some(&slot_path_environment()))
        .expect("hostile record keys project with facets");
    let arguments = &graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("call has facets")
        .expected_arguments;
    let slots = arguments
        .iter()
        .map(|argument| argument.slot.clone())
        .collect::<BTreeSet<_>>();

    assert_eq!(arguments.len(), 9, "fixture covers every nested location");
    assert_eq!(
        slots.len(),
        arguments.len(),
        "dots, brackets, quotes, empty keys, and nested records need unique addresses"
    );
    assert!(slots.contains(&WorkflowSlotPath(vec![
        WorkflowSlotPathSegment::Arg(0),
        WorkflowSlotPathSegment::Field("a.b".into()),
    ])));
    assert!(slots.contains(&WorkflowSlotPath(vec![
        WorkflowSlotPathSegment::Arg(0),
        WorkflowSlotPathSegment::Field("a".into()),
        WorkflowSlotPathSegment::Field("b".into()),
    ])));
    assert_eq!(
        serde_json::to_value(WorkflowSlotPath(vec![
            WorkflowSlotPathSegment::Call(1),
            WorkflowSlotPathSegment::Arg(0),
            WorkflowSlotPathSegment::Field("a.b".into()),
            WorkflowSlotPathSegment::Index(2),
        ]))
        .expect("slot path serializes"),
        serde_json::json!([
            { "call": 1 },
            { "arg": 0 },
            { "field": "a.b" },
            { "index": 2 }
        ])
    );

    let WorkflowNodeKind::Call {
        receiver,
        operation,
        arguments: call_arguments,
        result_steps,
        ..
    } = &graph.main.nodes[0].kind
    else {
        panic!("fixture projects as a call node");
    };
    let expression = workflow_call_to_ir(receiver, operation, call_arguments, result_steps);
    let resolved = arguments
        .iter()
        .map(|argument| {
            workflow_slot_value(&expression, &argument.slot)
                .map(|value| std::ptr::from_ref(value).addr())
                .expect("every projected slot resolves")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        resolved.len(),
        arguments.len(),
        "each slot resolves to one distinct IR location"
    );
}

#[test]
fn facet_slots_address_positional_named_nested_record_and_list_arguments() {
    let source = r#"const first = await tools.echo("x");
const second = await tools.compose({
  query: "ok",
  items: ["a"]
});
const nested = await tools.shape_text({ text: await tools.echo("x") });
finish(second);
"#;
    let graph = workflow_graph_from_source_with_facets(source, Some(&slot_path_environment()))
        .expect("fixture projects with facets");

    let first = graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("echo has facets");
    assert!(
        first
            .expected_arguments
            .iter()
            .any(|slot| slot.slot.to_string() == "arg[0]")
    );

    let second = graph.main.nodes[1]
        .type_facets
        .as_ref()
        .expect("compose has facets");
    let slots = second
        .expected_arguments
        .iter()
        .map(|slot| slot.slot.to_string())
        .collect::<BTreeSet<_>>();
    for expected in [
        "arg[0]",
        "arg[0][\"query\"]",
        "arg[0][\"items\"]",
        "arg[0][\"items\"][0]",
    ] {
        assert!(
            slots.contains(expected),
            "missing slot {expected}: {slots:?}"
        );
    }

    let nested = graph.main.nodes[2]
        .type_facets
        .as_ref()
        .expect("nested call has facets");
    let nested_slots = nested
        .expected_arguments
        .iter()
        .map(|slot| slot.slot.to_string())
        .collect::<BTreeSet<_>>();
    for expected in [
        "call[0].arg[0]",
        "call[0].arg[0][\"text\"]",
        "call[1].arg[0]",
    ] {
        assert!(
            nested_slots.contains(expected),
            "missing nested slot {expected}: {nested_slots:?}"
        );
    }
}

#[test]
fn type_diagnostic_carries_slot_kind_and_class() {
    let graph = workflow_graph_from_source_with_facets(
        "await tools.compose({ query: \"bad\", items: [\"a\"] });\n",
        Some(&slot_path_environment()),
    )
    .expect("a definite mismatch remains projectable");
    let diagnostic = graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("call has facets")
        .diagnostics
        .first()
        .expect("mismatch produces a diagnostic");
    assert_eq!(
        diagnostic.kind,
        WorkflowDiagnosticKind::IncompatibleExpectedLiteral
    );
    assert_eq!(diagnostic.class, WorkflowDiagnosticClass::Definite);
    assert_eq!(
        diagnostic.slot.as_ref().map(ToString::to_string).as_deref(),
        Some("arg[0][\"query\"]")
    );
}

#[test]
fn nested_call_diagnostic_identifies_the_inner_failing_argument() {
    let graph = workflow_graph_from_source_with_facets(
        "await tools.shape_text({ text: await tools.echo(42) });\n",
        Some(&slot_path_environment()),
    )
    .expect("a nested mismatch remains projectable");
    let diagnostic = graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("call has facets")
        .diagnostics
        .first()
        .expect("inner mismatch produces a diagnostic");

    assert_eq!(
        diagnostic.slot.as_ref().map(ToString::to_string).as_deref(),
        Some("call[1].arg[0]")
    );
}

#[test]
fn multi_call_diagnostic_identifies_only_the_later_failing_call() {
    let graph = workflow_graph_from_source_with_facets(
        concat!(
            "await tools.pair_text({ ",
            "first: await tools.echo(\"ok\"), ",
            "second: await tools.echo(42) ",
            "});\n"
        ),
        Some(&slot_path_environment()),
    )
    .expect("a later nested mismatch remains projectable");
    let diagnostics = &graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("call has facets")
        .diagnostics;

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0]
            .slot
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("call[2].arg[0]")
    );
}

#[test]
fn catalog_projection_exposes_typed_facets_non_fatally() {
    let source = r#"const workflow = async (name: string) => {
    const query = name;
    const result = await tools.lookup({ query: query });
    for (const item of "not a list") {
      const seen = item;
    }
    return result;
  };
finish(1);
"#;

    let graph = workflow_graph_from_source_with_facets(source, Some(&facet_environment()))
        .expect("a host-backed projection is best effort");
    assert_eq!(
        graph.facet_schema_version,
        Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let process = only_process(&graph);

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
    assert!(call_facets.expected_arguments.iter().any(|argument| {
        argument.slot.to_string() == "arg[0][\"query\"]" && argument.ty == TypeExpr::Str
    }));

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
            kind: WorkflowDiagnosticKind::UnknownName,
            class: WorkflowDiagnosticClass::Definite,
            slot: None,
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
            if expression == &ir("1 + 1")
    ));
    assert_lens_laws("1 + 1;\n");
}

#[test]
fn effectful_composites_are_typed_and_never_opaque() {
    let source = r#"const child = async () => {
    return 1;
  };
const runs = [await processes.start({ definition: child }), await processes.start({ definition: child })];
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
            .map(|site| (site.kind.as_str(), site.label.as_str()))
            .collect::<Vec<_>>(),
        vec![("loop", "while"), ("resource_operation", "ready")]
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
    let source = r#"const triage = async (input: unknown) => {
    if (input.source === "gmail") {
      const message = await gmail.getMessage(input.messageId);
      return message;
    } else {
      return null;
    }
  };
const handle = await processes.start({ definition: triage, args: { input: 1 } });
finish(handle);
"#;
    let graph = workflow_graph_from_source(source).expect("module should project");
    assert_eq!(only_process(&graph).params.len(), 1);
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
const lights = async () => {
  /** @label Go — Turn the green light on */
  await display.set_light({ name: "green", state: "on" });
  return 0;
};
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

    // A lifted process is named by its digest, so the label an author wrote on
    // the binding names the binding's node; the declaration keeps the derived
    // name the linker will lift to.
    let labeled = graph
        .main
        .nodes
        .iter()
        .filter(|node| node.name_source == WorkflowNodeNameSource::Label)
        .map(|node| node.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        labeled,
        vec!["Lookup".to_string(), "Traffic lights".to_string()]
    );
    let process = only_process(&graph);
    assert!(
        process
            .name
            .as_str()
            .starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX)
    );
    assert_eq!(process.name_source, WorkflowNodeNameSource::Derived);
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
/// An inline process body is a process container of the module (FIG-2997):
/// it projects its own declaration, named exactly what the linker lifts the
/// literal to, and the lens laws hold over a call that carries one in
/// argument position.
#[test]
fn an_inline_process_body_projects_as_a_process_container() {
    let source = "await triggers.register({\n  source: { expr: \"0 8 * * *\" },\n  target: async (event) => {\n    print(event);\n  },\n})\nfinish(null);\n";
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");
    let literal = graph
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.name.starts_with("__process_") => {
                Some(process)
            }
            _ => None,
        })
        .expect("the literal projects as a process container");
    assert!(
        literal.body.nodes.iter().any(|node| node.name == "print"),
        "the authored body is the container's subgraph: {literal:?}"
    );
    assert_lens_laws(source);
}

/// FIG-3118: the lens owns a lifted process's body in *both* directions.
///
/// Every top-level `const` arrow is a process literal (FIG-2999), so the lens
/// projects it twice: the statement node in `main` carries the arrow as
/// authored text, and the body is projected again as the lifted process's own
/// subgraph — which is the editable surface. Rendering `main` alone would take
/// the statement's pre-edit text and silently drop everything a host changed
/// inside the container, so `graph_to_program` splices the rendered lifted body
/// back into the literal it was projected from.
#[test]
fn an_edit_inside_a_process_container_survives_the_round_trip() {
    const SOURCE: &str = "const flow = async () => {\n  \
        await (display.show_message({ text: \"before\" }));\n  \
        let total = 1;\n  \
        return total;\n};\n";

    let mut graph = workflow_graph_from_source(SOURCE).expect("the fixture projects");
    let unedited = workflow_graph_to_source(&graph).expect("the projection renders");
    assert_eq!(unedited, SOURCE, "GetPut holds before the edit");

    // Edit one node *inside* the process container, which is the only place
    // the statement text in `main` does not reach.
    let WorkflowDeclaration::Process(process) = &mut graph.declarations[0] else {
        panic!("the fixture lifts one process");
    };
    let WorkflowNodeKind::Call { arguments, .. } = &mut process.body.nodes[0].kind else {
        panic!("the container's first node is the display call");
    };
    let [WorkflowArgument::Named { fields }] = arguments.as_mut_slice() else {
        panic!("the call has one named argument record");
    };
    assert_eq!(fields[0].1, lashlang::Expr::String("before".into()));
    fields[0].1 = lashlang::Expr::String("after".into());

    let saved = workflow_graph_to_source(&graph).expect("the edited graph renders");
    assert_eq!(
        saved,
        SOURCE.replace("\"before\"", "\"after\""),
        "the edit lands and nothing else in the module moves"
    );

    // PutGet: reprojecting the saved source gives back the edited graph, and
    // rendering that is a fixpoint.
    let reprojected = workflow_graph_from_source(&saved).expect("the saved source reprojects");
    let WorkflowDeclaration::Process(reprojected_process) = &reprojected.declarations[0] else {
        panic!("the saved source lifts one process");
    };
    let WorkflowNodeKind::Call { arguments, .. } = &reprojected_process.body.nodes[0].kind else {
        panic!("the reprojected container's first node is the display call");
    };
    let [WorkflowArgument::Named { fields }] = arguments.as_slice() else {
        panic!("the call has one named argument record");
    };
    assert_eq!(fields[0].1, lashlang::Expr::String("after".into()));
    assert_eq!(
        workflow_graph_to_source(&reprojected).expect("the reprojection renders"),
        saved,
        "rendering the reprojection is a fixpoint"
    );
}

#[test]
fn call_argument_ir_edit_renders_without_an_expression_text_field() {
    const SOURCE: &str = "const flow = async () => {\n  \
        await (display.show_message({ text: \"before\" }));\n};\n";

    let mut graph = workflow_graph_from_source(SOURCE).expect("the fixture projects");
    let WorkflowDeclaration::Process(process) = &mut graph.declarations[0] else {
        panic!("the fixture lifts one process");
    };
    let WorkflowNodeKind::Call { arguments, .. } = &mut process.body.nodes[0].kind else {
        panic!("the process body starts with a call");
    };
    let [WorkflowArgument::Named { fields }] = arguments.as_mut_slice() else {
        panic!("the call has one named argument record");
    };
    let (_, lashlang::Expr::String(value)) = &mut fields[0] else {
        panic!("the named text argument is a string");
    };
    *value = "after".into();

    let rendered = workflow_graph_to_source(&graph).expect("edited argument IR renders");
    assert_eq!(rendered, SOURCE.replace("\"before\"", "\"after\""));
}

#[test]
fn effect_argument_ir_edit_renders_without_an_expression_text_field() {
    const SOURCE: &str = "await sleep(\"1s\");\n";
    let mut graph = workflow_graph_from_source(SOURCE).expect("fixture projects");
    let WorkflowNodeKind::Effect { arguments, .. } = &mut graph.main.nodes[0].kind else {
        panic!("sleep projects as an effect");
    };
    let [
        WorkflowArgument::Positional {
            value: lashlang::Expr::String(duration),
        },
    ] = arguments.as_mut_slice()
    else {
        panic!("sleep carries one positional string argument");
    };
    *duration = "2s".into();

    let rendered = workflow_graph_to_source(&graph).expect("edited effect argument IR renders");
    assert_eq!(rendered, "await sleep(\"2s\");\n");
}
