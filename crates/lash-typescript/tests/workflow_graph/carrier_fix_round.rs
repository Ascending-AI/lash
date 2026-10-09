//! FIG-3571 review fixes to the lens: literals lifted out of declared
//! processes, derived process origins, and compound member assignment.

use super::*;

/// A process's origin is derived when its program is admitted, never authored
/// (FIG-3571): a submitted graph cannot drop it, move it, rename the lifted
/// process or overstate its hidden parameters. The lifted body is matched to
/// its literal by that origin, so an edit inside it still renders, and so
/// does a label a host adds to the statement holding the literal.
#[test]
fn graph_submission_cannot_edit_a_process_origin() {
    let source = "const child = async () => { return 1; };\nfinish(1);\n";
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let edit = |change: &dyn Fn(&mut lash_vm::WorkflowProcess)| {
        let mut edited = graph.clone();
        let process = edited
            .declarations
            .iter_mut()
            .find_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) if process.origin.is_lifted() => {
                    Some(process)
                }
                _ => None,
            })
            .expect("the fixture lifts one process");
        change(process);
        edited
    };
    let refused = |graph: &WorkflowGraph, what: &str| {
        let error = validate(graph).expect_err(what);
        assert_eq!(error.code(), "process_origin_mismatch", "{what}: {error}");
        assert_eq!(
            workflow_graph_to_source(graph).expect_err(what).code(),
            "process_origin_mismatch",
            "{what}"
        );
    };

    refused(
        &edit(&|process| process.origin = lash_vm::ProcessOrigin::Declared),
        "dropping a lifted process's origin",
    );
    refused(
        &edit(&|process| {
            if let lash_vm::ProcessOrigin::Lifted { site, .. } = &mut process.origin {
                site.steps.push(0);
            }
        }),
        "moving a lifted process's site",
    );
    refused(
        &edit(&|process| process.name = format!("{}0", process.name)),
        "renaming a lifted process",
    );
    refused(
        &edit(&|process| process.name = "authored".to_string()),
        "a lifted process without its digest name",
    );
    refused(
        &edit(&|process| {
            if let lash_vm::ProcessOrigin::Lifted { hidden_params, .. } = &mut process.origin {
                *hidden_params = 1;
            }
        }),
        "a lifted process with more hidden parameters than parameters",
    );

    let mut relabelled = graph.clone();
    relabelled.main.nodes[0].name_source = WorkflowNodeNameSource::Label;
    relabelled.main.nodes[0].name = "Child worker".into();
    let rendered = workflow_graph_to_source(&relabelled).expect("a label is not an origin edit");
    assert!(rendered.contains("return 1"), "{rendered}");
    assert_eq!(
        workflow_graph_to_source(&graph).expect("the unedited graph renders"),
        workflow_graph_to_source(&workflow_graph_from_source(source).expect("fixture projects"))
            .expect("the reprojection renders")
    );
}

/// A host edits an admitted view: it re-reads every node's text through the
/// lens's fragment door and inserts statements around a lifted literal. The
/// literal's lifted declaration is still found — by the reference the view
/// holds, as a reference or as the name it prints to, and for a draft by the
/// literal still digesting to its name at its declared site — so the edit
/// inside the process body renders wherever the literal now sits (FIG-3630).
/// A moved or renamed origin is still refused by
/// `graph_submission_cannot_edit_a_process_origin`.
#[test]
fn a_lifted_process_body_renders_after_host_text_round_trips_and_moves() {
    let source = "const blank = async () => {\n  return 0;\n};\n";
    let environment = lash_vm::testing::harness::test_environment();
    let admitted = workflow_graph_from_source_with_facets(source, Some(&environment))
        .expect("the source admits");
    let draft = workflow_graph_from_source(source).expect("the source projects");
    for (what, graph) in [("admitted view", admitted), ("draft", draft)] {
        let mut edited = graph.clone();
        let processes = edited
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) => Some(process.name.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        // A host spells every main expression as text and reads it back.
        for node in &mut edited.main.nodes {
            if let WorkflowNodeKind::Data { expression, .. } = &mut node.kind {
                let text = typescript_expression_source(expression).expect("print");
                *expression = parse_typescript_expression(&text, &BTreeSet::new(), &processes)
                    .expect("the lens reads back what it printed");
            }
        }
        // An edit inside the process body, and a statement inserted before
        // the literal.
        let process = edited
            .declarations
            .iter_mut()
            .find_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) if process.origin.is_lifted() => {
                    Some(process)
                }
                _ => None,
            })
            .expect("the literal lifts");
        let terminal = process
            .body
            .nodes
            .iter_mut()
            .find(|node| matches!(node.kind, WorkflowNodeKind::Terminal { .. }))
            .expect("the body returns");
        if let WorkflowNodeKind::Terminal {
            expression: lash_vm::Expr::FunctionReturn(value),
            ..
        } = &mut terminal.kind
        {
            **value = lash_vm::Expr::Number(7.0);
        }
        let mut inserted = edited.main.nodes[0].clone();
        inserted.id = WorkflowNodeId::new("node:0123456789abcdef01234567".to_string());
        inserted.kind = WorkflowNodeKind::Data {
            binding: Some(lash_vm::AssignTarget::variable("greeting".into())),
            expression: lash_vm::Expr::String("hello".into()),
        };
        edited.main.nodes.insert(0, inserted);

        let rendered = workflow_graph_to_source(&edited)
            .unwrap_or_else(|error| panic!("{what}: a moved literal still renders: {error}"));
        assert!(
            rendered.contains("greeting = \"hello\";"),
            "{what}: {rendered}"
        );
        assert!(rendered.contains("return 7;"), "{what}: {rendered}");
        assert_eq!(
            rendered, "let greeting = \"hello\";\nconst blank = async () => {\n  return 7;\n};\n",
            "{what}"
        );
    }
}
