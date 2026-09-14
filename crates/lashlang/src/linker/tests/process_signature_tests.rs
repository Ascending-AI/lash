use super::*;

/// `process scan(event: timer.Tick) -> bool { finish true }`
fn scan_process() -> Declaration {
    builders::process_returning(
        "scan",
        vec![builders::param("event", TypeExpr::Ref("timer.Tick".into()))],
        TypeExpr::Bool,
        builders::block(vec![builders::finish(builders::bool_lit(true))]),
    )
}

/// `Process<(<param>: timer.Tick), bool>`
fn handler_type(param: &str) -> TypeExpr {
    builders::process_type(
        vec![builders::param(param, TypeExpr::Ref("timer.Tick".into()))],
        TypeExpr::Bool,
    )
}

#[test]
fn trigger_target_uses_the_scoped_callable_when_it_shadows_a_declaration() {
    // process scan(event: timer.Tick) -> bool { finish true }
    // process install(scan: Process<(payload: timer.Tick), bool>) -> bool {
    //   source = timer.Schedule({ expr: "0 8 * * *" })
    //   await triggers.register({
    //     source: source,
    //     subscription_key: <key>,
    //     target: scan,
    //     inputs: { <input>: trigger.event }
    //   })?
    //   finish true
    // }
    let shadowed = |key: &str, input: &str| {
        builders::module(
            vec![
                scan_process(),
                builders::process_returning(
                    "install",
                    vec![builders::param("scan", handler_type("payload"))],
                    TypeExpr::Bool,
                    builders::block(vec![
                        builders::assign("source", timer_schedule("0 8 * * *")),
                        triggers_call(
                            "register",
                            vec![
                                ("source", builders::var("source")),
                                ("subscription_key", builders::string(key)),
                                ("target", builders::var("scan")),
                                ("inputs", builders::record(vec![(input, trigger_event())])),
                            ],
                        ),
                        builders::finish(builders::bool_lit(true)),
                    ]),
                ),
            ],
            Vec::new(),
        )
    };

    let wrong_name = shadowed("shadowed-wrong-name", "event");
    assert!(matches!(
        LinkedModule::link(wrong_name, full_host_environment()),
        Err(LinkError::UnknownTriggerInput { ref input, .. }) if input == "event"
    ));

    let correct_name = shadowed("shadowed-correct-name", "payload");
    LinkedModule::link(correct_name, full_host_environment())
        .expect("scoped process parameter signature is authoritative");
}

#[test]
fn trigger_list_accepts_same_signature_alias_branch_targets() {
    // type Handler = Process<(event: timer.Tick), bool>
    // process scan(event: timer.Tick) -> bool { finish true }
    // process install(handler: Handler) -> bool {
    //   selected = handler
    //   if true { selected = scan } else { selected = handler }
    //   await triggers.list({ target: selected })?
    //   finish true
    // }
    let program = builders::module(
        vec![
            builders::type_decl("Handler", handler_type("event")),
            scan_process(),
            builders::process_returning(
                "install",
                vec![builders::param("handler", TypeExpr::Ref("Handler".into()))],
                TypeExpr::Bool,
                builders::block(vec![
                    builders::assign("selected", builders::var("handler")),
                    builders::if_else(
                        builders::bool_lit(true),
                        builders::block(vec![builders::assign("selected", builders::var("scan"))]),
                        builders::block(vec![builders::assign(
                            "selected",
                            builders::var("handler"),
                        )]),
                    ),
                    triggers_call("list", vec![("target", builders::var("selected"))]),
                    builders::finish(builders::bool_lit(true)),
                ]),
            ),
        ],
        Vec::new(),
    );
    LinkedModule::link(program, full_host_environment())
        .expect("list uses the same normalized process target contract as registration");
}
