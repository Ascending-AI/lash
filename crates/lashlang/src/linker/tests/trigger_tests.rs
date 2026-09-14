use super::*;

/// `process <name>(<param>: timer.Tick) { finish true }`
fn tick_process(name: &str, param: &str) -> Declaration {
    builders::process(
        name,
        vec![builders::param(param, TypeExpr::Ref("timer.Tick".into()))],
        builders::block(vec![builders::finish(builders::bool_lit(true))]),
    )
}

/// `process <name>(<param>: timer.Tick) { finish <param>.fired_at }`
fn tick_process_returning_fired_at(name: &str, param: &str) -> Declaration {
    builders::process(
        name,
        vec![builders::param(param, TypeExpr::Ref("timer.Tick".into()))],
        builders::block(vec![builders::finish(builders::field(
            builders::var(param),
            "fired_at",
        ))]),
    )
}

/// `<binding> = timer.Schedule({ expr: "0 8 * * *" })`
fn assign_daily_source(binding: &str) -> Expr {
    builders::assign(binding, timer_schedule("0 8 * * *"))
}

/// `await triggers.register({ source: <source>, target: <target>, inputs: { <param>: trigger.event } })?`
fn register_trigger(source: &str, target: &str, param: &str) -> Expr {
    triggers_call(
        "register",
        vec![
            ("source", builders::var(source)),
            ("target", builders::var(target)),
            ("inputs", builders::record(vec![(param, trigger_event())])),
        ],
    )
}

#[test]
fn linked_module_accepts_named_processes_resource_params_and_activations() {
    // type ChangeEvent = { path: str }
    // process scan(tool: Tools, event: ChangeEvent) {
    //   text = await tool.read_file({ path: "changed.txt" })?
    //   finish text
    // }
    // process watcher(run: any) signals { ready: any } {
    //   sleep for "0ms"
    //   signal = wait_signal("ready")
    //   signal_run(run, "ready", signal)
    //   finish signal
    // }
    // process from_tick(tick: timer.Tick) { finish tick.fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
    // handle = await triggers.register({
    //   source: source,
    //   target: from_tick,
    //   inputs: { tick: trigger.event },
    //   name: "changed"
    // })?
    // finish handle
    let program = builders::module(
        vec![
            builders::type_decl(
                "ChangeEvent",
                TypeExpr::Object(vec![builders::type_field("path", TypeExpr::Str, false)]),
            ),
            builders::process(
                "scan",
                vec![
                    builders::param("tool", TypeExpr::Ref("Tools".into())),
                    builders::param("event", TypeExpr::Ref("ChangeEvent".into())),
                ],
                builders::block(vec![
                    builders::assign(
                        "text",
                        builders::unwrap(builders::await_expr(builders::receiver_call(
                            builders::var("tool"),
                            "read_file",
                            vec![builders::record(vec![(
                                "path",
                                builders::string("changed.txt"),
                            )])],
                        ))),
                    ),
                    builders::finish(builders::var("text")),
                ]),
            ),
            builders::process_with_signals(
                "watcher",
                vec![builders::param("run", TypeExpr::Any)],
                vec![builders::signal("ready", TypeExpr::Any)],
                builders::block(vec![
                    builders::sleep_for(builders::string("0ms")),
                    builders::assign("signal", builders::wait_signal("ready")),
                    builders::signal_run(builders::var("run"), "ready", builders::var("signal")),
                    builders::finish(builders::var("signal")),
                ]),
            ),
            tick_process_returning_fired_at("from_tick", "tick"),
        ],
        vec![
            builders::assign(
                "source",
                builders::receiver_call(
                    builders::resource(&["timer"]),
                    "Schedule",
                    vec![builders::record(vec![
                        ("expr", builders::string("0 8 * * *")),
                        ("tz", builders::string("UTC")),
                    ])],
                ),
            ),
            builders::assign(
                "handle",
                triggers_call(
                    "register",
                    vec![
                        ("source", builders::var("source")),
                        ("target", builders::var("from_tick")),
                        ("inputs", builders::record(vec![("tick", trigger_event())])),
                        ("name", builders::string("changed")),
                    ],
                ),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    let linked = LinkedModule::link(program, full_host_environment()).expect("link module");

    assert!(
        linked
            .module_ref
            .as_str()
            .starts_with("lashlang:v2:blake3:")
    );
}

#[test]
fn trigger_mutation_rejects_non_literal_subscription_key() {
    // key = "daily-digest"
    // await triggers.disable({ subscription_key: key, expected_revision: 1 })?
    // finish true
    let program = builders::program(vec![
        builders::assign("key", builders::string("daily-digest")),
        triggers_call(
            "disable",
            vec![
                ("subscription_key", builders::var("key")),
                ("expected_revision", builders::num(1.0)),
            ],
        ),
        builders::finish(builders::bool_lit(true)),
    ]);
    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("trigger keys must be literals");
    assert!(matches!(
        error,
        LinkError::InvalidTriggerSubscriptionKey { .. }
    ));
}

#[test]
fn trigger_mutation_rejects_reserved_subscription_key_prefix() {
    // await triggers.delete({
    //   subscription_key: "lash.internal/manager",
    //   expected_revision: 1
    // })?
    // finish true
    let program = builders::program(vec![
        triggers_call(
            "delete",
            vec![
                (
                    "subscription_key",
                    builders::string("lash.internal/manager"),
                ),
                ("expected_revision", builders::num(1.0)),
            ],
        ),
        builders::finish(builders::bool_lit(true)),
    ]);
    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("internal trigger prefix is reserved");
    assert!(matches!(
        error,
        LinkError::InvalidTriggerSubscriptionKey { .. }
    ));
}

#[test]
fn linked_module_allows_trigger_registration_name_to_match_target_process() {
    // process changed(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: changed,
    //   inputs: { tick: trigger.event },
    //   name: "changed"
    // })?
    let program = builders::module(
        vec![tick_process("changed", "tick")],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("changed")),
                    ("inputs", builders::record(vec![("tick", trigger_event())])),
                    ("name", builders::string("changed")),
                ],
            ),
        ],
    );

    LinkedModule::link(program, full_host_environment())
        .expect("trigger registration names and process names occupy different namespaces");
}

#[test]
fn linked_module_resolves_host_named_data_refs_for_fields_and_structural_assignability() {
    // process from_tick(tick: timer.Tick) { finish tick.fired_at }
    // finish true
    let direct_ref = builders::module(
        vec![tick_process_returning_fired_at("from_tick", "tick")],
        vec![builders::finish(builders::bool_lit(true))],
    );
    LinkedModule::link(direct_ref, full_host_environment())
        .expect("host data ref fields should link");

    // process from_tick(tick: { fired_at: str }) { finish tick.fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: from_tick,
    //   inputs: { tick: trigger.event }
    // })?
    let structural_input = builders::module(
        vec![builders::process(
            "from_tick",
            vec![builders::param(
                "tick",
                TypeExpr::Object(vec![builders::type_field("fired_at", TypeExpr::Str, false)]),
            )],
            builders::block(vec![builders::finish(builders::field(
                builders::var("tick"),
                "fired_at",
            ))]),
        )],
        vec![
            assign_daily_source("source"),
            register_trigger("source", "from_tick", "tick"),
        ],
    );
    LinkedModule::link(structural_input, full_host_environment())
        .expect("host data shape should be structurally assignable");
}

#[test]
fn linked_module_rejects_unknown_host_data_refs_and_opaque_source_field_access() {
    // process from_tick(tick: foo.Tick) { finish true }
    // finish true
    let unknown = builders::module(
        vec![builders::process(
            "from_tick",
            vec![builders::param("tick", TypeExpr::Ref("foo.Tick".into()))],
            builders::block(vec![builders::finish(builders::bool_lit(true))]),
        )],
        vec![builders::finish(builders::bool_lit(true))],
    );
    assert!(matches!(
        LinkedModule::link(unknown, full_host_environment()),
        Err(LinkError::UnknownType { name, .. }) if name == "foo.Tick"
    ));

    // source = timer.Schedule({ expr: "0 8 * * *" })
    // finish source.expr
    let opaque = builders::program(vec![
        assign_daily_source("source"),
        builders::finish(builders::field(builders::var("source"), "expr")),
    ]);
    assert!(matches!(
        LinkedModule::link(opaque, full_host_environment()),
        Err(LinkError::OpaqueHostDescriptorAccess { type_name, .. }) if type_name == "timer.Schedule"
    ));
}

#[test]
fn host_requirements_ref_tracks_host_named_data_type_shape_changes() {
    // process from_tick(tick: any) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: from_tick,
    //   inputs: { tick: trigger.event }
    // })?
    let program = builders::module(
        vec![builders::process(
            "from_tick",
            vec![builders::param("tick", TypeExpr::Any)],
            builders::block(vec![builders::finish(builders::bool_lit(true))]),
        )],
        vec![
            assign_daily_source("source"),
            register_trigger("source", "from_tick", "tick"),
        ],
    );
    let first = LinkedModule::link(
        program.clone(),
        LashlangHostEnvironment::new(
            resources_with_timer_event(timer_tick_type_with_field("fired_at")),
            LashlangAbilities::all(),
        ),
    )
    .expect("link first trigger occurrence shape");
    let second = LinkedModule::link(
        program,
        LashlangHostEnvironment::new(
            resources_with_timer_event(timer_tick_type_with_field("delivered_at")),
            LashlangAbilities::all(),
        ),
    )
    .expect("link changed trigger occurrence shape");

    assert_ne!(first.host_requirements_ref, second.host_requirements_ref);
}

#[test]
fn linked_module_validates_value_constructors_and_trigger_registry_ops() {
    // process scan(tick: timer.Tick) -> bool { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
    // handle = await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event },
    //   name: "scan",
    //   subscription_key: "scan"
    // })?
    // registrations = await triggers.list({ target: scan })?
    // disabled = await triggers.disable({
    //   subscription_key: "scan",
    //   expected_revision: registrations[0].revision
    // })?
    // finish { handle: handle, registrations: registrations, disabled: disabled }
    let program = builders::module(
        vec![builders::process_returning(
            "scan",
            vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
            TypeExpr::Bool,
            builders::block(vec![builders::finish(builders::bool_lit(true))]),
        )],
        vec![
            builders::assign(
                "source",
                builders::receiver_call(
                    builders::resource(&["timer"]),
                    "Schedule",
                    vec![builders::record(vec![
                        ("expr", builders::string("0 8 * * *")),
                        ("tz", builders::string("UTC")),
                    ])],
                ),
            ),
            builders::assign(
                "handle",
                triggers_call(
                    "register",
                    vec![
                        ("source", builders::var("source")),
                        ("target", builders::var("scan")),
                        ("inputs", builders::record(vec![("tick", trigger_event())])),
                        ("name", builders::string("scan")),
                        ("subscription_key", builders::string("scan")),
                    ],
                ),
            ),
            builders::assign(
                "registrations",
                triggers_call("list", vec![("target", builders::var("scan"))]),
            ),
            builders::assign(
                "disabled",
                triggers_call(
                    "disable",
                    vec![
                        ("subscription_key", builders::string("scan")),
                        (
                            "expected_revision",
                            builders::field(
                                builders::index(builders::var("registrations"), builders::num(0.0)),
                                "revision",
                            ),
                        ),
                    ],
                ),
            ),
            builders::finish(builders::record(vec![
                ("handle", builders::var("handle")),
                ("registrations", builders::var("registrations")),
                ("disabled", builders::var("disabled")),
            ])),
        ],
    );
    assert!(LinkedModule::link(program, full_host_environment()).is_ok());
}

#[test]
fn linked_module_accepts_explicit_trigger_input_mappings() {
    // process scan(a: timer.Tick, b: { fired_at: str }) {
    //   finish { a: a.fired_at, b: b.fired_at }
    // }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { a: trigger.event, b: trigger.event }
    // })?
    let repeated_event = builders::module(
        vec![builders::process(
            "scan",
            vec![
                builders::param("a", TypeExpr::Ref("timer.Tick".into())),
                builders::param(
                    "b",
                    TypeExpr::Object(vec![builders::type_field("fired_at", TypeExpr::Str, false)]),
                ),
            ],
            builders::block(vec![builders::finish(builders::record(vec![
                ("a", builders::field(builders::var("a"), "fired_at")),
                ("b", builders::field(builders::var("b"), "fired_at")),
            ]))]),
        )],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![("a", trigger_event()), ("b", trigger_event())]),
                    ),
                ],
            ),
        ],
    );
    LinkedModule::link(repeated_event, full_host_environment())
        .expect("event payload should map to multiple assignable params");

    // process scan(tick: timer.Tick, tool: Tools) {
    //   text = await tool.read_file({ path: tick.fired_at })?
    //   finish text
    // }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event, tool: tools }
    // })?
    let fixed_authority = builders::module(
        vec![builders::process(
            "scan",
            vec![
                builders::param("tick", TypeExpr::Ref("timer.Tick".into())),
                builders::param("tool", TypeExpr::Ref("Tools".into())),
            ],
            builders::block(vec![
                builders::assign(
                    "text",
                    builders::unwrap(builders::await_expr(builders::receiver_call(
                        builders::var("tool"),
                        "read_file",
                        vec![builders::record(vec![(
                            "path",
                            builders::field(builders::var("tick"), "fired_at"),
                        )])],
                    ))),
                ),
                builders::finish(builders::var("text")),
            ]),
        )],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![
                            ("tick", trigger_event()),
                            ("tool", builders::resource(&["tools"])),
                        ]),
                    ),
                ],
            ),
        ],
    );
    LinkedModule::link(fixed_authority, full_host_environment())
        .expect("fixed resource inputs should satisfy process authority params");
}

#[test]
fn linked_module_rejects_colliding_default_trigger_keys() {
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // first = timer.Schedule({ expr: "0 8 * * *" })
    // second = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: first, target: scan, inputs: { tick: trigger.event } })?
    // await triggers.register({ source: second, target: scan, inputs: { tick: trigger.event } })?
    let program = builders::module(
        vec![tick_process_returning_fired_at("scan", "tick")],
        vec![
            assign_daily_source("first"),
            assign_daily_source("second"),
            register_trigger("first", "scan", "tick"),
            register_trigger("second", "scan", "tick"),
        ],
    );

    let error = LinkedModule::link(program, full_host_environment())
        .expect_err("duplicate derived keys must fail linking");
    assert!(matches!(
        &error,
        LinkError::DuplicateDerivedTriggerSubscriptionKey {
            process,
            source_type,
            ..
        } if process == "scan" && source_type == "timer.Schedule"
    ));
    assert!(
        error
            .to_string()
            .contains("explicit literal subscription_key")
    );
}

#[test]
fn linked_module_allows_explicit_keys_for_default_key_collision_shape() {
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event },
    //   subscription_key: "morning-scan-primary"
    // })?
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event },
    //   subscription_key: "morning-scan-secondary"
    // })?
    let keyed_registration = |key: &str| {
        triggers_call(
            "register",
            vec![
                ("source", builders::var("source")),
                ("target", builders::var("scan")),
                ("inputs", builders::record(vec![("tick", trigger_event())])),
                ("subscription_key", builders::string(key)),
            ],
        )
    };
    let program = builders::module(
        vec![tick_process_returning_fired_at("scan", "tick")],
        vec![
            assign_daily_source("source"),
            keyed_registration("morning-scan-primary"),
            keyed_registration("morning-scan-secondary"),
        ],
    );

    LinkedModule::link(program, full_host_environment())
        .expect("explicit literal keys disambiguate registration sites");
}

#[test]
fn linked_artifact_materializes_explicit_and_generated_keys_into_register_calls() {
    let source = serde_json::json!({ "expr": "0 8 * * *" });
    let source_key = semantic_trigger_source_key("timer.Schedule", &source);
    let derived_key = semantic_trigger_subscription_key("scan", "timer.Schedule", &source_key);
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    // morning = timer.Schedule({ expr: "0 8 * * *" })
    // evening = timer.Schedule({ expr: "0 18 * * *" })
    // await triggers.register({ source: morning, target: scan, inputs: { tick: trigger.event } })?
    // await triggers.register({
    //   source: evening,
    //   target: scan,
    //   inputs: { tick: trigger.event },
    //   subscription_key: "evening-scan"
    // })?
    let program = builders::module(
        vec![tick_process_returning_fired_at("scan", "tick")],
        vec![
            assign_daily_source("morning"),
            builders::assign("evening", timer_schedule("0 18 * * *")),
            register_trigger("morning", "scan", "tick"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("evening")),
                    ("target", builders::var("scan")),
                    ("inputs", builders::record(vec![("tick", trigger_event())])),
                    ("subscription_key", builders::string("evening-scan")),
                ],
            ),
        ],
    );
    let linked =
        LinkedModule::link(program, full_host_environment()).expect("link manifest module");

    // The key the linker derived and the key the program stated both reach the
    // artifact as literals on the register call, which is what a replaying host
    // matches a registration on. The IR is read directly: the crate no longer
    // renders source to read it back out of (ADR 0096).
    let keys = subscription_keys(&linked.artifact.canonical_ir);
    assert!(keys.contains(&derived_key), "{keys:?}");
    assert!(keys.contains(&"evening-scan".to_string()), "{keys:?}");
}

/// Every `subscription_key:` string literal in `program`, in walk order.
fn subscription_keys(program: &Program) -> Vec<String> {
    struct Keys(Vec<String>);

    impl crate::ExprVisitor for Keys {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Record(fields) = expr {
                for (name, value) in fields {
                    if name == "subscription_key"
                        && let Expr::String(text) = value
                    {
                        self.0.push(text.to_string());
                    }
                }
            }
            crate::walk_expr(self, expr);
        }
    }

    let mut keys = Keys(Vec::new());
    crate::ExprVisitor::visit_expr(&mut keys, &program.main);
    keys.0
}

#[test]
fn linked_module_accepts_button_trigger_source_constructor() {
    let mut resources = resources();
    resources
        .add_trigger_source_constructor(
            ["ui", "button", "pressed"],
            TypeExpr::Object(vec![]),
            NamedDataType::object(
                "ui.button.Pressed",
                vec![
                    TypeField {
                        name: "button".into(),
                        ty: TypeExpr::Union(vec![
                            TypeExpr::Enum(vec!["Red".into()]),
                            TypeExpr::Enum(vec!["Blue".into()]),
                        ]),
                        optional: false,
                    },
                    TypeField {
                        name: "message".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    },
                    TypeField {
                        name: "pressed_at".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    },
                ],
            )
            .expect("valid button event type"),
        )
        .expect("valid button trigger source");
    // process on_button(event: ui.button.Pressed) {
    //   wake { kind: "button_pressed", button: event.button, message: event.message }
    //   finish true
    // }
    //
    // handle = await triggers.register({
    //   source: ui.button.pressed({}),
    //   target: on_button,
    //   inputs: { event: trigger.event },
    //   name: "button watcher"
    // })?
    // finish handle
    let program = builders::module(
        vec![builders::process(
            "on_button",
            vec![builders::param(
                "event",
                TypeExpr::Ref("ui.button.Pressed".into()),
            )],
            builders::block(vec![
                builders::wake(builders::record(vec![
                    ("kind", builders::string("button_pressed")),
                    ("button", builders::field(builders::var("event"), "button")),
                    (
                        "message",
                        builders::field(builders::var("event"), "message"),
                    ),
                ])),
                builders::finish(builders::bool_lit(true)),
            ]),
        )],
        vec![
            builders::assign(
                "handle",
                triggers_call(
                    "register",
                    vec![
                        (
                            "source",
                            builders::receiver_call(
                                builders::resource(&["ui", "button"]),
                                "pressed",
                                vec![builders::record(Vec::new())],
                            ),
                        ),
                        ("target", builders::var("on_button")),
                        ("inputs", builders::record(vec![("event", trigger_event())])),
                        ("name", builders::string("button watcher")),
                    ],
                ),
            ),
            builders::finish(builders::var("handle")),
        ],
    );

    LinkedModule::link(
        program,
        LashlangHostEnvironment::new(resources, LashlangAbilities::all()),
    )
    .expect("button trigger source should link");
}

#[test]
fn named_process_signature_survives_parameter_return_container_branch_and_trigger_flow() {
    // process scan(event: timer.Tick) -> bool { finish true }
    // process install(handler: Process<(event: timer.Tick), bool>)
    //     -> Process<(event: timer.Tick), bool> {
    //   handlers = [handler]
    //   boxed = { target: handlers[0] }
    //   selected = handler
    //   if true { selected = boxed.target } else { selected = handler }
    //   source = timer.Schedule({ expr: "0 8 * * *" })
    //   await triggers.register({
    //     source: source,
    //     target: selected,
    //     inputs: { event: trigger.event },
    //     subscription_key: "indirect-handler"
    //   })?
    //   finish selected
    // }
    // finish start install(handler: scan)
    let handler_ty = || {
        builders::process_type(
            vec![builders::param("event", TypeExpr::Ref("timer.Tick".into()))],
            TypeExpr::Bool,
        )
    };
    let program = builders::module(
        vec![
            builders::process_returning(
                "scan",
                vec![builders::param("event", TypeExpr::Ref("timer.Tick".into()))],
                TypeExpr::Bool,
                builders::block(vec![builders::finish(builders::bool_lit(true))]),
            ),
            builders::process_returning(
                "install",
                vec![builders::param("handler", handler_ty())],
                handler_ty(),
                builders::block(vec![
                    builders::assign("handlers", builders::list(vec![builders::var("handler")])),
                    builders::assign(
                        "boxed",
                        builders::record(vec![(
                            "target",
                            builders::index(builders::var("handlers"), builders::num(0.0)),
                        )]),
                    ),
                    builders::assign("selected", builders::var("handler")),
                    builders::if_else(
                        builders::bool_lit(true),
                        builders::block(vec![builders::assign(
                            "selected",
                            builders::field(builders::var("boxed"), "target"),
                        )]),
                        builders::block(vec![builders::assign(
                            "selected",
                            builders::var("handler"),
                        )]),
                    ),
                    assign_daily_source("source"),
                    triggers_call(
                        "register",
                        vec![
                            ("source", builders::var("source")),
                            ("target", builders::var("selected")),
                            ("inputs", builders::record(vec![("event", trigger_event())])),
                            ("subscription_key", builders::string("indirect-handler")),
                        ],
                    ),
                    builders::finish(builders::var("selected")),
                ]),
            ),
        ],
        vec![builders::finish(builders::start(
            "install",
            vec![("handler", builders::var("scan"))],
        ))],
    );

    LinkedModule::link(program, full_host_environment())
        .expect("named process signature should survive supported indirect flows");
}

#[test]
fn zero_parameter_process_is_valid_but_trigger_registration_still_requires_event_mapping() {
    // process idle() -> bool { finish true }
    // finish start idle()
    let idle_process = || {
        builders::process_returning(
            "idle",
            Vec::new(),
            TypeExpr::Bool,
            builders::block(vec![builders::finish(builders::bool_lit(true))]),
        )
    };
    let direct = builders::module(
        vec![idle_process()],
        vec![builders::finish(builders::start("idle", Vec::new()))],
    );
    LinkedModule::link(direct, full_host_environment()).expect("zero parameter start links");

    // process idle() -> bool { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: idle, inputs: {} })?
    let trigger = builders::module(
        vec![idle_process()],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("idle")),
                    ("inputs", builders::record(Vec::new())),
                ],
            ),
        ],
    );
    assert!(matches!(
        LinkedModule::link(trigger, full_host_environment()),
        Err(LinkError::MissingTriggerEventInput { .. })
    ));
}

#[test]
fn omitted_trigger_inputs_bind_the_event_to_a_single_target_parameter() {
    // process <declaration>
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: <name> })?
    let register = |name: &str, declaration: Declaration| {
        let program = builders::module(
            vec![declaration],
            vec![
                assign_daily_source("source"),
                triggers_call(
                    "register",
                    vec![
                        ("source", builders::var("source")),
                        ("target", builders::var(name)),
                    ],
                ),
            ],
        );
        LinkedModule::link(program, full_host_environment())
    };
    // process scan(tick: timer.Tick) { finish tick.fired_at }
    register("scan", tick_process_returning_fired_at("scan", "tick"))
        .expect("event without inputs");
    // process idle() -> bool { finish true }
    assert!(matches!(
        register(
            "idle",
            builders::process_returning(
                "idle",
                Vec::new(),
                TypeExpr::Bool,
                builders::block(vec![builders::finish(builders::bool_lit(true))]),
            ),
        ),
        Err(LinkError::TriggerTargetTakesNoEvent { .. })
    ));
    // process scan(tick: timer.Tick, label: str) { finish label }
    assert!(matches!(
        register(
            "scan",
            builders::process(
                "scan",
                vec![
                    builders::param("tick", TypeExpr::Ref("timer.Tick".into())),
                    builders::param("label", TypeExpr::Str),
                ],
                builders::block(vec![builders::finish(builders::var("label"))]),
            ),
        ),
        Err(LinkError::AmbiguousOmittedTriggerInputs { .. })
    ));
    // process scan(tick: str) { finish tick }
    assert!(matches!(
        register(
            "scan",
            builders::process(
                "scan",
                vec![builders::param("tick", TypeExpr::Str)],
                builders::block(vec![builders::finish(builders::var("tick"))]),
            ),
        ),
        Err(LinkError::TriggerEventMismatch { .. })
    ));
}

#[test]
fn linked_module_rejects_bad_trigger_registry_bindings() {
    // process scan(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ target: scan })?
    let missing = builders::module(
        vec![tick_process("scan", "tick")],
        vec![
            assign_daily_source("source"),
            triggers_call("register", vec![("target", builders::var("scan"))]),
        ],
    );
    assert!(matches!(
        LinkedModule::link(missing, full_host_environment()),
        Err(LinkError::InvalidTriggerRegistration { .. })
    ));

    // process scan(tick: timer.Tick) { finish true }
    // await triggers.register({
    //   source: { expr: "0 8 * * *" },
    //   target: scan,
    //   inputs: { tick: trigger.event }
    // })?
    let wrong_source = builders::module(
        vec![tick_process("scan", "tick")],
        vec![triggers_call(
            "register",
            vec![
                (
                    "source",
                    builders::record(vec![("expr", builders::string("0 8 * * *"))]),
                ),
                ("target", builders::var("scan")),
                ("inputs", builders::record(vec![("tick", trigger_event())])),
            ],
        )],
    );
    assert!(matches!(
        LinkedModule::link(wrong_source, full_host_environment()),
        Err(LinkError::UnknownTriggerEventType { .. })
    ));

    // process scan(tick: str) { finish tick }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: scan, inputs: { tick: trigger.event } })?
    let payload_mismatch = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tick", TypeExpr::Str)],
            builders::block(vec![builders::finish(builders::var("tick"))]),
        )],
        vec![
            assign_daily_source("source"),
            register_trigger("source", "scan", "tick"),
        ],
    );
    assert!(matches!(
        LinkedModule::link(payload_mismatch, full_host_environment()),
        Err(LinkError::TriggerEventMismatch { .. })
    ));

    // process scan(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event, extra: "nope" }
    // })?
    let unknown_input = builders::module(
        vec![tick_process("scan", "tick")],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![
                            ("tick", trigger_event()),
                            ("extra", builders::string("nope")),
                        ]),
                    ),
                ],
            ),
        ],
    );
    assert!(matches!(
        LinkedModule::link(unknown_input, full_host_environment()),
        Err(LinkError::UnknownTriggerInput { input, .. }) if input == "extra"
    ));

    // process scan(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: trigger.event, tick: trigger.event }
    // })?
    let duplicate_input = builders::module(
        vec![tick_process("scan", "tick")],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![
                            ("tick", trigger_event()),
                            ("tick", trigger_event()),
                        ]),
                    ),
                ],
            ),
        ],
    );
    assert!(matches!(
        LinkedModule::link(duplicate_input, full_host_environment()),
        Err(LinkError::DuplicateTriggerInput { input, .. }) if input == "tick"
    ));

    // process scan(tick: timer.Tick, label: str) { finish label }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { tick: { fired_at: "static" }, label: "static" }
    // })?
    let no_event_input = builders::module(
        vec![builders::process(
            "scan",
            vec![
                builders::param("tick", TypeExpr::Ref("timer.Tick".into())),
                builders::param("label", TypeExpr::Str),
            ],
            builders::block(vec![builders::finish(builders::var("label"))]),
        )],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![
                            (
                                "tick",
                                builders::record(vec![("fired_at", builders::string("static"))]),
                            ),
                            ("label", builders::string("static")),
                        ]),
                    ),
                ],
            ),
        ],
    );
    assert!(matches!(
        LinkedModule::link(no_event_input, full_host_environment()),
        Err(LinkError::MissingTriggerEventInput { .. })
    ));

    // process scan(fired_at: str) { finish fired_at }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({
    //   source: source,
    //   target: scan,
    //   inputs: { fired_at: trigger.event.fired_at }
    // })?
    let event_projection = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("fired_at", TypeExpr::Str)],
            builders::block(vec![builders::finish(builders::var("fired_at"))]),
        )],
        vec![
            assign_daily_source("source"),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    (
                        "inputs",
                        builders::record(vec![(
                            "fired_at",
                            builders::resource(&["trigger", "event", "fired_at"]),
                        )]),
                    ),
                ],
            ),
        ],
    );
    assert!(matches!(
        LinkedModule::link(event_projection, full_host_environment()),
        Err(LinkError::TriggerEventProjection { .. })
    ));

    // process scan(tick: timer.Tick) { finish true }
    // finish trigger.event
    let event_outside_inputs = builders::module(
        vec![tick_process("scan", "tick")],
        vec![builders::finish(trigger_event())],
    );
    assert!(matches!(
        LinkedModule::link(event_outside_inputs, full_host_environment()),
        Err(LinkError::TriggerEventOutsideInputs { .. })
    ));

    // process scan(tick: timer.Tick, extra: str) { finish extra }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: scan, inputs: { tick: trigger.event } })?
    let multi_input = builders::module(
        vec![builders::process(
            "scan",
            vec![
                builders::param("tick", TypeExpr::Ref("timer.Tick".into())),
                builders::param("extra", TypeExpr::Str),
            ],
            builders::block(vec![builders::finish(builders::var("extra"))]),
        )],
        vec![
            assign_daily_source("source"),
            register_trigger("source", "scan", "tick"),
        ],
    );
    assert!(matches!(
        LinkedModule::link(multi_input, full_host_environment()),
        Err(LinkError::MissingTriggerInput { input, .. }) if input == "extra"
    ));

    // process scan(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: source, inputs: { tick: trigger.event } })?
    let target_is_not_process = builders::module(
        vec![tick_process("scan", "tick")],
        vec![
            assign_daily_source("source"),
            register_trigger("source", "source", "tick"),
        ],
    );
    assert!(matches!(
        LinkedModule::link(target_is_not_process, full_host_environment()),
        Err(LinkError::InvalidTriggerTarget { .. })
    ));

    // process scan(tick: timer.Tick) { finish true }
    // await triggers.list({})?
    let list_without_filters = builders::module(
        vec![tick_process("scan", "tick")],
        vec![triggers_call("list", Vec::new())],
    );
    assert!(LinkedModule::link(list_without_filters, full_host_environment()).is_ok());

    // process scan(tick: timer.Tick) { finish true }
    // await triggers.list({
    //   target: scan,
    //   name: "daily",
    //   source_type: "timer.Schedule",
    //   enabled: true
    // })?
    let list_with_filters = builders::module(
        vec![tick_process("scan", "tick")],
        vec![triggers_call(
            "list",
            vec![
                ("target", builders::var("scan")),
                ("name", builders::string("daily")),
                ("source_type", builders::string("timer.Schedule")),
                ("enabled", builders::bool_lit(true)),
            ],
        )],
    );
    assert!(LinkedModule::link(list_with_filters, full_host_environment()).is_ok());

    // process scan(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.list({ target: source })?
    let list_target_is_not_process = builders::module(
        vec![tick_process("scan", "tick")],
        vec![
            assign_daily_source("source"),
            triggers_call("list", vec![("target", builders::var("source"))]),
        ],
    );
    assert!(matches!(
        LinkedModule::link(list_target_is_not_process, full_host_environment()),
        Err(LinkError::InvalidTriggerTarget { .. })
            | Err(LinkError::IncompatibleOperationInput { .. })
    ));

    // source = timer.Schedule({ expr: 1 })
    // finish source
    let constructor_mismatch = builders::program(vec![
        builders::assign(
            "source",
            builders::receiver_call(
                builders::resource(&["timer"]),
                "Schedule",
                vec![builders::record(vec![("expr", builders::num(1.0))])],
            ),
        ),
        builders::finish(builders::var("source")),
    ]);
    assert!(matches!(
        LinkedModule::link(constructor_mismatch, full_host_environment()),
        Err(LinkError::IncompatibleConstructorInput { .. })
    ));

    // await tools.read_file({ path: 1 })?
    let operation_mismatch = builders::program(vec![builders::module_call(
        &["tools"],
        "read_file",
        vec![builders::record(vec![("path", builders::num(1.0))])],
    )]);
    assert!(matches!(
        LinkedModule::link(operation_mismatch, full_host_environment()),
        Err(LinkError::IncompatibleOperationInput { .. })
    ));
}
