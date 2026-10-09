use std::collections::BTreeSet;

use lash_typescript::workflow_graph::workflow_graph_from_source;
use lash_vm::{WorkflowContainer, WorkflowDeclaration, WorkflowNodeKind};

#[allow(clippy::disallowed_methods)]
fn published_graph_schema() -> serde_json::Value {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/host/workflow-graph");
    let paths: Vec<_> = std::fs::read_dir(&directory)
        .expect("read published graph schema directory")
        .map(|entry| entry.expect("read published graph schema entry"))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with('v') && name.ends_with(".schema.json")
        })
        .map(|entry| entry.path())
        .collect();
    let [path] = paths.as_slice() else {
        panic!("exactly one published graph schema is required: {paths:?}");
    };
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read published schema {}: {error}", path.display()));
    serde_json::from_str(&source).expect("published graph schema parses")
}

#[test]
fn published_schema_accepts_real_graph_with_every_node_and_container_kind() {
    let source = r#"const child = async () => {
    return 1;
  };
const values = [1, 2];
await tools.lookup({ query: "x" });
await sleep("1s");
1 + 1;
let count = 0;
count = count + 1;
if (true) {
  console.log("yes");
} else {
  console.log("no");
}
for (const value of values) {
  console.log(value);
}
while (count < 2) {
  count = count + 1;
}
try {
  console.log("try");
} catch (error) {
  console.log(error);
}
finish(values);
"#;
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let kinds = graph
        .nodes()
        .map(|node| match &node.kind {
            WorkflowNodeKind::Data { .. } => "data",
            WorkflowNodeKind::Call { .. } => "call",
            WorkflowNodeKind::Effect { .. } => "effect",
            WorkflowNodeKind::Computation { .. } => "computation",
            WorkflowNodeKind::StateUpdate { .. } => "state_update",
            WorkflowNodeKind::Terminal { .. } => "terminal",
            WorkflowNodeKind::Container(WorkflowContainer::If { .. }) => "if",
            WorkflowNodeKind::Container(WorkflowContainer::For { .. }) => "for",
            WorkflowNodeKind::Container(WorkflowContainer::While { .. }) => "while",
            WorkflowNodeKind::Container(WorkflowContainer::Try { .. }) => "try",
            WorkflowNodeKind::Container(WorkflowContainer::Scope { .. }) => "scope",
            WorkflowNodeKind::Throw { .. } => "throw",
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        kinds,
        BTreeSet::from([
            "data",
            "call",
            "effect",
            "computation",
            "state_update",
            "terminal",
            "if",
            "for",
            "while",
            "try",
        ])
    );
    assert!(
        graph
            .declarations
            .iter()
            .any(|declaration| matches!(declaration, WorkflowDeclaration::Process(_)))
    );

    let schema = published_graph_schema();
    let validator = jsonschema::validator_for(&schema).expect("graph schema compiles");
    let value = serde_json::to_value(&graph).expect("real graph serializes");
    if !validator.is_valid(&value) {
        let errors = validator.iter_errors(&value);
        panic!(
            "published schema rejected a real graph:\n{}",
            errors
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let mut unknown = value;
    unknown["main"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .last_mut()
        .expect("list-comprehension node")["kind"]["future"] = serde_json::json!(true);
    assert!(!validator.is_valid(&unknown));
}

#[test]
fn published_schema_requires_a_closed_workflow_diagnostic_classification() {
    let graph = workflow_graph_from_source("finish(1);").expect("fixture projects");
    let mut value = serde_json::to_value(graph).expect("graph encodes");
    value["facet_schema_version"] = serde_json::json!(lash_vm::WORKFLOW_TYPE_FACET_SCHEMA_VERSION);
    value["main"]["nodes"][0]["type_facets"] = serde_json::json!({
        "diagnostics": [{
            "node_id": value["main"]["nodes"][0]["id"].clone(),
            "kind": "unknown_name",
            "classification": "definite",
            "message": "fixture"
        }]
    });
    let schema = published_graph_schema();
    let validator = jsonschema::validator_for(&schema).expect("published schema compiles");
    for classification in ["definite", "advisory"] {
        value["main"]["nodes"][0]["type_facets"]["diagnostics"][0]["classification"] =
            serde_json::json!(classification);
        assert!(
            validator.is_valid(&value),
            "closed classification must validate"
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
            !validator.is_valid(&value),
            "invalid classification must be refused"
        );
    }
    value["main"]["nodes"][0]["type_facets"]["diagnostics"][0]
        .as_object_mut()
        .expect("diagnostic object")
        .remove("classification");
    assert!(
        !validator.is_valid(&value),
        "unclassified diagnostics must be refused"
    );
}
