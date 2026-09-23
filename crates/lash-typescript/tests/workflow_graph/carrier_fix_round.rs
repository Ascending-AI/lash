//! FIG-3571 review fixes to the lens: literals lifted out of declared
//! processes, derived process origins, and compound member assignment.

use super::*;

/// A literal the linker lifts out of a declared process's body (its site is
/// declaration-rooted) is spanned where the printed program holds it: inside
/// the literal the declared process prints as (FIG-3571 review finding 11).
#[test]
fn artifact_projection_spans_a_literal_lifted_from_a_declared_process() {
    // Lower TypeScript whose outer arrow holds a const-bound inner literal,
    // then make the outer arrow a declared process of the direct IR, which
    // TypeScript itself never declares.
    let authored = "const worker=async()=>{const inner=async()=>{await sleep(2);return 2;};await sleep(1);return 1;};";
    let mut program = lash_typescript::parse(authored).expect("fixture parses");
    let lashlang::Expr::Block(statements) = &mut program.main else {
        panic!("a lowered program's main is a block")
    };
    let [lashlang::Expr::Assign { target, expr }] = statements.as_mut_slice() else {
        panic!("the fixture binds one process literal")
    };
    let lashlang::Expr::ProcessLiteral(literal) = std::mem::replace(
        expr.as_mut(),
        lashlang::Expr::ProcessRef {
            process: "worker".into(),
        },
    ) else {
        panic!("the fixture binds a process literal")
    };
    assert_eq!(target.root.as_str(), "worker");
    program
        .declarations
        .push(lashlang::Declaration::Process(lashlang::ProcessDecl {
            name: "worker".into(),
            params: Vec::new(),
            signals: Vec::new(),
            return_ty: None,
            label: None,
            origin: lashlang::ProcessOrigin::Declared,
            body: *literal.body,
        }));
    let linked =
        lashlang::LinkedModule::link(program, lashlang::testing::harness::test_environment())
            .expect("the declared process links");
    let lifted = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) if process.origin.is_lifted() => Some(process),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Inferring the declared process's output lowers its body once more; the
    // literal still lifts to one declaration.
    let [inner] = lifted.as_slice() else {
        panic!("the inner literal lifts exactly once, got {}", lifted.len())
    };
    assert!(
        matches!(
            &inner.origin,
            lashlang::ProcessOrigin::Lifted { site, .. }
                if matches!(site.root, lashlang::AstRoot::Declaration(_))
        ),
        "the inner literal is lifted out of the declared body: {:?}",
        inner.origin
    );
    let canonical =
        typescript_program_source(linked.artifact.ir()).expect("the artifact prints canonically");
    let graph = lash_typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact);
    let body_text = |name: &str| {
        let process = graph
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) if process.name == name => Some(process),
                _ => None,
            })
            .expect("the process projects");
        process
            .body
            .nodes
            .iter()
            .map(|node| source_slice(&canonical, node))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        body_text(inner.name.as_str()),
        ["sleep(2)", "return 2;"],
        "{canonical}"
    );
    assert_eq!(
        body_text("worker")[1..],
        ["sleep(1)", "return 1;"],
        "{canonical}"
    );
}

/// A process's origin is derived when its program is admitted, never authored
/// (FIG-3571): a submitted graph cannot drop it, move it, rename the lifted
/// process or overstate its hidden parameters. The lifted body is matched to
/// its literal by that origin, so an edit inside it still renders, and so
/// does a label a host adds to the statement holding the literal.
#[test]
fn graph_submission_cannot_edit_a_process_origin() {
    let source = "const child = async () => { return 1; };\nfinish(1);\n";
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let edit = |change: &dyn Fn(&mut lashlang::WorkflowProcess)| {
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
        &edit(&|process| process.origin = lashlang::ProcessOrigin::Declared),
        "dropping a lifted process's origin",
    );
    refused(
        &edit(&|process| {
            if let lashlang::ProcessOrigin::Lifted { site, .. } = &mut process.origin {
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
            if let lashlang::ProcessOrigin::Lifted { hidden_params, .. } = &mut process.origin {
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

/// A compound member assignment is an attribute update (FIG-3571): it
/// projects as a state update of its target with its operator, and renders
/// back as the same compound assignment.
#[test]
fn compound_member_assignment_is_an_attribute_update() {
    let source = "const box = { value: 1 };\nbox.value += 2;\nfinish(box.value);\n";
    let graph = workflow_graph_from_source(source).expect("fixture projects");
    let update = graph
        .nodes()
        .find_map(|node| match &node.kind {
            WorkflowNodeKind::StateUpdate {
                target,
                expression,
                update: Some(operator),
            } => Some((target.clone(), expression.clone(), *operator)),
            _ => None,
        })
        .expect("the compound assignment projects as a state update");
    assert_eq!(update.0.root.as_str(), "box");
    assert_eq!(update.1, lashlang::Expr::Number(2.0));
    assert_eq!(update.2, lashlang::UpdateOperator::Add);
    let rendered = workflow_graph_to_source(&graph).expect("the graph renders");
    assert!(rendered.contains("box.value += 2;"), "{rendered}");
}
