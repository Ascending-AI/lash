use super::*;
use crate::ast::UnaryOp;

/// `process scan(tick: timer.Tick) { finish tick.fired_at }`
fn scan_tick_process() -> Declaration {
    builders::process(
        "scan",
        vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
        builders::block(vec![builders::finish(builders::field(
            builders::var("tick"),
            "fired_at",
        ))]),
    )
}

/// `await triggers.register({ source: <source>, target: scan, inputs: { tick: trigger.event } })?`
fn register_scan_trigger(source: Expr) -> Expr {
    triggers_call(
        "register",
        vec![
            ("source", source),
            ("target", builders::var("scan")),
            ("inputs", builders::record(vec![("tick", trigger_event())])),
        ],
    )
}

fn assert_link_and_facet_binding(expr: Expr, expected: TypeExpr) {
    let surface = full_host_environment();
    let empty_program = Program::block(Vec::new());
    let linker = Linker::new(&empty_program, &surface);
    let mut scope = Scope::new(false, None);
    for name in &surface.globals {
        scope.bind(name, any_binding());
    }
    let (_, binding) = linker
        .lower_expr(&expr, &mut scope)
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
    assert_eq!(
        analysis
            .facts_for(&nodes[1])
            .and_then(|facts| facts.available_variables.get("value")),
        Some(&expected),
    );
}

#[test]
fn canonical_walk_aligns_javascript_and_map_bindings_with_facets() {
    for (op, expected) in [
        (crate::JavaScriptUnaryOp::Not, TypeExpr::Bool),
        (crate::JavaScriptUnaryOp::TypeOf, TypeExpr::Str),
        (crate::JavaScriptUnaryOp::Plus, TypeExpr::Float),
        (crate::JavaScriptUnaryOp::Negate, TypeExpr::Float),
    ] {
        assert_link_and_facet_binding(
            Expr::JavaScriptUnary {
                op,
                expr: Box::new(Expr::Number(1.0)),
            },
            expected,
        );
    }

    for op in [
        crate::JavaScriptBinaryOp::StrictEqual,
        crate::JavaScriptBinaryOp::StrictNotEqual,
        crate::JavaScriptBinaryOp::LooseEqual,
        crate::JavaScriptBinaryOp::LooseNotEqual,
        crate::JavaScriptBinaryOp::Less,
        crate::JavaScriptBinaryOp::LessEqual,
        crate::JavaScriptBinaryOp::Greater,
        crate::JavaScriptBinaryOp::GreaterEqual,
    ] {
        assert_link_and_facet_binding(
            Expr::JavaScriptBinary {
                left: Box::new(Expr::Number(1.0)),
                op,
                right: Box::new(Expr::Number(2.0)),
            },
            TypeExpr::Bool,
        );
    }

    assert_link_and_facet_binding(
        Expr::JavaScriptBinary {
            left: Box::new(Expr::Number(1.0)),
            op: crate::JavaScriptBinaryOp::Add,
            right: Box::new(Expr::Number(2.0)),
        },
        TypeExpr::Any,
    );

    assert_link_and_facet_binding(
        Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(1.0)])),
            function: Box::new(Expr::Function(Box::new(crate::FunctionExpr {
                name: None,
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
            builders::unary(UnaryOp::Negate, builders::var("missing")),
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

        let facts = statement_facts(&analysis, &statements(&program)[0]);
        assert!(
            facts.diagnostics.iter().any(|diagnostic| {
                diagnostic.error.kind() == "unknown_name"
                    && diagnostic.error.to_string().contains("missing")
            }),
            "the recovered diagnostic must reach the owning statement for {source}"
        );
    }
}

#[test]
fn default_trigger_key_rejects_a_shadowed_comprehension_source() {
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // sources = [timer.Schedule({ expr: "0 9 * * *" })]
    // registrations = [await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event }
    // })? for source in sources]
    // finish registrations
    let program = builders::module(
        vec![scan_tick_process()],
        vec![
            builders::assign("source", timer_schedule("0 8 * * *")),
            builders::assign("sources", builders::list(vec![timer_schedule("0 9 * * *")])),
            builders::assign(
                "registrations",
                builders::comprehension(
                    register_scan_trigger(builders::var("source")),
                    vec![builders::comprehension_for(
                        "source",
                        builders::var("sources"),
                    )],
                ),
            ),
            builders::finish(builders::var("registrations")),
        ],
    );

    assert!(matches!(
        LinkedModule::link(program, full_host_environment()),
        Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey { .. })
    ));
}

#[test]
fn default_trigger_key_uses_the_pre_try_scope_in_the_catch_path() {
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // try { source = timer.Schedule({ expr: "0 9 * * *" }) }
    // catch error { await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event }
    // })? }
    //
    // The catch body must derive its key from the pre-try binding, so the
    // shadowing assignment lives inside the try body.
    let program = builders::module(
        vec![scan_tick_process()],
        vec![
            builders::assign("source", timer_schedule("0 8 * * *")),
            builders::try_expr(
                builders::assign("source", timer_schedule("0 9 * * *")),
                Some(builders::catch(
                    "error",
                    register_scan_trigger(builders::var("source")),
                )),
                None,
            ),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("catch path resolves source from its lowering scope");
    let source = serde_json::json!({ "expr": "0 8 * * *" });
    let source_key = semantic_trigger_source_key("timer.Schedule", &source);
    let expected_key = semantic_trigger_subscription_key("scan", "timer.Schedule", &source_key);
    let mut actual_key = None;
    struct KeyVisitor<'key>(&'key mut Option<String>);
    impl crate::ExprVisitor for KeyVisitor<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::ReceiverCall {
                operation, args, ..
            } = expr
                && operation.as_str() == crate::TriggerHostOperation::Register.receiver_method()
                && let Ok(call) = crate::register_call_args(args)
                && let Some(Expr::String(key)) = call.subscription_key
            {
                *self.0 = Some(key.to_string());
            }
            crate::walk_expr(self, expr);
        }
    }
    crate::walk_expr(&mut KeyVisitor(&mut actual_key), &linked.program().main);
    assert_eq!(actual_key.as_deref(), Some(expected_key.as_str()));
}

fn registration_call(expr: &Expr) -> (&Expr, &[Expr]) {
    let mut expr = expr;
    while let Expr::Await(inner) | Expr::ResultUnwrap(inner) = expr {
        expr = inner;
    }
    let Expr::ReceiverCall {
        receiver,
        operation,
        args,
    } = expr
    else {
        panic!("expected a registration call, got {expr:?}")
    };
    assert_eq!(
        operation.as_str(),
        crate::TriggerHostOperation::Register.receiver_method()
    );
    (receiver, args)
}

fn registration_key(expr: &Expr) -> &str {
    let (_, args) = registration_call(expr);
    let call = crate::register_call_args(args).expect("lowered registration remains valid");
    let Some(Expr::String(key)) = call.subscription_key else {
        panic!("lowered registration has no literal key")
    };
    key.as_str()
}

#[test]
fn assignment_indexes_retain_lowering_and_their_own_trigger_keys_in_evaluation_order() {
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // items[await triggers.register({
    //   source: timer.Schedule({ expr: "0 8 * * *" }), target: scan,
    //   inputs: { tick: trigger.event }
    // })?] = await triggers.register({
    //   source: timer.Schedule({ expr: "0 9 * * *" }), target: scan,
    //   inputs: { tick: trigger.event }
    // })?
    // await triggers.register({
    //   source: timer.Schedule({ expr: "0 10 * * *" }), target: scan,
    //   inputs: { tick: trigger.event }
    // })?
    let register_at = |expr: &str| register_scan_trigger(timer_schedule(expr));
    let program = builders::module(
        vec![scan_tick_process()],
        vec![
            builders::assign_path(
                "items",
                vec![builders::index_step(register_at("0 8 * * *"))],
                register_at("0 9 * * *"),
            ),
            register_at("0 10 * * *"),
        ],
    );
    let linked = LinkedModule::link(program, full_host_environment().with_globals(["items"]))
        .expect("every registration keeps its own derived key");
    let Expr::Block(main) = &linked.program().main else {
        unreachable!("parsed main is a block")
    };
    let [Expr::Assign { target, expr: rhs }, subsequent] = main.as_slice() else {
        panic!("unexpected lowered main: {main:?}")
    };
    let [AssignPathStep::Index(index)] = target.steps.as_slice() else {
        panic!("dynamic assignment index was not retained: {target:?}")
    };
    let (index_receiver, _) = registration_call(index);
    assert!(matches!(index_receiver, Expr::ResourceRef(_)));

    let expected = ["0 8 * * *", "0 9 * * *", "0 10 * * *"].map(|expr| {
        let source_key =
            semantic_trigger_source_key("timer.Schedule", &serde_json::json!({ "expr": expr }));
        semantic_trigger_subscription_key("scan", "timer.Schedule", &source_key)
    });
    assert_eq!(registration_key(index), expected[0]);
    assert_eq!(registration_key(rhs), expected[1]);
    assert_eq!(registration_key(subsequent), expected[2]);

    struct OrderedKeys(Vec<String>);
    impl crate::ExprVisitor for OrderedKeys {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::ReceiverCall { operation, .. } = expr
                && operation.as_str() == crate::TriggerHostOperation::Register.receiver_method()
            {
                self.0.push(registration_key(expr).to_string());
            }
            crate::walk_expr(self, expr);
        }
    }
    let mut ordered = OrderedKeys(Vec::new());
    crate::walk_expr(&mut ordered, &linked.program().main);
    assert_eq!(ordered.0, expected);
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

/// The facts the projector's facet derivation reads for one expression.
///
/// These witnesses carry names the host cannot resolve, which is exactly what
/// they exist to prove the canonical walk still visits. The lens's canonical
/// text is TypeScript, whose front-end refuses an unknown binding at parse, so
/// no projected graph can carry them: they read the analysis the projector
/// reads instead of a graph (FIG-3033).
fn statement_facts<'a>(
    analysis: &'a crate::WorkflowLinkAnalysis,
    expression: &Expr,
) -> &'a super::WorkflowLinkNodeFacts {
    // The projector peels a label before deriving facets, so an annotated
    // statement's facts live on the expression the label carries.
    let annotated = match expression {
        Expr::LabelAnnotated { expr, .. } => Some(expr.as_ref()),
        _ => None,
    };
    annotated
        .and_then(|expr| analysis.facts_for(expr))
        .or_else(|| analysis.facts_for(expression))
        .expect("the canonical walk records facts for every statement it visits")
}

fn assert_available_type(facts: &super::WorkflowLinkNodeFacts, name: &str, expected: &TypeExpr) {
    assert_eq!(
        facts.available_variables.get(name),
        Some(expected),
        "{name} did not have type {expected:?}"
    );
}

fn assert_unknown_child(analysis: &crate::WorkflowLinkAnalysis, statements: &[Expr]) {
    assert!(
        statements
            .iter()
            .all(|statement| analysis.facts_for(statement).is_some())
    );
    assert!(statements.iter().any(|statement| {
        statement_facts(analysis, statement)
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
        statement_facts(&analysis, &nodes[1])
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
    assert_unknown_child(&analysis, block_statements(then_block));
    assert!(
        block_statements(else_block)
            .iter()
            .all(|statement| analysis.facts_for(statement).is_some())
    );
    assert_available_type(
        statement_facts(&analysis, &nodes[2]),
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
        statement_facts(&analysis, &nodes[1])
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "unknown_name")
    );
    let Expr::While { body, .. } = &nodes[1] else {
        panic!("expected a while")
    };
    assert_unknown_child(&analysis, block_statements(body));
    assert_available_type(
        statement_facts(&analysis, &nodes[2]),
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
        statement_facts(&analysis, &nodes[1])
            .diagnostics
            .iter()
            .any(|error| error.error.kind() == "incompatible_iteration_target")
    );
    let Expr::For { body, .. } = &nodes[1] else {
        panic!("expected a for")
    };
    let body = block_statements(body);
    assert_unknown_child(&analysis, body);
    assert_available_type(statement_facts(&analysis, &body[0]), "item", &TypeExpr::Any);
    assert_available_type(
        statement_facts(&analysis, &nodes[2]),
        "item",
        &TypeExpr::Str,
    );
}

/// A recovered diagnostic lands on the statement that owns the invalid
/// expression, not on the expression itself.
///
/// The owner is the statement the projector makes a node from, so this is the
/// property that decides which node a host sees the error on. The witnesses
/// carry an unresolvable name and so have no TypeScript spelling; the owner
/// relation is read off the analysis the projector reads. The projector-side
/// half — that the diagnostic's span is the owning node's `source_span` — has
/// no TypeScript witness at all and is not proved here (FIG-3033).
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
        // The statement's own span is stated, since the recovered diagnostic
        // falls back to the span of the statement that owns the expression.
        let program = builders::with_source_spans(
            builders::program(vec![builders::assign("value", assigned)]),
            &[(&[0], 0, source.len())],
        );
        let analysis = analyze_workflow_program(&program, &environment);
        let owner = statement_facts(&analysis, &statements(&program)[0]);
        assert_eq!(
            owner.diagnostics.len(),
            1,
            "unexpected diagnostics for {source}"
        );
        assert_eq!(owner.diagnostics[0].error.kind(), "unknown_name");
        assert!(owner.diagnostics[0].error.to_string().contains("missing"));
        assert!(owner.diagnostics[0].span.is_some());
        assert!(matches!(
            LinkedModule::link(program, environment.clone()),
            Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
        ));
    }

    // print (missing ? 1 : 2)
    let print_program = builders::program(vec![builders::print(builders::if_else(
        builders::var("missing"),
        builders::num(1.0),
        builders::num(2.0),
    ))]);
    let analysis = analyze_workflow_program(&print_program, &environment);
    assert!(
        statement_facts(&analysis, &statements(&print_program)[0])
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
        let owner = statement_facts(&analysis, &statements(&program)[0]);
        assert_eq!(
            owner.diagnostics.len(),
            1,
            "unexpected diagnostics for {source}"
        );
        assert_eq!(owner.diagnostics[0].error.kind(), "unknown_name");
        assert!(owner.diagnostics[0].error.to_string().contains("missing"));
    }

    // value = missing
    //     ? timer.Schedule({ expr: "0 8 * * *" })
    //     : timer.Schedule({ expr: "0 9 * * *" })
    let program = builders::program(vec![builders::assign(
        "value",
        builders::if_else(
            builders::var("missing"),
            timer_schedule("0 8 * * *"),
            timer_schedule("0 9 * * *"),
        ),
    )]);
    let analysis = analyze_workflow_program(&program, &environment);
    let owner = statement_facts(&analysis, &statements(&program)[0]);
    assert_eq!(owner.diagnostics.len(), 1);
    assert_eq!(owner.diagnostics[0].error.kind(), "unknown_name");
    assert_eq!(owner.expected_arguments.len(), 4);
    assert_eq!(
        owner
            .expected_arguments
            .iter()
            .filter(|argument| {
                argument.slot.ends_with("arg[0].expr") && argument.ty == TypeExpr::Str
            })
            .count(),
        2
    );
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
        .lower_expr_expected(&try_expr, &mut scope, Some(&TypeExpr::Bool))
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
