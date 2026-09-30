use lash::typescript::workflow_graph::{
    GraphRenderError, validate_for_fleet, workflow_graph_from_source,
    workflow_graph_to_source_for_fleet,
};
use lash_core::FleetFormat;
use lashlang::{WORKFLOW_GRAPH_SCHEMA_VERSION, WorkflowGraph, WorkflowGraphDecodeError};

#[cfg(feature = "synthetic-next")]
#[test]
fn predecessor_graph_validates_on_synthetic_next() {
    assert_eq!(WORKFLOW_GRAPH_SCHEMA_VERSION, 22);
    let fleet = FleetFormat::from_version(1);
    let mut graph = workflow_graph_from_source("finish(1);\n").expect("project fixture");
    graph.schema_version = 21;
    let graph = WorkflowGraph::decode_json_value_for_fleet(
        serde_json::to_value(graph).expect("encode graph"),
        fleet,
    )
    .expect("N+1 decodes N's graph while F is N's epoch");
    validate_for_fleet(&graph, fleet).expect("an admitted predecessor graph validates");
    assert_eq!(
        workflow_graph_to_source_for_fleet(&graph, fleet).expect("render"),
        "finish(1);\n"
    );
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source_in_session_for_fleet(
            &graph,
            &std::collections::BTreeSet::new(),
            fleet,
        )
        .expect("render a session cell"),
        "finish(1);\n"
    );
    let error = lash::typescript::workflow_graph::validate(&graph)
        .expect_err("after finalize a derived predecessor regenerates");
    assert!(
        matches!(error, GraphRenderError::UnsupportedSchemaVersion(refusal)
        if refusal.found == 21 && refusal.reads.recorded() == 22)
    );
    assert!(
        WorkflowGraph::decode_json_value(serde_json::to_value(&graph).expect("encode graph"),)
            .is_err()
    );
}

#[test]
fn out_of_range_graph_refuses_typed() {
    let fleet = FleetFormat::from_version(1);
    let mut graph = workflow_graph_from_source("finish(1);\n").expect("project fixture");
    for found in [20, WORKFLOW_GRAPH_SCHEMA_VERSION + 1] {
        graph.schema_version = found;
        let error = validate_for_fleet(&graph, fleet).expect_err("outside the read window");
        let GraphRenderError::UnsupportedSchemaVersion(refusal) = error else {
            panic!("expected a typed version refusal, got {error}");
        };
        assert_eq!(refusal.found, found);
        assert_eq!(refusal.reads.recorded(), 21);
        assert_eq!(
            refusal.reads.supported().min(),
            WORKFLOW_GRAPH_SCHEMA_VERSION
        );
        assert_eq!(
            refusal.reads.supported().max(),
            WORKFLOW_GRAPH_SCHEMA_VERSION
        );
        assert_eq!(
            workflow_graph_to_source_for_fleet(&graph, fleet).expect_err("render refuses too"),
            error
        );
        let mut value = serde_json::to_value(&graph).expect("encode graph");
        value["main"] = serde_json::json!({ "future": true });
        let decoded = WorkflowGraph::decode_json_value_for_fleet(value, fleet)
            .expect_err("version refusal precedes an unknown shape");
        assert!(
            matches!(decoded, WorkflowGraphDecodeError::UnsupportedSchemaVersion(actual)
            if actual == refusal)
        );
        assert!(
            error.to_string().contains("supported range"),
            "refusal names its range: {error}"
        );
    }
}

#[test]
fn workflow_diagnostic_classification_changes_in_place_under_the_version_freeze() {
    assert_eq!(lashlang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION, 4);
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let graph = lash::typescript::workflow_graph::workflow_graph_from_source_with_facets(
        "const shape = { ready: true }; finish(shape.missing);",
        Some(&environment),
    )
    .expect("draft projects");
    assert_eq!(graph.schema_version, WORKFLOW_GRAPH_SCHEMA_VERSION);
    assert_eq!(graph.facet_schema_version, Some(4));
    let diagnostics = graph
        .nodes()
        .filter_map(|node| node.type_facets.as_ref())
        .flat_map(|facets| &facets.diagnostics)
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = serde_json::to_value(diagnostics[0]).expect("diagnostic encodes");
    assert_eq!(diagnostic["kind"], "unknown_object_field");
    assert_eq!(diagnostic["classification"], "definite");
    let value = serde_json::to_value(graph).expect("graph encodes");
    let decoded =
        WorkflowGraph::decode_json_value(value).expect("current frozen facet shape decodes");
    assert_eq!(decoded.facet_schema_version, Some(4));
}
