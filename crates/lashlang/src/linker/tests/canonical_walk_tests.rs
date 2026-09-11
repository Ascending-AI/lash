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
    let program = crate::parse(
        r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            items[await triggers.register({
              source: timer.Schedule({ expr: "0 8 * * *" }),
              target: scan,
              inputs: { tick: trigger.event }
            })?] = await triggers.register({
              source: timer.Schedule({ expr: "0 9 * * *" }),
              target: scan,
              inputs: { tick: trigger.event }
            })?
            await triggers.register({
              source: timer.Schedule({ expr: "0 10 * * *" }),
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
    )
    .expect("dynamic assignment-index witness parses");
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

fn assert_available_type(node: &crate::WorkflowNode, name: &str, expected: &TypeExpr) {
    assert!(
        node.type_facets
            .as_ref()
            .expect("sourceable node keeps type facets")
            .available_variables
            .iter()
            .any(|variable| variable.name == name && &variable.ty == expected),
        "{name} did not have type {expected:?} at node {node:?}"
    );
}

fn assert_unknown_child(nodes: &[crate::WorkflowNode]) {
    assert!(nodes.iter().all(|node| node.type_facets.is_some()));
    assert!(nodes.iter().any(|node| {
        node.type_facets.as_ref().is_some_and(|facets| {
            facets
                .diagnostics
                .iter()
                .any(|error| error.kind == "unknown_name")
        })
    }));
}

#[test]
fn invalid_control_headers_keep_nested_facets_and_restore_the_outer_scope() {
    let environment = full_host_environment();

    let if_source = r#"
        value = 1
        if missing { value = "then"; child = unknown } else { value = "else" }
        after = value
    "#;
    assert!(matches!(
        LinkedModule::link(crate::parse(if_source).unwrap(), environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));
    let graph = crate::workflow_graph_from_source_with_facets(if_source, Some(&environment))
        .expect("invalid if still projects");
    let if_node = &graph.main.nodes[1];
    assert!(if_node.type_facets.as_ref().is_some_and(|facets| {
        facets
            .diagnostics
            .iter()
            .any(|error| error.kind == "unknown_name")
    }));
    let crate::WorkflowNodeKind::Container(crate::WorkflowContainer::If {
        then_graph,
        else_graph,
        ..
    }) = &if_node.kind
    else {
        panic!("expected if container")
    };
    assert_unknown_child(&then_graph.nodes);
    assert!(
        else_graph
            .nodes
            .iter()
            .all(|node| node.type_facets.is_some())
    );
    assert_available_type(&graph.main.nodes[2], "value", &TypeExpr::Int);

    let while_source = r#"
        value = 1
        while missing { value = "loop"; child = unknown }
        after = value
    "#;
    assert!(matches!(
        LinkedModule::link(crate::parse(while_source).unwrap(), environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));
    let graph = crate::workflow_graph_from_source_with_facets(while_source, Some(&environment))
        .expect("invalid while still projects");
    let while_node = &graph.main.nodes[1];
    assert!(while_node.type_facets.as_ref().is_some_and(|facets| {
        facets
            .diagnostics
            .iter()
            .any(|error| error.kind == "unknown_name")
    }));
    let crate::WorkflowNodeKind::Container(crate::WorkflowContainer::While { body, .. }) =
        &while_node.kind
    else {
        panic!("expected while container")
    };
    assert_unknown_child(&body.nodes);
    assert_available_type(&graph.main.nodes[2], "value", &TypeExpr::Int);

    let for_source = r#"
        item = "outer"
        for item in 1 { seen = item; child = unknown }
        after = item
    "#;
    assert!(matches!(
        LinkedModule::link(crate::parse(for_source).unwrap(), environment.clone()),
        Err(LinkError::IncompatibleIterationTarget { .. })
    ));
    let graph = crate::workflow_graph_from_source_with_facets(for_source, Some(&environment))
        .expect("invalid for still projects");
    let for_node = &graph.main.nodes[1];
    assert!(for_node.type_facets.as_ref().is_some_and(|facets| {
        facets
            .diagnostics
            .iter()
            .any(|error| error.kind == "incompatible_iteration_target")
    }));
    let crate::WorkflowNodeKind::Container(crate::WorkflowContainer::For { body, .. }) =
        &for_node.kind
    else {
        panic!("expected for container")
    };
    let body = &body.nodes;
    assert_unknown_child(body);
    assert_available_type(&body[0], "item", &TypeExpr::Any);
    assert_available_type(&graph.main.nodes[2], "item", &TypeExpr::Str);
}

#[test]
fn recovered_diagnostics_follow_the_workflow_projection_owner() {
    let environment = full_label_environment();

    for source in [
        "value = missing ? 1 : 2",
        "value = [missing ? 1 : 2]",
        "value = { choice: missing ? 1 : 2 }",
    ] {
        let graph = crate::workflow_graph_from_source_with_facets(source, Some(&environment))
            .expect("an assigned invalid conditional remains projectable");
        let node = &graph.main.nodes[0];
        let diagnostics = &node
            .type_facets
            .as_ref()
            .expect("host-backed projection has type facets")
            .diagnostics;
        assert_eq!(diagnostics.len(), 1, "unexpected diagnostics for {source}");
        assert_eq!(diagnostics[0].kind, "unknown_name");
        assert!(diagnostics[0].message.contains("missing"));
        assert_eq!(diagnostics[0].span, node.source_span);
        assert!(matches!(
            LinkedModule::link(crate::parse(source).unwrap(), environment.clone()),
            Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
        ));
    }

    let print_source = "print (missing ? 1 : 2)";
    let graph = crate::workflow_graph_from_source_with_facets(print_source, Some(&environment))
        .expect("a printed invalid conditional remains projectable");
    assert!(
        graph.main.nodes[0]
            .type_facets
            .as_ref()
            .expect("host-backed projection has type facets")
            .diagnostics
            .is_empty()
    );
    assert!(matches!(
        LinkedModule::link(crate::parse(print_source).unwrap(), environment.clone()),
        Err(LinkError::UnknownName { ref name, .. }) if name == "missing"
    ));

    for source in [
        "@label(title: \"Guard\")\nif missing { seen = 1 } else { seen = 2 }",
        "@label(title: \"Choice\")\nvalue = [missing ? 1 : 2]",
    ] {
        let graph = crate::workflow_graph_from_source_with_facets(source, Some(&environment))
            .expect("a labeled invalid conditional remains projectable");
        let node = &graph.main.nodes[0];
        let diagnostics = &node
            .type_facets
            .as_ref()
            .expect("labeled projection has type facets")
            .diagnostics;
        assert_eq!(diagnostics.len(), 1, "unexpected diagnostics for {source}");
        assert_eq!(diagnostics[0].kind, "unknown_name");
        assert!(diagnostics[0].message.contains("missing"));
    }

    let call_source = r#"value = missing
        ? timer.Schedule({ expr: "0 8 * * *" })
        : timer.Schedule({ expr: "0 9 * * *" })"#;
    let graph = crate::workflow_graph_from_source_with_facets(call_source, Some(&environment))
        .expect("recovery preserves expected argument facets on the assignment owner");
    let facets = graph.main.nodes[0]
        .type_facets
        .as_ref()
        .expect("the assigned conditional has type facets");
    assert_eq!(facets.diagnostics.len(), 1);
    assert_eq!(facets.diagnostics[0].kind, "unknown_name");
    assert_eq!(facets.expected_arguments.len(), 4);
    assert_eq!(
        facets
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
