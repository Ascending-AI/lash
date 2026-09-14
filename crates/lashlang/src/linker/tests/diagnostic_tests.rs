use super::*;

/// `process scan() { finish missing }`
fn unknown_name_in_process_body() -> Program {
    builders::module(
        vec![builders::process(
            "scan",
            Vec::new(),
            builders::block(vec![builders::finish(builders::var("missing"))]),
        )],
        Vec::new(),
    )
}

// --- behaviour-pinning tests for the single linking walk -------------
//
// These lock in the error *set*, *ordering*, and *spans* the linker
// produced when validation and lowering were two separate passes, so the
// fold into one walk stays behaviour-preserving.

#[test]
fn declaration_errors_report_before_main_errors() {
    // The process body references an unknown name AND the main block
    // references a different unknown name. The declaration error must win.
    // process scan() { finish missing_in_body }
    // finish missing_in_main
    let program = builders::module(
        vec![builders::process(
            "scan",
            Vec::new(),
            builders::block(vec![builders::finish(builders::var("missing_in_body"))]),
        )],
        vec![builders::finish(builders::var("missing_in_main"))],
    );
    let err = LinkedModule::link(program, full_host_environment())
        .expect_err("both bodies reference unknowns");
    assert!(
        matches!(&err, LinkError::UnknownName { name, .. } if name == "missing_in_body"),
        "{err:?}"
    );
}

#[test]
fn unknown_name_in_process_body_carries_declaration_span() {
    // process scan() { finish missing }
    //
    // The span comes from the caller-supplied declaration table: TypeScript
    // programs do not carry lashlang spans yet (FIG-3065).
    let program = builders::with_declaration_spans(unknown_name_in_process_body(), &[(0, 33)]);
    let err = LinkedModule::link(program, full_host_environment()).expect_err("unknown name");
    let LinkError::UnknownName { name, span } = &err else {
        panic!("expected UnknownName, got {err:?}");
    };
    assert_eq!(name, "missing");
    assert!(span.is_some(), "declaration-body error should carry a span");
}

#[test]
fn unknown_top_level_name_on_line_40_fails_at_link() {
    // value_1 = 1
    // ... 38 more assignments ...
    // finish value_39_typo
    //
    // The renderer needs real spans and TypeScript lowering does not supply
    // them yet (FIG-3065), so the test builds the source text and pins the
    // matching span table itself.
    let mut lines = (1..40)
        .map(|index| format!("value_{index} = {index}"))
        .collect::<Vec<_>>();
    lines.push("finish value_39_typo".to_string());
    let source = lines.join("\n");
    let mut statement_spans = Vec::new();
    let mut offset = 0usize;
    for line in &lines {
        statement_spans.push((offset, offset + line.len()));
        offset += line.len() + 1;
    }
    let typo_start = statement_spans[39].0 + "finish ".len();
    let mut statements = (1..40)
        .map(|index| builders::assign(&format!("value_{index}"), builders::num(f64::from(index))))
        .collect::<Vec<_>>();
    statements.push(builders::finish(builders::var("value_39_typo")));
    let program = builders::with_source_spans(
        builders::with_expression_spans(builders::program(statements), &statement_spans),
        &[(&[39, 0], typo_start, typo_start + "value_39_typo".len())],
    );

    let err = LinkedModule::link(program, full_host_environment())
        .expect_err("top-level typo must fail before execution");
    let LinkError::UnknownName { name, span } = err else {
        panic!("expected UnknownName");
    };
    assert_eq!(name, "value_39_typo");
    let diagnostic = crate::format_link_diagnostic(&source, &LinkError::UnknownName { name, span });
    assert!(diagnostic.contains("--> line 40, column 8"), "{diagnostic}");
}

#[test]
fn live_host_globals_are_known_at_top_level() {
    // finish { saved: persisted, payload: projected }
    let program = builders::program(vec![builders::finish(builders::record(vec![
        ("saved", builders::var("persisted")),
        ("payload", builders::var("projected")),
    ]))]);
    let environment = full_host_environment().with_globals(["persisted", "projected"]);
    LinkedModule::link(program, environment).expect("live host globals should link");
}

#[test]
fn module_operation_calls_win_over_colliding_live_globals() {
    // finish await tools.echo({ value: tools })?
    let program = builders::program(vec![builders::finish(builders::module_call(
        &["tools"],
        "echo",
        vec![builders::record(vec![(
            "value",
            builders::resource(&["tools"]),
        )])],
    ))]);
    let environment = full_host_environment().with_globals(["tools"]);
    LinkedModule::link(program, environment)
        .expect("the exact module operation path should remain callable");
}

#[test]
fn linker_reproduces_full_error_set() {
    // One representative source per error variant that the expression walk
    // is responsible for raising.
    // Unknown names are rejected in both declarations and the main block.
    type ErrorCase = (&'static str, Program, fn(&LinkError) -> bool);
    let cases: Vec<ErrorCase> = vec![
        (
            "process scan() { finish missing }",
            unknown_name_in_process_body(),
            |err| matches!(err, LinkError::UnknownName { name, .. } if name == "missing"),
        ),
        (
            "process scan() { missing[0] = 1 }",
            builders::module(
                vec![builders::process(
                    "scan",
                    Vec::new(),
                    builders::block(vec![builders::assign_path(
                        "missing",
                        vec![builders::index_step(builders::num(0.0))],
                        builders::num(1.0),
                    )]),
                )],
                Vec::new(),
            ),
            |err| matches!(err, LinkError::UnknownName { name, .. } if name == "missing"),
        ),
        (
            "finish not_a_builtin(1)",
            builders::program(vec![builders::finish(builders::builtin(
                "not_a_builtin",
                vec![builders::num(1.0)],
            ))]),
            |err| matches!(err, LinkError::UnknownBuiltin { name, .. } if name == "not_a_builtin"),
        ),
        (
            "x = 1\nfinish x.read_file({})",
            builders::program(vec![
                builders::assign("x", builders::num(1.0)),
                builders::finish(builders::receiver_call(
                    builders::var("x"),
                    "read_file",
                    vec![builders::record(Vec::new())],
                )),
            ]),
            |err| matches!(err, LinkError::UnresolvedReceiver { operation, .. } if operation == "read_file"),
        ),
        (
            "ghost",
            builders::program(vec![builders::finish(builders::process_ref("ghost"))]),
            |err| matches!(err, LinkError::UnknownProcess { name, .. } if name == "ghost"),
        ),
    ];

    for (source, program, predicate) in cases {
        let err = LinkedModule::link(program, full_host_environment())
            .err()
            .unwrap_or_else(|| panic!("{source:?} should fail to link"));
        assert!(predicate(&err), "unexpected error for {source:?}: {err:?}");
    }
}

#[test]
fn unknown_resource_operation_still_rejected_after_receiver_resolves() {
    // process scan(tool: Tools) { finish await tool.does_not_exist({})? }
    let program = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tool", TypeExpr::Ref("Tools".into()))],
            builders::block(vec![builders::finish(builders::unwrap(
                builders::await_expr(builders::receiver_call(
                    builders::var("tool"),
                    "does_not_exist",
                    vec![builders::record(Vec::new())],
                )),
            ))]),
        )],
        Vec::new(),
    );
    let err = LinkedModule::link(program, full_host_environment()).expect_err("operation missing");
    assert!(
        matches!(&err, LinkError::UnknownResourceOperation { operation, .. } if operation == "does_not_exist"),
        "{err:?}"
    );
}

#[test]
fn link_diagnostics_render_deduplicated_operation_hints() {
    let unknown_source = "finish await tools.does_not_exist({})?";
    let unknown = LinkedModule::link(
        builders::program(vec![builders::finish(builders::module_call(
            &["tools"],
            "does_not_exist",
            vec![builders::record(Vec::new())],
        ))]),
        full_host_environment(),
    )
    .expect_err("operation missing");
    let LinkError::UnknownResourceOperation { suggestions, .. } = &unknown else {
        panic!("expected UnknownResourceOperation, got {unknown:?}");
    };
    assert!(
        suggestions.contains(&"tools.echo".to_string()),
        "{suggestions:?}"
    );
    assert!(
        suggestions.contains(&"tools.read_file".to_string()),
        "{suggestions:?}"
    );
    let diagnostic = crate::format_link_diagnostic(unknown_source, &unknown);
    assert!(
        diagnostic.contains("hint: available operations:")
            && diagnostic.contains("`tools.echo`")
            && diagnostic.contains("`tools.read_file`"),
        "{diagnostic}"
    );

    let receiver_source = "value = 1\nfinish await value.echo({})?";
    let receiver = LinkedModule::link(
        builders::program(vec![
            builders::assign("value", builders::num(1.0)),
            builders::finish(builders::unwrap(builders::await_expr(
                builders::receiver_call(
                    builders::var("value"),
                    "echo",
                    vec![builders::record(Vec::new())],
                ),
            ))),
        ]),
        full_host_environment(),
    )
    .expect_err("receiver is not an authority");
    let LinkError::UnresolvedReceiver { suggestions, .. } = &receiver else {
        panic!("expected UnresolvedReceiver, got {receiver:?}");
    };
    assert_eq!(suggestions, &["tools.echo"]);
    let diagnostic = crate::format_link_diagnostic(receiver_source, &receiver);
    assert!(
        diagnostic.contains("hint: use a module authority, e.g. `tools.echo`"),
        "{diagnostic}"
    );
    assert_eq!(diagnostic.matches("tools.echo").count(), 1, "{diagnostic}");
}
