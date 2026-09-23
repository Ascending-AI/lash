use std::collections::BTreeSet;

use lash_typescript::workflow_graph::{parse_typescript_expression, workflow_graph_from_source};
use lashlang::{
    WorkflowContainer, WorkflowDeclaration, WorkflowListComprehensionClause, WorkflowNode,
    WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowSubgraph,
};

fn ir(text: &str) -> lashlang::Expr {
    let globals = ["child", "tools", "value", "values"]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    parse_typescript_expression(text, &globals, &BTreeSet::new())
        .expect("fixture expression parses")
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
    let mut graph = workflow_graph_from_source(source).expect("fixture projects");
    graph.main.nodes.push(WorkflowNode {
        id: WorkflowNodeId::new("fixture:list-comprehension".to_string()),
        name: "list comprehension".to_string(),
        description: None,
        name_source: WorkflowNodeNameSource::Derived,
        kind: WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
            binding: None,
            clauses: vec![WorkflowListComprehensionClause::For {
                binding: "item".to_string(),
                iterable: ir("values"),
            }],
            element: Box::new(WorkflowSubgraph::default()),
        }),
        available_variables: Vec::new(),
        type_facets: None,
        outputs: Vec::new(),
        execution_sites: Vec::new(),
        source_span: None,
    });

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
            WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { .. }) => {
                "list_comprehension"
            }
            WorkflowNodeKind::Opaque { .. } => "opaque",
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
            "list_comprehension",
            "opaque",
        ])
    );
    assert!(
        graph
            .declarations
            .iter()
            .any(|declaration| matches!(declaration, WorkflowDeclaration::Process(_)))
    );

    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../schemas/host/workflow-graph/v16.schema.json"
    ))
    .expect("published graph schema parses");
    let validator = jsonschema::JSONSchema::compile(&schema).expect("graph schema compiles");
    let value = serde_json::to_value(&graph).expect("real graph serializes");
    if let Err(errors) = validator.validate(&value) {
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
