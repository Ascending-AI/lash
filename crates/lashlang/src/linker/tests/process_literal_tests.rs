use super::*;

/// `await crew.run({ program: async (tick) => { ... }, inputs: { tick: ... } })?`
fn crew_run(program: Expr) -> Expr {
    builders::unwrap(builders::await_expr(builders::module_call(
        &["crew"],
        "run",
        vec![builders::record(vec![
            ("program", program),
            (
                "inputs",
                builders::record(vec![("tick", builders::string("0 8 * * *"))]),
            ),
        ])],
    )))
}

/// The `process` names `program`'s declarations carry, in walk order.
fn process_names(program: &Program) -> Vec<String> {
    program
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            Declaration::Process(process) => Some(process.name.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn lift_processes_a_literal_in_a_process_typed_slot() {
    // The literal carries its parameter annotations, so the lifted
    // declaration's signature is typed and the tool's contract accepts it.
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handle",
                crew_run(builders::process_literal(
                    vec![builders::param("tick", TypeExpr::Any)],
                    builders::finish(builders::string("done")),
                )),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("a literal lifts where the slot expects a process");
    let names = process_names(linked.program());
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(
        names[0].starts_with(crate::LIFTED_PROCESS_NAME_PREFIX),
        "lifted names are linker-invented: {names:?}"
    );
}

#[test]
fn lifted_literals_do_not_lower_process_parameters_to_any() {
    // A `str` annotation on the literal's parameter reaches the lifted
    // declaration's signature: the gap where TypeScript process parameters
    // widened to `Any` is closed by the node the lift reads.
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handle",
                crew_run(builders::process_literal(
                    vec![builders::param("tick", TypeExpr::Str)],
                    builders::finish(builders::field(builders::var("tick"), "fired_at")),
                )),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("an annotated parameter is carried into the signature");
    let Some(Declaration::Process(process)) = process_names_first(&linked) else {
        unreachable!("the module lifted exactly one process");
    };
    assert_eq!(
        process
            .params
            .first()
            .map(|param| (param.name.as_str(), &param.ty)),
        Some(("tick", &TypeExpr::Str)),
        "the annotation reaches the lifted signature"
    );
}

fn process_names_first(linked: &LinkedModule) -> Option<&Declaration> {
    linked
        .program()
        .declarations
        .iter()
        .find(|declaration| matches!(declaration, Declaration::Process(_)))
}

#[test]
fn relinking_the_same_lift_lifts_the_same_process_ref() {
    fn lifted() -> String {
        let program = builders::module(
            vec![],
            vec![
                builders::assign(
                    "handle",
                    crew_run(builders::process_literal(
                        vec![builders::param("tick", TypeExpr::Any)],
                        builders::finish(builders::string("done")),
                    )),
                ),
                builders::finish(builders::var("handle")),
            ],
        );
        let linked = LinkedModule::link(program, full_host_environment())
            .expect("a literal lifts where the slot expects a process");
        struct Ref(BTreeSet<String>);
        impl crate::ExprVisitor for Ref {
            fn visit_expr(&mut self, expr: &Expr) {
                if let Expr::ProcessRef { process } = expr
                    && process.starts_with(crate::LIFTED_PROCESS_NAME_PREFIX)
                {
                    self.0.insert(process.to_string());
                }
                crate::walk_expr(self, expr);
            }
        }
        let mut refs = Ref(BTreeSet::new());
        crate::walk_expr(&mut refs, &linked.program().main);
        refs.0.into_iter().next().expect("one lifted ref")
    }

    let first = lifted();
    let second = lifted();
    assert_eq!(
        first, second,
        "re-linking the same cell must resolve the same process"
    );
}

#[test]
fn a_literal_in_a_non_process_slot_is_a_type_error_naming_the_slot() {
    let program = builders::module(
        vec![],
        vec![builders::unwrap(builders::await_expr(
            builders::module_call(
                &["tools"],
                "accept_str",
                vec![builders::process_literal(
                    vec![],
                    builders::finish(builders::string("done")),
                )],
            ),
        ))],
    );

    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("a non-process slot refuses a literal");
    assert!(
        matches!(
            error,
            LinkError::ProcessLiteralOutsideProcessSlot { ref expected, .. }
                if expected == "str"
        ),
        "{error}"
    );
}

#[test]
fn a_const_bound_literal_lifts_the_same_way() {
    // handler = async (tick) => { ... }
    // handle = await triggers.register({
    //   source: timer.Schedule({ expr: "0 8 * * *" }),
    //   target: handler,
    //   inputs: { tick: trigger.event }
    // })?
    // The binding is the slot: the literal lifts at the binding and reads of
    // the name carry the process value.
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handler",
                builders::process_literal(
                    vec![builders::param("tick", TypeExpr::Any)],
                    builders::finish(builders::string("done")),
                ),
            ),
            builders::assign("source", timer_schedule("0 8 * * *")),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("handler")),
                    ("inputs", builders::record(vec![("tick", trigger_event())])),
                ],
            ),
            builders::finish(builders::string("ok")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("a const-bound literal lifts at the binding");
    let names = process_names(linked.program());
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn a_const_bound_literal_lifts_when_the_session_already_carries_the_name() {
    // The second turn of a session that bound a process literal on the first:
    // the name is carried as a session global, which `pass_setup` seeds into
    // the root scope as `any`. That inferred type is not a declaration, so the
    // binding is still the process slot and the same literal still lifts —
    // otherwise a cell would link on turn 1 and be refused on every turn after
    // (FIG-3120: /workspace/notes/lash/perf-guard-0915/runtime-diagnosis.md).
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handler",
                builders::process_literal(
                    vec![builders::param("tick", TypeExpr::Any)],
                    builders::finish(builders::string("done")),
                ),
            ),
            builders::finish(builders::string("ok")),
        ],
    );

    let carried = full_host_environment().with_globals(["handler"]);
    let linked = LinkedModule::link(program, carried)
        .expect("a re-bound session global is still the process slot");
    let names = process_names(linked.program());
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn a_literal_through_a_path_step_or_shape_mismatch_stays_refused() {
    // A slot that does not carry `Process` refuses, even when the shape is
    // a record containing a process — the membership test is per slot.
    let program = builders::module(
        vec![],
        vec![builders::unwrap(builders::await_expr(
            builders::module_call(
                &["tools"],
                "read_file",
                vec![builders::record(vec![(
                    "path",
                    builders::process_literal(vec![], builders::finish(builders::string("done"))),
                )])],
            ),
        ))],
    );

    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("a str slot refuses a literal");
    assert!(matches!(
        error,
        LinkError::ProcessLiteralOutsideProcessSlot { .. }
    ));
}

#[test]
fn the_signal_set_is_inferred_from_wait_sites_including_unreached_branches() {
    // Structural enumeration, so an unreached branch's "hold" counts too.
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handle",
                crew_run(builders::process_literal(
                    vec![],
                    builders::if_else(
                        builders::bool_lit(true),
                        builders::assign("signal", builders::wait_signal("go")),
                        builders::assign("unused", builders::wait_signal("hold")),
                    ),
                )),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("inferred wait sites are legal");
    let Declaration::Process(process) = process_names_first(&linked).expect("lift happened") else {
        unreachable!("the module lifted exactly one process");
    };
    let names: Vec<&str> = process
        .signals
        .iter()
        .map(|signal| signal.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["go", "hold"],
        "the inferred set is a structural walk: {names:?}"
    );
}

#[test]
fn disjoint_signal_payloads_for_one_name_are_refused() {
    // Two wait sites for one literal name, each awaiting a different shape:
    // `tools.accept_str(<literal>)` types the payload str, the second arm
    // types it integer. Mutually unassignable sites are an error.
    let program = builders::module(
        vec![],
        vec![
            builders::assign(
                "handle",
                crew_run(builders::process_literal(
                    vec![],
                    builders::if_else(
                        builders::bool_lit(true),
                        builders::unwrap(builders::await_expr(builders::module_call(
                            &["tools"],
                            "accept_str",
                            vec![builders::wait_signal("daily")],
                        ))),
                        builders::unwrap(builders::await_expr(builders::module_call(
                            &["tools"],
                            "accept_int",
                            vec![builders::wait_signal("daily")],
                        ))),
                    ),
                )),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("mutually unassignable payload sites conflict");
    assert!(
        matches!(
            error,
            LinkError::ConflictingSignalPayload { ref name, .. } if name == "daily"
        ),
        "{error}"
    );
}

#[test]
fn an_immutable_capture_becomes_a_hidden_start_argument() {
    //    budget = ...
    //    handler = async (tick) => { ... finish(tick.budget_below(budget)) }
    // The lifted signature carries the read's hidden arg, and the prompt
    // teaches that the process sees the value a variable had when it started.
    let program = builders::module(
        vec![],
        vec![
            builders::assign("budget", builders::num(3.0)),
            builders::assign("handler", {
                let mut literal =
                    builders::process_literal(vec![], builders::finish(builders::var("budget")));
                if let Expr::ProcessLiteral(inner) = &mut literal {
                    inner
                        .hidden_args
                        .push(builders::param("budget", TypeExpr::Any));
                }
                literal
            }),
            builders::assign("handle", crew_run(builders::var("handler"))),
            builders::finish(builders::var("handle")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment())
        .expect("a captured immutable local is a hidden start argument");
    let Declaration::Process(process) = process_names_first(&linked).expect("lift happened") else {
        unreachable!("the module lifted exactly one process");
    };
    assert_eq!(
        process.params.last().map(|param| param.name.as_str()),
        Some("budget"),
        "the hidden start argument rides the signature: {:?}",
        process.params
    );
}
