use super::*;

fn assert_link_and_facet_binding(expr: Expr, expected: TypeExpr) {
    let surface = full_host_environment();
    let empty_program = Program::block(Vec::new());
    let linker =
        Linker::new(&empty_program, &surface).with_dialect(crate::CompilationDialect::Typescript);
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
    LinkedModule::link_with_dialect(
        program.clone(),
        full_host_environment(),
        crate::CompilationDialect::Typescript,
    )
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
    for source in ["value = [1][missing]", "value = -missing"] {
        let program = crate::parse(source).expect("operand witness parses");
        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
        ));

        let graph =
            crate::workflow_graph_from_source_with_facets(source, Some(&full_host_environment()))
                .expect("facet projection remains best effort");
        let diagnostics = &graph.main.nodes[0]
            .type_facets
            .as_ref()
            .expect("host-backed projection has type facets")
            .diagnostics;
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "unknown_name" && diagnostic.message.contains("missing")
        }));
    }
}

#[test]
fn default_trigger_key_rejects_a_shadowed_comprehension_source() {
    let program = crate::parse(
        r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            sources = [timer.Schedule({ expr: "0 9 * * *" })]
            registrations = [await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })? for source in sources]
            finish registrations
            "#,
    )
    .expect("comprehension trigger witness parses");

    assert!(matches!(
        LinkedModule::link(program, full_host_environment()),
        Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey { .. })
    ));
}

#[test]
fn default_trigger_key_uses_the_pre_try_scope_in_the_catch_path() {
    let mut program = crate::parse(
        r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
    )
    .expect("base trigger witness parses");
    let Expr::Block(main) = &mut program.main else {
        unreachable!("parsed main is a block")
    };
    let register = main.pop().expect("registration expression");
    let outer_source = main.pop().expect("outer source assignment");
    let mut body_source = outer_source.clone();
    let Expr::Assign { expr, .. } = &mut body_source else {
        unreachable!("source setup is an assignment")
    };
    let Expr::ReceiverCall { args, .. } = expr.as_mut() else {
        panic!("unexpected timer source expression: {expr:?}")
    };
    let [Expr::Record(fields)] = args.as_mut_slice() else {
        unreachable!("timer source input is a record")
    };
    fields[0].1 = Expr::String("0 9 * * *".into());
    main.push(outer_source);
    main.push(Expr::Try(Box::new(crate::TryExpr {
        body: Box::new(body_source),
        catch: Some(crate::CatchClause {
            binding: "error".into(),
            body: Box::new(register),
        }),
        finally: None,
    })));

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
