use super::*;
use crate::ast::{AstPath, CoercingUnaryOp};

fn assert_link_and_facet_binding(expr: Expr, expected: TypeExpr) {
    let surface = full_host_environment();
    let empty_program = Program::block(Vec::new());
    let linker = Linker::new(&empty_program, &surface);
    let mut scope = Scope::new(false, None);
    for name in &surface.globals {
        scope.bind(name, any_binding());
    }
    let (_, binding) = linker
        .lower_expr(&expr, &AstPath::main(Vec::new()), &mut scope)
        .expect("the canonical linker walk must lower the expression");
    assert_eq!(binding_type(&binding), expected.clone());

    let program = Program::block(vec![
        Expr::Assign {
            target: crate::AssignTarget::variable("value".into()),
            expr: Box::new(expr),
        },
        Expr::Print(Box::new(Expr::Variable("value".into()))),
    ]);
    LinkedModule::link(program.clone(), full_host_environment())
        .expect("the canonical walk must link the expression");

    let analysis = analyze_workflow_program(&program, &full_host_environment());
    let Expr::Block(nodes) = &program.main else {
        unreachable!("Program::block always produces a block")
    };
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        analysis
            .facts_for(&AstPath::main(vec![1]))
            .and_then(|facts| facts.available_variables.get("value")),
        Some(&expected),
    );
}

#[test]
fn canonical_walk_aligns_javascript_and_map_bindings_with_facets() {
    for (op, expected) in [
        (crate::CoercingUnaryOp::Not, TypeExpr::Bool),
        (crate::CoercingUnaryOp::TypeOf, TypeExpr::Str),
        (crate::CoercingUnaryOp::Plus, TypeExpr::Float),
        (crate::CoercingUnaryOp::Negate, TypeExpr::Float),
    ] {
        assert_link_and_facet_binding(
            Expr::CoercingUnary {
                op,
                expr: Box::new(Expr::Number(1.0)),
            },
            expected,
        );
    }

    for op in [
        crate::CoercingBinaryOp::StrictEqual,
        crate::CoercingBinaryOp::StrictNotEqual,
        crate::CoercingBinaryOp::LooseEqual,
        crate::CoercingBinaryOp::LooseNotEqual,
        crate::CoercingBinaryOp::Less,
        crate::CoercingBinaryOp::LessEqual,
        crate::CoercingBinaryOp::Greater,
        crate::CoercingBinaryOp::GreaterEqual,
    ] {
        assert_link_and_facet_binding(
            Expr::CoercingBinary {
                left: Box::new(Expr::Number(1.0)),
                op,
                right: Box::new(Expr::Number(2.0)),
            },
            TypeExpr::Bool,
        );
    }

    assert_link_and_facet_binding(
        Expr::CoercingBinary {
            left: Box::new(Expr::Number(1.0)),
            op: crate::CoercingBinaryOp::Add,
            right: Box::new(Expr::Number(2.0)),
        },
        TypeExpr::Any,
    );

    assert_link_and_facet_binding(
        Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(1.0)])),
            function: Box::new(Expr::Function(Box::new(crate::FunctionExpr {
                name: None,
                js_name: None,
                receiver: None,
                params: vec!["item".into()],
                captures: Vec::new(),
                body: Box::new(Expr::Variable("item".into())),
            }))),
        },
        TypeExpr::Any,
    );
}

#[test]
fn canonical_walk_visits_index_and_unary_operands_for_link_and_facets() {
    let witnesses = [
        (
            "value = [1][missing]",
            builders::index(
                builders::list(vec![builders::num(1.0)]),
                builders::var("missing"),
            ),
        ),
        (
            "value = -missing",
            builders::unary(CoercingUnaryOp::Negate, builders::var("missing")),
        ),
    ];
    for (source, operand) in witnesses {
        let program = builders::program(vec![builders::assign("value", operand)]);
        assert!(
            matches!(
                LinkedModule::link(program.clone(), full_host_environment()),
                Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
            ),
            "{source}"
        );
        let analysis = analyze_workflow_program(&program, &full_host_environment());

        let facts = statement_facts(&analysis, &statements(&program)[0], &AstPath::main(vec![0]));
        assert!(
            facts.diagnostics.iter().any(|diagnostic| {
                diagnostic.error.kind() == "unknown_name"
                    && diagnostic.error.to_string().contains("missing")
            }),
            "the recovered diagnostic must reach the owning statement for {source}"
        );
    }
}

/// The statements of a block, or the one statement a non-block body is.
fn statements(program: &Program) -> &[Expr] {
    block_statements(&program.main)
}

fn block_statements(expression: &Expr) -> &[Expr] {
    match expression {
        Expr::Block(statements) => statements,
        single => std::slice::from_ref(single),
    }
}

/// The facts the projector's facet derivation reads for the node at `path`.
///
/// These witnesses carry names the host cannot resolve, which is exactly what
/// they exist to prove the canonical walk still visits. The lens's canonical
/// text is TypeScript, whose front-end refuses an unknown binding at parse, so
/// no projected graph can carry them: they read the analysis the projector
/// reads instead of a graph (FIG-3033).
fn statement_facts<'a>(
    analysis: &'a crate::WorkflowLinkAnalysis,
    expression: &Expr,
    path: &AstPath,
) -> &'a super::WorkflowLinkNodeFacts {
    // The projector peels a label before deriving facets, so an annotated
    // statement's facts live on the expression the label carries.
    let annotated_path = match expression {
        Expr::LabelAnnotated { .. } => Some(path.child(0)),
        _ => None,
    };
    annotated_path
        .and_then(|path| analysis.facts_for(&path))
        .or_else(|| analysis.facts_for(path))
        .expect("the canonical walk records facts for every statement it visits")
}

fn assert_available_type(facts: &super::WorkflowLinkNodeFacts, name: &str, expected: &TypeExpr) {
    assert_eq!(
        facts.available_variables.get(name),
        Some(expected),
        "{name} did not have type {expected:?}"
    );
}

fn assert_unknown_child(
    analysis: &crate::WorkflowLinkAnalysis,
    base_path: &AstPath,
    statements: &[Expr],
) {
    assert!(
        statements
            .iter()
            .enumerate()
            .all(|(index, _)| { analysis.facts_for(&base_path.child(index as u32)).is_some() })
    );
    assert!(statements.iter().enumerate().any(|(index, statement)| {
        statement_facts(analysis, statement, &base_path.child(index as u32))
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "unknown_name")
    }));
}

#[test]
fn invalid_control_headers_keep_nested_facets_and_restore_the_outer_scope() {
    let environment = full_host_environment();

    // value = 1
    // if missing { value = "then"; child = unknown } else { value = "else" }
    // after = value
    let program = builders::program(vec![
        builders::assign("value", builders::num(1.0)),
        builders::if_else(
            builders::var("missing"),
            builders::block(vec![
                builders::assign("value", builders::string("then")),
                builders::assign("child", builders::var("unknown")),
            ]),
            builders::block(vec![builders::assign("value", builders::string("else"))]),
        ),
        builders::assign("after", builders::var("value")),
    ]);
    assert!(matches!(
        LinkedModule::link(program.clone(), environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));
    let analysis = analyze_workflow_program(&program, &environment);
    let nodes = statements(&program);
    assert!(
        statement_facts(&analysis, &nodes[1], &AstPath::main(vec![1]))
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "unknown_name")
    );
    let Expr::If {
        then_block,
        else_block,
        ..
    } = &nodes[1]
    else {
        panic!("expected an if")
    };
    assert_unknown_child(
        &analysis,
        &AstPath::main(vec![1, 1]),
        block_statements(then_block),
    );
    assert!(
        block_statements(else_block)
            .iter()
            .enumerate()
            .all(|(index, _)| analysis
                .facts_for(&AstPath::main(vec![1, 2, index as u32]))
                .is_some())
    );
    assert_available_type(
        statement_facts(&analysis, &nodes[2], &AstPath::main(vec![2])),
        "value",
        &TypeExpr::Int,
    );

    // value = 1
    // while missing { value = "loop"; child = unknown }
    // after = value
    let program = builders::program(vec![
        builders::assign("value", builders::num(1.0)),
        builders::while_loop(
            builders::var("missing"),
            builders::block(vec![
                builders::assign("value", builders::string("loop")),
                builders::assign("child", builders::var("unknown")),
            ]),
        ),
        builders::assign("after", builders::var("value")),
    ]);
    assert!(matches!(
        LinkedModule::link(program.clone(), environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));
    let analysis = analyze_workflow_program(&program, &environment);
    let nodes = statements(&program);
    assert!(
        statement_facts(&analysis, &nodes[1], &AstPath::main(vec![1]))
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "unknown_name")
    );
    let Expr::While { body, .. } = &nodes[1] else {
        panic!("expected a while")
    };
    assert_unknown_child(
        &analysis,
        &AstPath::main(vec![1, 1]),
        block_statements(body),
    );
    assert_available_type(
        statement_facts(&analysis, &nodes[2], &AstPath::main(vec![2])),
        "value",
        &TypeExpr::Int,
    );

    // item = "outer"
    // for item in 1 { seen = item; child = unknown }
    // after = item
    let program = builders::program(vec![
        builders::assign("item", builders::string("outer")),
        builders::for_in(
            "item",
            builders::num(1.0),
            builders::block(vec![
                builders::assign("seen", builders::var("item")),
                builders::assign("child", builders::var("unknown")),
            ]),
        ),
        builders::assign("after", builders::var("item")),
    ]);
    assert!(matches!(
        LinkedModule::link(program.clone(), environment.clone()),
        Err(LinkError::IncompatibleIterationTarget { .. })
    ));
    let analysis = analyze_workflow_program(&program, &environment);
    let nodes = statements(&program);
    assert!(
        statement_facts(&analysis, &nodes[1], &AstPath::main(vec![1]))
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "incompatible_iteration_target")
    );
    let Expr::For { body, .. } = &nodes[1] else {
        panic!("expected a for")
    };
    let body = block_statements(body);
    assert_unknown_child(&analysis, &AstPath::main(vec![1, 1]), body);
    assert_available_type(
        statement_facts(&analysis, &body[0], &AstPath::main(vec![1, 1, 0])),
        "item",
        &TypeExpr::Any,
    );
    assert_available_type(
        statement_facts(&analysis, &nodes[2], &AstPath::main(vec![2])),
        "item",
        &TypeExpr::Str,
    );
}

/// A recovered diagnostic lands on the statement that owns the invalid
/// expression, not on the expression itself.
///
/// The owner is the statement the projector makes a node from, so this is the
/// property that decides which node a host sees the error on. Source offsets
/// are supplied separately by the source view.
#[test]
fn recovered_diagnostics_follow_the_workflow_projection_owner() {
    let environment = full_label_environment();

    let conditional = || {
        builders::if_else(
            builders::var("missing"),
            builders::num(1.0),
            builders::num(2.0),
        )
    };
    for (source, assigned) in [
        ("value = missing ? 1 : 2", conditional()),
        (
            "value = [missing ? 1 : 2]",
            builders::list(vec![conditional()]),
        ),
        (
            "value = { choice: missing ? 1 : 2 }",
            builders::record(vec![("choice", conditional())]),
        ),
    ] {
        let program = builders::with_source_spans(
            builders::program(vec![builders::assign("value", assigned)]),
            &[(&[0], 0, source.len())],
        );
        let analysis = analyze_workflow_program(&program, &environment);
        let owner = statement_facts(&analysis, &statements(&program)[0], &AstPath::main(vec![0]));
        assert_eq!(
            owner.diagnostics.len(),
            1,
            "unexpected diagnostics for {source}"
        );
        assert_eq!(owner.diagnostics[0].error.kind(), "unknown_name");
        assert!(owner.diagnostics[0].error.to_string().contains("missing"));
        assert!(matches!(
            LinkedModule::link(program, environment.clone()),
            Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
        ));
    }

    let print_program = builders::program(vec![builders::print(builders::if_else(
        builders::var("missing"),
        builders::num(1.0),
        builders::num(2.0),
    ))]);
    let analysis = analyze_workflow_program(&print_program, &environment);
    assert!(
        statement_facts(
            &analysis,
            &statements(&print_program)[0],
            &AstPath::main(vec![0])
        )
        .diagnostics
        .is_empty()
    );
    assert!(matches!(
        LinkedModule::link(print_program, environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));

    for (source, statement) in [
        (
            "@label(title: \"Guard\")\nif missing { seen = 1 } else { seen = 2 }",
            builders::labelled(
                builders::label("Guard", None),
                builders::if_else(
                    builders::var("missing"),
                    builders::block(vec![builders::assign("seen", builders::num(1.0))]),
                    builders::block(vec![builders::assign("seen", builders::num(2.0))]),
                ),
            ),
        ),
        (
            "@label(title: \"Choice\")\nvalue = [missing ? 1 : 2]",
            builders::labelled(
                builders::label("Choice", None),
                builders::assign(
                    "value",
                    builders::list(vec![builders::if_else(
                        builders::var("missing"),
                        builders::num(1.0),
                        builders::num(2.0),
                    )]),
                ),
            ),
        ),
    ] {
        let program = builders::program(vec![statement]);
        let analysis = analyze_workflow_program(&program, &environment);
        let owner = statement_facts(&analysis, &statements(&program)[0], &AstPath::main(vec![0]));
        assert_eq!(
            owner.diagnostics.len(),
            1,
            "unexpected diagnostics for {source}"
        );
        assert_eq!(owner.diagnostics[0].error.kind(), "unknown_name");
        assert!(owner.diagnostics[0].error.to_string().contains("missing"));
    }
}

#[test]
fn try_keeps_its_compatible_any_binding_while_lowering_its_body() {
    let try_expr = Expr::Try(Box::new(crate::TryExpr {
        body: Box::new(Expr::String("text".into())),
        catch: None,
        finally: None,
    }));
    let empty_program = Program::block(Vec::new());
    let environment = full_host_environment();
    let linker = Linker::new(&empty_program, &environment);
    let mut scope = Scope::new(false, None);
    let (lowered, binding) = linker
        .lower_expr_expected(
            &try_expr,
            &AstPath::main(Vec::new()),
            &mut scope,
            Some(&TypeExpr::Bool),
        )
        .expect("Try retains its pre-cutover Any result contract");
    assert_eq!(lowered, try_expr);
    assert_eq!(binding_type(&binding), TypeExpr::Any);

    let mut program = Program::block(Vec::new());
    program
        .declarations
        .push(Declaration::Function(crate::FunctionDecl {
            name: "try_result".into(),
            params: Vec::new(),
            return_ty: TypeExpr::Bool,
            body: try_expr,
        }));
    LinkedModule::link(program, environment)
        .expect("a Bool-declared function with Try(String) remains accepted");
}

#[test]
fn finally_completion_restores_each_lexical_scope() {
    for completion in [
        Expr::Null,
        Expr::FunctionReturn(Box::new(Expr::Number(1.0))),
        Expr::Throw(Box::new(Expr::String("thrown".into()))),
        Expr::Break,
        Expr::Continue,
    ] {
        let program = builders::program(vec![
            builders::assign("shadow", builders::string("outer")),
            builders::for_in(
                "shadow",
                builders::list(vec![builders::num(1.0)]),
                builders::block(vec![
                    builders::try_expr(
                        builders::block(vec![completion.clone()]),
                        Some(builders::catch(
                            "shadow",
                            builders::block(vec![builders::print(builders::var("shadow"))]),
                        )),
                        Some(builders::block(vec![builders::print(builders::var(
                            "shadow",
                        ))])),
                    ),
                    builders::print(builders::var("shadow")),
                ]),
            ),
            builders::print(builders::var("shadow")),
        ]);
        let analysis = analyze_workflow_program(&program, &full_host_environment());
        // Paths are the canonical lowering paths, independent of facet
        // enumeration order. The catch restores the loop's binding before
        // finally, and the loop restores the outer binding at exit.
        for (path, expected) in [
            (vec![1, 1, 0, 1, 0], TypeExpr::Any),
            (vec![1, 1, 0, 2, 0], TypeExpr::Int),
            (vec![1, 1, 1], TypeExpr::Int),
            (vec![2], TypeExpr::Str),
        ] {
            let facts = analysis
                .facts_for(&AstPath::main(path.clone()))
                .unwrap_or_else(|| panic!("missing {path:?} for {completion:?}"));
            assert_eq!(
                facts.available_variables.get("shadow"),
                Some(&expected),
                "{path:?} / {completion:?}"
            );
            assert!(
                facts.diagnostics.is_empty(),
                "{path:?}: {:?}",
                facts.diagnostics
            );
        }
    }
}
