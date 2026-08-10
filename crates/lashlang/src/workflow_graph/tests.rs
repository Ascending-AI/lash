use super::*;
use crate::TypeField;

const REPRESENTATIVE: &str = r#"type Input = { enabled: bool, values: list[num] }

@label(title: "Run child", description: "A labeled process")
process child(input: Input) signals { refresh: null } -> number {
  total = 0
  for value in input.values {
    @label(title: "Pause")
    sleep for 1
  }
  signal = wait_signal("refresh")
  finish total
}

items = [value * 2 for value in [1, 2, 3] if value > 1]
if true {
  @label(title: "Print items", description: "Label metadata survives")
  print items
} else {}
finish items
"#;

#[test]
fn canonical_get_put_and_put_get() {
    let canonical = canonical_program_source(&parse(REPRESENTATIVE).unwrap()).unwrap();
    let graph = workflow_graph_from_source(&canonical).unwrap();
    let rendered = workflow_graph_to_source(&graph).unwrap();
    assert_eq!(rendered, canonical);
    assert_eq!(workflow_graph_from_source(&rendered).unwrap(), graph);
    assert_eq!(parse(&rendered).unwrap(), parse(&canonical).unwrap());
}

#[test]
fn expression_if_and_direct_else_if_obey_all_lens_laws() {
    let source = r#"choice = (true ? 1 : (false ? 2 : 3))
if choice == 1 {
  print "one"
} else if choice == 2 {
  print "two"
} else {
  print "other"
}
finish choice
"#;
    let canonical = canonical_program_source(&parse(source).unwrap()).unwrap();
    let graph = workflow_graph_from_source(&canonical).unwrap();

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
        else_graph.as_deref().unwrap().nodes.as_slice(),
        [WorkflowNode {
            kind: WorkflowNodeKind::Container(WorkflowContainer::If {
                then_is_block: true,
                ..
            }),
            ..
        }]
    ));

    let rendered = workflow_graph_to_source(&graph).unwrap();
    assert_eq!(rendered, canonical, "GetPut and canonical fixpoint");
    assert_eq!(parse(&rendered).unwrap(), parse(&canonical).unwrap());
    assert_eq!(
        workflow_graph_from_source(&rendered).unwrap(),
        graph,
        "PutGet"
    );
}

#[test]
fn canonicalization_discards_comments_but_preserves_labels() {
    let graph = workflow_graph_from_source(
        "# comment\n@label(title: \"Named\", description: \"Kept\")\nvalue = 1\n",
    )
    .unwrap();
    let rendered = workflow_graph_to_source(&graph).unwrap();
    assert!(!rendered.contains("comment"));
    assert!(rendered.contains("@label(title: \"Named\", description: \"Kept\")"));
}

#[test]
fn invalid_graphs_are_refused() {
    let mut graph = workflow_graph_from_source("value = 1\nfinish value\n").unwrap();
    graph.main.edges.push(WorkflowEdge {
        id: "dangling".to_string(),
        from: graph.main.nodes[0].id.clone(),
        to: WorkflowNodeId("missing".to_string()),
        kind: WorkflowEdgeKind::Sequence,
    });
    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::UnknownNodeReference { .. })
    ));

    let mut graph = workflow_graph_from_source("if true { value = 1 }\n").unwrap();
    let WorkflowNodeKind::Container(WorkflowContainer::If { then_graph, .. }) =
        &mut graph.main.nodes[0].kind
    else {
        panic!("expected if container")
    };
    *then_graph = None;
    assert!(matches!(
        workflow_graph_to_source(&graph),
        Err(GraphRenderError::MissingRequiredChild {
            child: "then_graph",
            ..
        })
    ));
}

#[test]
fn while_and_path_assignment_are_structured_and_typed() {
    let source = "state = { count: 0 }\nstate.count = 1\nwhile state.count < 3 { state.count = state.count + 1 }\nfinish state\n";
    let graph = workflow_graph_from_source(source).unwrap();
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
        body.as_deref().unwrap().nodes[0].kind,
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
    let rendered = workflow_graph_to_source(&graph).unwrap();
    let canonical = canonical_program_source(&parse(source).unwrap()).unwrap();
    assert_eq!(rendered, canonical);
    assert_eq!(workflow_graph_from_source(&rendered).unwrap(), graph);
}

#[test]
fn edited_expression_text_is_rendered_and_reprojected() {
    let source = r#"process child() {
  finish 1
}

process workflow() {
  state = { count: 0, other: 0 }
  while state.count < 3 {
    state.count = state.count + 1
  }
  state.count = 7
  runs = [start child(), start child()]
  finish state
}
"#;
    let graph = workflow_graph_from_source(source).unwrap();

    let mut edited = graph.clone();
    let process = edited
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.name == "workflow" => Some(process),
            _ => None,
        })
        .unwrap();
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
    *expression = "[start child(), start child(), start child()]".to_string();

    let rendered = workflow_graph_to_source(&edited).unwrap();
    assert!(
        rendered.contains("while (state.count < 2)"),
        "rendered source:\n{rendered}"
    );
    assert!(rendered.contains("state.other = (state.count + 40)"));
    assert!(rendered.contains("started = [start child(), start child(), start child()]"));

    let reprojected = workflow_graph_from_source(&rendered).unwrap();
    let process = reprojected.process("workflow").unwrap();
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
                && expression == "[start child(), start child(), start child()]"
    ));
    assert_eq!(workflow_graph_to_source(&reprojected).unwrap(), rendered);
}

#[test]
fn invalid_edited_expression_returns_field_typed_error() {
    let mut graph =
        workflow_graph_from_source("process workflow() { while true { sleep for 1 } finish null }")
            .unwrap();
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .unwrap();
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
        "process workflow() { state = { count: 0 } state.count = 1 finish state }",
    )
    .unwrap();
    let process = graph.process("workflow").unwrap();
    let state_update_id = process.body.nodes[1].id.clone();
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .unwrap();
    let node = process
        .body
        .nodes
        .iter_mut()
        .find(|node| node.id == state_update_id)
        .unwrap();
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
    let source = r#"process workflow() {
  values = [1, 2]
  if true { sleep for 1 } else { sleep for 2 }
  for value in values { sleep for value }
  selected = [value for value in values if value > 0]
  finish selected
}
"#;
    let mut graph = workflow_graph_from_source(source).unwrap();
    let process = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            _ => None,
        })
        .unwrap();
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
    let WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { clauses, .. }) =
        &mut process.body.nodes[3].kind
    else {
        panic!("expected list-comprehension container")
    };
    clauses[0] = WorkflowListComprehensionClause::For {
        binding: "value".to_string(),
        iterable: "[5, 6]".to_string(),
    };
    clauses[1] = WorkflowListComprehensionClause::If {
        condition: "(value > 5)".to_string(),
    };

    let rendered = workflow_graph_to_source(&graph).unwrap();
    assert!(rendered.contains("if false"));
    assert!(rendered.contains("for value in [3, 4]"));
    assert!(rendered.contains("for value in [5, 6] if (value > 5)"));

    let reprojected = workflow_graph_from_source(&rendered).unwrap();
    let process = reprojected.process("workflow").unwrap();
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
    assert!(matches!(
        &process.body.nodes[3].kind,
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { clauses, .. })
            if clauses == &vec![
                WorkflowListComprehensionClause::For {
                    binding: "value".to_string(),
                    iterable: "[5, 6]".to_string(),
                },
                WorkflowListComprehensionClause::If {
                    condition: "(value > 5)".to_string(),
                },
            ]
    ));
    assert_eq!(workflow_graph_to_source(&reprojected).unwrap(), rendered);
}

#[test]
fn execution_site_correlation_survives_edit_and_reprojection() {
    let source = r#"if await tools.ready({ attempt: 1 })? {
  print "ready"
} else {
  print "not ready"
}
for value in await tools.values({ batch: 1 })? {
  print "loop"
}
selected = [1 for value in await tools.values({ batch: 2 })? if await tools.keep({})?]
finish selected
"#;
    let mut graph = workflow_graph_from_source(source).unwrap();
    let original_sites = graph.main.nodes[..3]
        .iter()
        .map(|node| node.execution_sites.clone())
        .collect::<Vec<_>>();
    assert!(original_sites.iter().all(|sites| !sites.is_empty()));

    let WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. }) =
        &mut graph.main.nodes[0].kind
    else {
        panic!("expected if container")
    };
    *condition = "await tools.ready({ attempt: 2 })?".to_string();

    let WorkflowNodeKind::Container(WorkflowContainer::For {
        binding, iterable, ..
    }) = &mut graph.main.nodes[1].kind
    else {
        panic!("expected for container")
    };
    *binding = "entry".to_string();
    *iterable = "await tools.values({ batch: 3 })?".to_string();

    let WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { clauses, .. }) =
        &mut graph.main.nodes[2].kind
    else {
        panic!("expected list-comprehension container")
    };
    clauses[0] = WorkflowListComprehensionClause::For {
        binding: "candidate".to_string(),
        iterable: "await tools.values({ batch: 4 })?".to_string(),
    };
    clauses[1] = WorkflowListComprehensionClause::If {
        condition: "await tools.keep({ strict: true })?".to_string(),
    };

    let rendered = workflow_graph_to_source(&graph).unwrap();
    let reprojected = workflow_graph_from_source(&rendered).unwrap();

    for (node, sites) in reprojected.main.nodes[..3].iter().zip(original_sites) {
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
    let graph = workflow_graph_from_source(
        "total = 0\nfor value in [1, 2] { total = total + value }\nfinish total\n",
    )
    .unwrap();
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) =
        &graph.main.nodes[1].kind
    else {
        panic!("expected for container")
    };
    assert!(matches!(
        body.as_deref().unwrap().nodes[0].kind,
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
    assert!(workflow_graph_to_source(&graph).is_ok());
}

#[test]
fn loops_publish_path_and_loop_introduced_writes_once() {
    let source = r#"state = { count: 0 }
for item in [1, 2] {
  state.count = item
  introduced = item
  item = item + 1
}
finish [state, introduced]
"#;
    let graph = workflow_graph_from_source(source).unwrap();
    let loop_node = &graph.main.nodes[1];
    assert_eq!(
        loop_node.outputs,
        vec![
            VariableVersion {
                variable: "introduced".to_string(),
                version: 1,
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
        body.as_deref().unwrap().nodes[0].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    assert!(matches!(
        body.as_deref().unwrap().nodes[2].kind,
        WorkflowNodeKind::StateUpdate { .. }
    ));
    assert_lens_laws(source);
}

#[test]
fn scoped_loop_bindings_do_not_depend_on_or_replace_outer_versions() {
    let source = r#"item = 99
items = [1, 2]
for item in items {
  print item
}
selected = [item for item in items if item > 0]
finish item
"#;
    let graph = workflow_graph_from_source(source).unwrap();
    let outer_item = &graph.main.nodes[0];
    let WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) =
        &graph.main.nodes[2].kind
    else {
        panic!("expected for container")
    };
    let body_node = &body.as_deref().unwrap().nodes[0];
    assert!(!body.as_deref().unwrap().edges.iter().any(|edge| {
        edge.from == outer_item.id
            && edge.to == body_node.id
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { ref variable, .. } if variable == "item"
            )
    }));
    assert!(!graph.main.edges.iter().any(|edge| {
        edge.from == outer_item.id
            && (edge.to == graph.main.nodes[2].id || edge.to == graph.main.nodes[3].id)
            && matches!(
                edge.kind,
                WorkflowEdgeKind::DataDependency { ref variable, .. } if variable == "item"
            )
    }));
    assert!(graph.main.edges.iter().any(|edge| {
        edge.from == outer_item.id
            && edge.to == graph.main.nodes[4].id
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
        r#"process scoped(record: any) {
  state = { count: 0 }
  first = 1
  for item in [1] { nested = first + item }
  finish state
}
"#,
    )
    .unwrap();
    let process = graph.process("scoped").unwrap();
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
        body.as_deref().unwrap().nodes[0].available_variables,
        ["first", "item", "record", "state"]
    );
    assert_eq!(
        process.body.nodes[3].available_variables,
        ["first", "nested", "record", "state"]
    );
}

#[test]
fn catalog_projection_exposes_typed_facets_non_fatally() {
    let mut catalog = crate::LashlangHostCatalog::new();
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
    let environment = crate::LashlangHostEnvironment::new(catalog, crate::LashlangAbilities::all())
        .with_language_features(
            crate::LashlangLanguageFeatures::default().with_label_annotations(),
        );
    let source = r##"process workflow(name: str) {
  query = name
  result = await tools.lookup({ query: query })?
  for item in "not a list" { seen = item }
  finish result
}
"##;

    let graph = workflow_graph_from_source_with_facets(source, Some(&environment)).unwrap();
    assert_eq!(
        graph.facet_schema_version,
        Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let process = graph.process("workflow").unwrap();

    let call_facets = process.body.nodes[1].type_facets.as_ref().unwrap();
    assert!(
        call_facets
            .available_variables
            .iter()
            .any(|variable| { variable.name == "query" && variable.ty == TypeExpr::Str })
    );
    assert!(
        call_facets
            .expected_arguments
            .iter()
            .any(|argument| { argument.slot == "arg[0].query" && argument.ty == TypeExpr::Str })
    );

    let loop_facets = process.body.nodes[2].type_facets.as_ref().unwrap();
    assert!(loop_facets.available_variables.iter().any(|variable| {
        variable.name == "result"
            && variable.ty
                == TypeExpr::Object(vec![TypeField {
                    name: "answer".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }])
    }));
    assert!(loop_facets.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == "incompatible_iteration_target"
            && diagnostic.message.contains("expected a list")
    }));
}

#[test]
fn type_facets_are_ignored_by_put_and_canonicalization() {
    let source = "value = \"text\"\nfinish value\n";
    let canonical = canonical_program_source(&parse(source).unwrap()).unwrap();
    let environment = crate::LashlangHostEnvironment::new(
        crate::LashlangHostCatalog::new(),
        crate::LashlangAbilities::all(),
    );
    let mut graph = workflow_graph_from_source_with_facets(source, Some(&environment)).unwrap();
    graph.facet_schema_version = Some(WORKFLOW_TYPE_FACET_SCHEMA_VERSION + 99);
    let terminal_id = graph.main.nodes[1].id.clone();
    graph.main.nodes[1]
        .type_facets
        .as_mut()
        .unwrap()
        .diagnostics
        .push(WorkflowTypeDiagnostic {
            node_id: terminal_id,
            kind: "client_echo".to_string(),
            message: "must not become source".to_string(),
            span: None,
        });

    assert_eq!(workflow_graph_to_source(&graph).unwrap(), canonical);
    let reprojected = workflow_graph_from_source(&canonical).unwrap();
    assert_eq!(reprojected.facet_schema_version, None);
    assert!(reprojected.nodes().all(|node| node.type_facets.is_none()));
}

#[test]
fn standalone_pure_expressions_remain_computations() {
    let graph = workflow_graph_from_source("1 + 1\n").unwrap();
    assert!(matches!(
        graph.main.nodes[0].kind,
        WorkflowNodeKind::Computation { ref expression, binding: None }
            if expression == "(1 + 1)"
    ));
    assert_lens_laws("1 + 1\n");
}

#[test]
fn reference_type_literals_and_effectful_composites_are_typed() {
    let source = r#"type Item = { name: str }

process child() {
  finish 1
}

schema = Type { item: Item }
runs = [start child(), start child()]
tupled = (await runs[0], await runs[1])
listed = [await runs[0], await runs[1]]
recorded = { value: await runs[0] }
built = len(await runs)
binary = (await runs[0] + 1)
unary = not await runs[0]
field = (await runs[0]).value
indexed = (await runs)[0]
unusual = (await runs[0])??
finish unusual
"#;
    let graph = workflow_graph_from_source(source).unwrap();
    assert!(matches!(
        &graph.main.nodes[0].kind,
        WorkflowNodeKind::Data { expression, .. } if expression == "Type { item: Item }"
    ));
    assert!(
        graph.main.nodes[1..=10]
            .iter()
            .all(|node| matches!(node.kind, WorkflowNodeKind::Computation { .. }))
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
    let source = r#"@label(title: "Loop guard")
while await tools.ready({})? {
  await tools.tick({})?
}
"#;
    let graph = workflow_graph_from_source(source).unwrap();
    let node = &graph.main.nodes[0];
    let WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) = &node.kind else {
        panic!("expected while container")
    };
    assert_eq!(
        node.execution_sites
            .iter()
            .map(|site| site.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Loop guard", "ready"]
    );
    assert_eq!(
        body.as_deref().unwrap().nodes[0].execution_sites[0].label,
        "tick"
    );
    assert_lens_laws(source);
}

fn assert_lens_laws(source: &str) {
    let canonical = canonical_program_source(&parse(source).unwrap()).unwrap();
    let graph = workflow_graph_from_source(&canonical).unwrap();
    let rendered = workflow_graph_to_source(&graph).unwrap();
    assert_eq!(rendered, canonical, "GetPut");
    assert_eq!(parse(&rendered).unwrap(), parse(&canonical).unwrap());
    assert_eq!(
        workflow_graph_from_source(&rendered).unwrap(),
        graph,
        "PutGet"
    );
}

#[test]
fn structured_loop_control_round_trips_without_opaque_parse_context() {
    let source = "for value in [1, 2] {\n  if value == 1 { continue } else { break }\n}\n";
    let canonical = canonical_program_source(&parse(source).unwrap()).unwrap();
    let graph = workflow_graph_from_source(&canonical).unwrap();
    assert_eq!(workflow_graph_to_source(&graph).unwrap(), canonical);
}
