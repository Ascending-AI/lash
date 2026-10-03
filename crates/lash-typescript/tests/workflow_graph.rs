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
    GraphRenderError, parse_typescript_assign_target, parse_typescript_expression,
    typescript_expression_source, typescript_program_source, validate, workflow_graph_from_source,
    workflow_graph_from_source_with_facets, workflow_graph_to_source,
};
use lashlang::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, TypeExpr, TypeField,
    VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
    WorkflowArgument, WorkflowContainer, WorkflowDeclaration, WorkflowDiagnosticKind, WorkflowEdge,
    WorkflowEdgeKind, WorkflowGraph, WorkflowGraphDecodeError, WorkflowGraphReconcileSide,
    WorkflowNode, WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowSlotPath,
    WorkflowSlotPathSegment, WorkflowSubgraph, reconcile, workflow_call_to_ir, workflow_slot_value,
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

#[test]
fn canonical_get_put_and_put_get() {
    assert_lens_laws(goldens::REPRESENTATIVE);
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
        Err(GraphRenderError::UnsupportedSchemaVersion(refusal))
            if refusal.found == WORKFLOW_GRAPH_SCHEMA_VERSION - 1
                && refusal.reads.supported().min() == WORKFLOW_GRAPH_SCHEMA_VERSION
                && refusal.reads.supported().max() == WORKFLOW_GRAPH_SCHEMA_VERSION
                && refusal.reads.recorded() == WORKFLOW_GRAPH_SCHEMA_VERSION
    ));

    let legacy_json = json.replacen("\"container_kind\":\"if\"", "\"kind\":\"if\"", 1);
    let legacy_error = WorkflowGraph::decode_json(&legacy_json)
        .expect_err("the colliding legacy container representation must stay refused");
    assert!(matches!(
        legacy_error,
        WorkflowGraphDecodeError::Document(_)
    ));
    assert!(legacy_error.to_string().contains("unknown variant `if`"));
}

#[test]
fn workflow_graph_decode_checks_version_before_shape() {
    let graph = workflow_graph_from_source("finish(1);\n").expect("fixture projects");
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    value["schema_version"] = serde_json::json!(WORKFLOW_GRAPH_SCHEMA_VERSION - 1);
    value["main"]["nodes"][0]["kind"] = serde_json::json!({ "kind": "future_node" });

    let encoded = serde_json::to_string(&value).expect("fixture JSON encodes");
    assert!(matches!(
        WorkflowGraph::decode_json(&encoded),
        Err(WorkflowGraphDecodeError::UnsupportedSchemaVersion(refusal))
            if refusal.found == WORKFLOW_GRAPH_SCHEMA_VERSION - 1
                && refusal.reads.supported().min() == WORKFLOW_GRAPH_SCHEMA_VERSION
                && refusal.reads.supported().max() == WORKFLOW_GRAPH_SCHEMA_VERSION
                && refusal.reads.recorded() == WORKFLOW_GRAPH_SCHEMA_VERSION
    ));
}

#[test]
fn workflow_graph_refuses_unknown_variant() {
    let graph = workflow_graph_from_source("finish(1);\n").expect("fixture projects");
    let mut unknown_variant = serde_json::to_value(graph).expect("graph serializes");
    unknown_variant["main"]["nodes"][0]["kind"] = serde_json::json!({ "kind": "future_node" });
    let error = WorkflowGraph::decode_json(
        &serde_json::to_string(&unknown_variant).expect("fixture JSON encodes"),
    )
    .expect_err("same-version unknown variants are refused");
    assert!(error.to_string().contains("unknown variant `future_node`"));
}

fn populated_facet_graph() -> WorkflowGraph {
    let graph = workflow_graph_from_source_with_facets(
        "await tools.lookup({ query: \"x\" });\nfinish(1);\n",
        Some(&facet_environment()),
    )
    .expect("fixture projects with facets");
    assert_eq!(
        graph.facet_schema_version,
        Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    assert!(graph.nodes().any(|node| {
        node.type_facets.as_ref().is_some_and(|facets| {
            !facets.available_variables.is_empty()
                || !facets.expected_arguments.is_empty()
                || !facets.diagnostics.is_empty()
        })
    }));
    graph
}

#[test]
fn facet_reader_requires_exact_version() {
    let graph = populated_facet_graph();
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    assert_eq!(
        value["facet_schema_version"],
        serde_json::json!(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    assert!(
        value["main"]["nodes"][0]["type_facets"]
            .as_object()
            .is_some_and(|facets| !facets.is_empty())
    );
    value["facet_schema_version"] = serde_json::json!(WORKFLOW_TYPE_FACET_SCHEMA_VERSION - 1);
    value["main"]["nodes"][0]["type_facets"]["diagnostics"] = serde_json::json!([{
        "kind": "future_diagnostic"
    }]);

    let decoded = WorkflowGraph::decode_json_value(value)
        .expect("stale optional facets are discarded before their shape is decoded");
    assert_eq!(decoded.facet_schema_version, None);
    assert!(decoded.nodes().all(|node| node.type_facets.is_none()));
}

#[test]
fn facet_reader_tolerates_unknown_field() {
    let graph = populated_facet_graph();
    let expected = graph.main.nodes[0]
        .type_facets
        .clone()
        .expect("fixture has facets");
    let mut value = serde_json::to_value(&graph).expect("graph serializes");
    value["main"]["nodes"][0]["type_facets"]["future"] = serde_json::json!(true);

    let decoded = WorkflowGraph::decode_json_value(value)
        .expect("known facet objects tolerate additive unknown fields");
    assert_eq!(
        decoded.facet_schema_version,
        Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    assert_eq!(decoded.main.nodes[0].type_facets, Some(expected));
}

#[test]
fn facet_reader_refuses_unknown_variant() {
    let graph = populated_facet_graph();
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    value["main"]["nodes"][0]["type_facets"]["diagnostics"] = serde_json::json!([{
        "node_id": value["main"]["nodes"][0]["id"].clone(),
        "kind": "future_diagnostic",
        "slot": null,
        "message": "fixture",
        "span": null
    }]);

    let error = WorkflowGraph::decode_json_value(value)
        .expect_err("unknown facet enum variants must be refused at the current version");
    assert!(
        error
            .to_string()
            .contains("unknown variant `future_diagnostic`")
    );
}

#[test]
fn workflow_graph_decode_refuses_unknown_fields_in_nested_non_facet_payloads() {
    let golden = include_str!("fixtures/workflow_graph_with_facets.json");
    WorkflowGraph::decode_json(golden).expect("the untouched graph golden decodes");

    let golden_value =
        serde_json::from_str::<serde_json::Value>(golden).expect("the graph golden is JSON");
    let mut cases = Vec::new();

    let mut source_span = golden_value.clone();
    source_span["main"]["nodes"][0]["source_span"]["future"] = serde_json::json!(true);
    cases.push(("source_span", "future", source_span));

    let mut source_span_facet_collision = golden_value.clone();
    source_span_facet_collision["main"]["nodes"][0]["source_span"]["type_facets"] =
        serde_json::json!({});
    cases.push((
        "source_span type_facets collision",
        "type_facets",
        source_span_facet_collision,
    ));

    let mut root_facet_collision = golden_value.clone();
    root_facet_collision["type_facets"] = serde_json::json!({});
    cases.push((
        "root type_facets collision",
        "type_facets",
        root_facet_collision,
    ));

    let mut execution_site = golden_value.clone();
    execution_site["main"]["nodes"][0]["execution_sites"][0]["future"] = serde_json::json!(true);
    cases.push(("execution site", "future", execution_site));

    let mut ast_payload = golden_value.clone();
    ast_payload["main"]["nodes"][0]["kind"]["binding"]["future"] = serde_json::json!(true);
    cases.push(("AssignTarget", "future", ast_payload));

    let function_graph = WorkflowGraph {
        schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
        source_identity: Some("fixture".to_string()),
        facet_schema_version: None,
        declarations: vec![WorkflowDeclaration::Function(lashlang::FunctionDecl {
            name: "describe".into(),
            params: vec![lashlang::FunctionParam {
                name: "name".into(),
                ty: TypeExpr::Str,
            }],
            return_ty: TypeExpr::Str,
            body: lashlang::Expr::Variable("name".into()),
        })],
        main: WorkflowSubgraph::default(),
    };
    let mut function_decl =
        serde_json::to_value(function_graph).expect("the function graph serializes");
    function_decl["declarations"][0]["future"] = serde_json::json!(true);
    cases.push(("FunctionDecl", "future", function_decl));

    for (name, field, value) in cases {
        let error = match WorkflowGraph::decode_json(
            &serde_json::to_string(&value).expect("the mutated graph encodes"),
        ) {
            Ok(_) => panic!("{name} accepted an unknown nested field"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "{name} mutation returned the wrong error: {error}"
        );
    }

    let mut extended_facet = golden_value;
    extended_facet["main"]["nodes"][0]["type_facets"]["future"] = serde_json::json!(true);
    WorkflowGraph::decode_json(
        &serde_json::to_string(&extended_facet).expect("the facet extension encodes"),
    )
    .expect("known facet objects remain tolerant of unknown fields");
}

#[test]
fn source_identity_is_the_admitted_artifacts_and_ignores_input_formatting() {
    let environment = lashlang::testing::harness::labeled_test_environment();
    let admitted = |source: &str| {
        workflow_graph_from_source_with_facets(source, Some(&environment))
            .expect("source projects")
            .source_identity
            .expect("an admitted source names its artifact identity")
    };
    let compact = admitted("const value=1;finish(value);\n");
    let formatted = admitted("const value = 1;\n\nfinish(value);\n");
    assert_eq!(compact, formatted, "spans are not identity");

    let linked = lash_typescript::link("const value = 1;\nfinish(value);\n", &environment)
        .expect("fixture links");
    assert_eq!(
        formatted,
        linked.artifact.source_identity(),
        "the draft names the identity of the artifact it admits to"
    );
    assert_eq!(
        lash_typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact)
            .source_identity,
        Some(formatted.clone()),
        "the runnable view and the trace name the same identity"
    );

    let labeled = admitted("/** @label Finish value */\nconst value = 1;\nfinish(value);\n");
    assert_ne!(formatted, labeled);
}

#[test]
fn a_draft_claims_no_runtime_identity_and_identity_never_depends_on_printing() {
    let draft = workflow_graph_from_source("const value = 1;\n").expect("draft projects");
    assert_eq!(draft.source_identity, None);

    let artifact = lashlang::ModuleArtifact::from_program(lashlang::Program::block(vec![
        lashlang::Expr::Call {
            function: Box::new(lashlang::Expr::Block(vec![lashlang::Expr::Absent])),
            args: vec![],
        },
    ]))
    .expect("a non-sourceable program still forms an artifact");
    assert!(typescript_program_source(artifact.ir()).is_err());
    let graph = lash_typescript::workflow_graph::workflow_graph_from_artifact(&artifact);
    assert_eq!(graph.source_identity, Some(artifact.source_identity()));
    assert!(graph.nodes().all(|node| node.source_span.is_none()));
}

#[test]
fn reconcile_pairs_inserted_nodes_by_structural_location() {
    let mut submitted = workflow_graph_from_source("finish(1);\n").expect("fixture projects");
    let inserted_id = WorkflowNodeId::new("new:inserted".to_string());
    submitted.main.nodes.insert(
        0,
        WorkflowNode {
            id: inserted_id.clone(),
            name: "computation".to_string(),
            description: None,
            name_source: WorkflowNodeNameSource::Derived,
            kind: WorkflowNodeKind::Computation {
                binding: None,
                expression: lashlang::Expr::Number(2.0),
            },
            available_variables: Vec::new(),
            type_facets: None,
            outputs: Vec::new(),
            execution_sites: Vec::new(),
            source_span: None,
        },
    );
    let source = workflow_graph_to_source(&submitted).expect("submitted graph renders");
    let reprojected = workflow_graph_from_source(&source).expect("source reprojects");

    let result = reconcile(&submitted, &reprojected);
    assert!(result.unmatched.is_empty());
    assert!(result.ambiguous.is_empty());
    assert_eq!(result.pairs.len(), 2);
    assert!(
        result
            .pairs
            .iter()
            .any(|pair| pair.submitted == inserted_id)
    );
}

#[test]
fn reconcile_reports_unmatched_and_ambiguous_ids_without_guessing() {
    let submitted = workflow_graph_from_source("1;\nfinish(1);\n").expect("fixture projects");
    let mut missing = submitted.clone();
    missing.main.nodes.remove(0);
    let unmatched = reconcile(&submitted, &missing);
    assert_eq!(unmatched.unmatched.len(), 1);
    assert!(unmatched.ambiguous.is_empty());

    let mut duplicate_ids = submitted.clone();
    duplicate_ids.main.nodes[1].id = duplicate_ids.main.nodes[0].id.clone();
    let ambiguous = reconcile(&duplicate_ids, &duplicate_ids);
    assert!(ambiguous.unmatched.is_empty());
    assert_eq!(ambiguous.ambiguous.len(), 2);
    assert!(ambiguous.pairs.is_empty());
}

#[test]
fn reconcile_suppresses_pairs_for_ids_duplicated_at_unmatched_locations() {
    let two_nodes = workflow_graph_from_source("1;\nfinish(1);\n").expect("fixture projects");
    let one_node = workflow_graph_from_source("finish(1);\n").expect("fixture projects");

    let mut duplicate_submitted = two_nodes.clone();
    duplicate_submitted.main.nodes[1].id = duplicate_submitted.main.nodes[0].id.clone();
    let submitted_result = reconcile(&duplicate_submitted, &one_node);
    assert!(submitted_result.pairs.is_empty());
    assert_eq!(submitted_result.unmatched.len(), 1);
    assert_eq!(submitted_result.ambiguous.len(), 1);
    assert_eq!(
        submitted_result.ambiguous[0].side,
        WorkflowGraphReconcileSide::Submitted
    );
    assert_eq!(submitted_result.ambiguous[0].locations.len(), 2);
    assert!(
        submitted_result.ambiguous[0]
            .locations
            .contains(&submitted_result.unmatched[0].location)
    );

    let mut duplicate_reprojected = two_nodes;
    duplicate_reprojected.main.nodes[1].id = duplicate_reprojected.main.nodes[0].id.clone();
    let reprojected_result = reconcile(&one_node, &duplicate_reprojected);
    assert!(reprojected_result.pairs.is_empty());
    assert_eq!(reprojected_result.unmatched.len(), 1);
    assert_eq!(reprojected_result.ambiguous.len(), 1);
    assert_eq!(
        reprojected_result.ambiguous[0].side,
        WorkflowGraphReconcileSide::Reprojected
    );
    assert_eq!(reprojected_result.ambiguous[0].locations.len(), 2);
    assert!(
        reprojected_result.ambiguous[0]
            .locations
            .contains(&reprojected_result.unmatched[0].location)
    );
}

#[test]
fn workflow_graph_ir_json_golden_is_exact() {
    let graph = workflow_graph_from_source(goldens::IR_JSON).expect("fixture projects");
    assert_eq!(graph.schema_version, 21);
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
fn workflow_graph_refuses_unknown_type_expr_variant() {
    let graph = WorkflowGraph {
        schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
        source_identity: Some("fixture".to_string()),
        facet_schema_version: None,
        declarations: vec![WorkflowDeclaration::Function(lashlang::FunctionDecl {
            name: "name".into(),
            params: vec![],
            return_ty: TypeExpr::Str,
            body: lashlang::Expr::String("result".into()),
        })],
        main: WorkflowSubgraph::default(),
    };
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    assert_eq!(value["declarations"][0]["return_ty"], "Str");
    value["declarations"][0]["return_ty"] = serde_json::json!("FutureType");

    let error = serde_json::from_value::<WorkflowGraph>(value)
        .expect_err("an unknown TypeExpr variant must be refused");
    assert!(error.to_string().contains("unknown variant `FutureType`"));
}

#[test]
fn workflow_graph_refuses_unknown_fields_inside_type_expr_payloads() {
    let graph = WorkflowGraph {
        schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
        source_identity: Some("fixture".to_string()),
        facet_schema_version: None,
        declarations: vec![WorkflowDeclaration::Function(lashlang::FunctionDecl {
            name: "record".into(),
            params: vec![],
            return_ty: TypeExpr::Object(vec![TypeField {
                name: "value".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
            body: lashlang::Expr::String("result".into()),
        })],
        main: WorkflowSubgraph::default(),
    };
    let mut value = serde_json::to_value(graph).expect("graph serializes");
    value["declarations"][0]["return_ty"]["Object"][0]["future"] = serde_json::json!(true);

    let error = serde_json::from_value::<WorkflowGraph>(value)
        .expect_err("an unknown TypeField member must be refused inside the graph carrier");
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
fn validate_and_render_agree_on_every_graph_failure_class() {
    let fixture = || {
        workflow_graph_from_source(
            "const child = async () => { return 1; };\nconst value = 1;\nfinish(value);\n",
        )
        .expect("fixture projects")
    };

    let mut unsupported_schema = fixture();
    unsupported_schema.schema_version -= 1;

    let mut duplicate_node_id = fixture();
    duplicate_node_id.main.nodes[1].id = duplicate_node_id.main.nodes[0].id.clone();

    let mut unknown_node_reference = fixture();
    unknown_node_reference.main.edges.push(WorkflowEdge {
        id: "dangling".to_string(),
        from: unknown_node_reference.main.nodes[0].id.clone(),
        to: WorkflowNodeId::new("missing".to_string()),
        kind: WorkflowEdgeKind::Sequence,
    });

    let mut invalid_node_payload = fixture();
    let expression = invalid_node_payload
        .main
        .nodes
        .iter_mut()
        .find_map(|node| match &mut node.kind {
            WorkflowNodeKind::Data { expression, .. } => Some(expression),
            _ => None,
        })
        .expect("fixture contains a data node");
    *expression = lashlang::Expr::SleepFor(Box::new(lashlang::Expr::Number(1.0)));

    let mut invalid_opaque_source = fixture();
    invalid_opaque_source.main.nodes[0].kind = WorkflowNodeKind::Opaque {
        source: "let =".to_string(),
    };

    let mut duplicate_process_name = fixture();
    let process = duplicate_process_name
        .declarations
        .iter()
        .find(|declaration| matches!(declaration, WorkflowDeclaration::Process(_)))
        .expect("fixture declares a process")
        .clone();
    duplicate_process_name.declarations.push(process);

    let mut canonical_source = fixture();
    canonical_source.main.nodes[0].name_source = WorkflowNodeNameSource::Label;
    canonical_source.main.nodes[0].name = "Close */ me".into();

    let mut rendered_source_invalid =
        workflow_graph_from_source("finish(1);\n").expect("final-parse fixture projects");
    let terminal = rendered_source_invalid
        .main
        .nodes
        .iter_mut()
        .find_map(|node| match &mut node.kind {
            WorkflowNodeKind::Terminal { expression, .. } => Some(expression),
            _ => None,
        })
        .expect("fixture contains a terminal node");
    *terminal = lashlang::Expr::FunctionReturn(Box::new(lashlang::Expr::Number(1.0)));

    let cases = [
        ("unsupported_schema_version", unsupported_schema),
        ("duplicate_node_id", duplicate_node_id),
        ("unknown_node_reference", unknown_node_reference),
        ("invalid_node_payload", invalid_node_payload),
        ("invalid_opaque_source", invalid_opaque_source),
        ("duplicate_process_name", duplicate_process_name),
        ("canonical_source", canonical_source),
        ("rendered_source_invalid", rendered_source_invalid),
    ];
    for (expected_code, graph) in cases {
        let validation_error = validate(&graph).expect_err(expected_code);
        let render_error = workflow_graph_to_source(&graph).expect_err(expected_code);
        assert_eq!(validation_error.code(), expected_code);
        assert_eq!(validation_error.code(), render_error.code());
        assert_eq!(validation_error.node_id(), render_error.node_id());
        assert_eq!(validation_error.field(), render_error.field());
    }
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
                authored_binding: None,
                binding: "item".to_string(),
                iterable: ir("[]"),
                bind: None,
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
fn invalid_host_edited_text_is_refused_before_it_enters_the_graph() {
    let globals = BTreeSet::from(["value".to_string(), "state".to_string()]);
    assert!(parse_typescript_expression("value <", &globals, &BTreeSet::new()).is_err());
    assert!(parse_typescript_assign_target("state.", &globals, &BTreeSet::new()).is_err());
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
await triggers.register({source:{expr:"0 8 * * *"},target:{definition:async(event)=>{await tools.echo({value:"inline"});return event;}}});
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
fn artifact_projection_rebuilds_canonical_spans_for_lifted_processes() {
    let authored = "const worker=async()=>{await sleep(1);return 1;};";
    let linked = lash_typescript::link(authored, &lashlang::testing::harness::test_environment())
        .expect("compact process source links");
    let canonical =
        typescript_program_source(linked.artifact.ir()).expect("the artifact prints canonically");
    assert_ne!(authored, canonical, "the fixture must change formatting");

    let graph = lash_typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact);
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
    let draft = workflow_graph_from_source(authored).expect("source projection succeeds");
    assert_eq!(
        graph
            .nodes()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>(),
        draft
            .nodes()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>(),
        "the runnable view and the draft mint the same node ids"
    );
    assert_eq!(
        graph
            .nodes()
            .map(|node| node.source_span)
            .collect::<Vec<_>>(),
        draft
            .nodes()
            .map(|node| node.source_span)
            .collect::<Vec<_>>(),
        "the runnable view carries the draft's canonical provenance"
    );
}

#[test]
fn canonical_span_goldens_cover_every_textual_node() {
    let fixtures: Vec<(&str, &str, &[&str])> = vec![
        (
            "named-nested-repeated",
            goldens::SPAN_NAMED_NESTED_REPEATED,
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
            goldens::SPAN_LIFTED_INLINE,
            &[
                "await (triggers.register({ source: timer.Schedule({ expr: \"0 8 * * *\" }), target: { definition: async (event) => {\n  await (tools.echo({ value: \"inline\" }));\n  return event;\n} } }))",
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
fn type_diagnostic_carries_slot_and_kind() {
    let graph = workflow_graph_from_source_with_facets(
        "await tools.compose({ query: \"bad\", items: [\"a\"] });\n",
        Some(&slot_path_environment()),
    )
    .expect("a type mismatch remains projectable");
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
    assert_eq!(
        diagnostic.slot.as_ref().map(ToString::to_string).as_deref(),
        Some("arg[0][\"query\"]")
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
        vec![("resource_operation", "ready"), ("loop", "while")]
    );
    assert_eq!(body.nodes[0].execution_sites[0].label, "tick");
    assert_lens_laws(source);
}

#[test]
fn structured_loop_control_round_trips() {
    let source = "for (const value of [1, 2]) {\n  if (value === 1) {\n    continue;\n  } else {\n    break;\n  }\n}\n";
    assert_lens_laws(source);
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

#[path = "workflow_graph/carrier_fix_round.rs"]
mod carrier_fix_round;
#[path = "workflow_graph/goldens.rs"]
mod goldens;

#[path = "workflow_graph/adr_claims.rs"]
mod adr_claims;

#[test]
fn workflow_diagnostic_classification_is_required_and_closed_on_the_wire() {
    let mut value = serde_json::to_value(populated_facet_graph()).expect("graph encodes");
    value["main"]["nodes"][0]["type_facets"]["diagnostics"] = serde_json::json!([{
        "node_id": value["main"]["nodes"][0]["id"].clone(),
        "kind": "unknown_name",
        "classification": "definite",
        "message": "fixture"
    }]);
    for classification in ["definite", "advisory"] {
        value["main"]["nodes"][0]["type_facets"]["diagnostics"][0]["classification"] =
            serde_json::json!(classification);
        let decoded =
            WorkflowGraph::decode_json_value(value.clone()).expect("closed classification decodes");
        let encoded = serde_json::to_value(decoded).expect("graph encodes");
        assert_eq!(
            encoded["main"]["nodes"][0]["type_facets"]["diagnostics"][0]["classification"],
            classification
        );
    }
    for classification in [
        serde_json::Value::Null,
        serde_json::json!("future"),
        serde_json::json!(0),
    ] {
        value["main"]["nodes"][0]["type_facets"]["diagnostics"][0]["classification"] =
            classification;
        assert!(
            WorkflowGraph::decode_json_value(value.clone()).is_err(),
            "invalid classification must be refused"
        );
    }
    value["main"]["nodes"][0]["type_facets"]["diagnostics"][0]
        .as_object_mut()
        .expect("diagnostic object")
        .remove("classification");
    assert!(
        WorkflowGraph::decode_json_value(value).is_err(),
        "unclassified diagnostics must be refused"
    );
}
