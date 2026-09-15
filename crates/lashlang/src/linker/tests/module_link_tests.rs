use super::*;
use crate::ast::BinaryOp;

#[test]
fn linked_module_accepts_restate_board_process_with_imported_schemas() {
    let mut catalog = LashlangHostCatalog::new();
    let read_input = crate::json_schema_to_type_expr(&serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
    .expect("schema imports");
    let read_output = crate::json_schema_to_type_expr(&serde_json::json!({ "type": "object" }))
        .expect("schema imports");
    let play_input = crate::json_schema_to_type_expr(&serde_json::json!({
        "type": "object",
        "properties": {
            "cell": {
                "type": "integer",
                "minimum": 0,
                "maximum": 8
            }
        },
        "required": ["cell"],
        "additionalProperties": false
    }))
    .expect("schema imports");
    let play_output = crate::json_schema_to_type_expr(&serde_json::json!({ "type": "object" }))
        .expect("schema imports");

    assert_eq!(read_output, TypeExpr::Dict);
    assert_eq!(
        play_input,
        TypeExpr::Object(vec![TypeField {
            name: "cell".into(),
            ty: TypeExpr::Int,
            optional: false,
        }])
    );
    catalog
        .add_module_operation(["board"], "Board", "read", "read", read_input, read_output)
        .expect("host catalog operation must not conflict");
    catalog
        .add_module_operation(["board"], "Board", "play", "play", play_input, play_output)
        .expect("host catalog operation must not conflict");
    crate::testing::harness::add_process_control_operations(&mut catalog);
    let environment = LashlangHostEnvironment::new(catalog, LashlangAbilities::all());
    // process play_center_once(board_tool: Board) {
    //   state = await board_tool.read({})?
    //   if state.turn == "O" and contains(state.legal_moves, 4) {
    //     move = await board_tool.play({ cell: 4 })?
    //     finish { before: state, move: move, played: true }
    //   } else { finish { before: state, played: false } }
    // }
    // handle = start play_center_once(board_tool: board)
    // result = (await handle)?
    // finish "done via Restate E2E"
    let program = builders::module(
        vec![builders::process(
            "play_center_once",
            vec![builders::param("board_tool", TypeExpr::Ref("Board".into()))],
            builders::block(vec![
                builders::assign(
                    "state",
                    builders::unwrap(builders::await_expr(builders::receiver_call(
                        builders::var("board_tool"),
                        "read",
                        vec![builders::record(Vec::new())],
                    ))),
                ),
                builders::if_else(
                    builders::binary(
                        builders::binary(
                            builders::field(builders::var("state"), "turn"),
                            BinaryOp::Equal,
                            builders::string("O"),
                        ),
                        BinaryOp::And,
                        builders::builtin(
                            "contains",
                            vec![
                                builders::field(builders::var("state"), "legal_moves"),
                                builders::num(4.0),
                            ],
                        ),
                    ),
                    builders::block(vec![
                        builders::assign(
                            "move",
                            builders::unwrap(builders::await_expr(builders::receiver_call(
                                builders::var("board_tool"),
                                "play",
                                vec![builders::record(vec![("cell", builders::num(4.0))])],
                            ))),
                        ),
                        builders::finish(builders::record(vec![
                            ("before", builders::var("state")),
                            ("move", builders::var("move")),
                            ("played", builders::bool_lit(true)),
                        ])),
                    ]),
                    builders::block(vec![builders::finish(builders::record(vec![
                        ("before", builders::var("state")),
                        ("played", builders::bool_lit(false)),
                    ]))]),
                ),
            ]),
        )],
        vec![
            builders::assign(
                "handle",
                builders::start(
                    "play_center_once",
                    vec![("board_tool", builders::resource(&["board"]))],
                ),
            ),
            builders::assign(
                "result",
                builders::unwrap(builders::await_expr(builders::var("handle"))),
            ),
            builders::finish(builders::string("done via Restate E2E")),
        ],
    );

    LinkedModule::link(program, environment.clone())
        .expect("link Restate board process with imported schemas");

    // await board.play({ cell: 4.5 })?
    let fractional = builders::program(vec![builders::module_call(
        &["board"],
        "play",
        vec![builders::record(vec![("cell", builders::num(4.5))])],
    )]);
    assert!(matches!(
        LinkedModule::link(fractional, environment),
        Err(LinkError::IncompatibleOperationInput { expected, actual, .. })
            if expected == "{ cell: int }" && actual == "{ cell: float }"
    ));
}

#[test]
fn linked_module_accepts_top_level_sleep() {
    // sleep for 1
    let program = builders::program(vec![builders::sleep_for(builders::num(1.0))]);

    LinkedModule::link(program, full_host_environment()).expect("top-level sleep should link");
}

#[test]
fn linked_module_rejects_process_lifecycle_outside_process_body() {
    // payload = wait_signal("ready")
    let program = builders::program(vec![builders::assign(
        "payload",
        builders::wait_signal("ready"),
    )]);

    let err = LinkedModule::link(program, full_host_environment())
        .expect_err("top-level process lifecycle should be rejected");

    assert!(
        matches!(
            err,
            LinkError::ProcessLifecycleOutsideProcess {
                // Link diagnostics speak one vocabulary now that TypeScript is
                // the only surface language (ADR 0096).
                keyword: "waitSignal",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn linked_module_accepts_top_level_signal_run() {
    // `signal_run` (sending) mirrors `await` / `cancel`: legal from the
    // foreground turn, unlike the process-only `wait_signal`.
    // signal_run("handle", "ready", "ping")
    let program = builders::program(vec![builders::signal_run(
        builders::string("handle"),
        "ready",
        builders::string("ping"),
    )]);

    LinkedModule::link(program, full_host_environment()).expect("top-level signal_run should link");
}

#[test]
fn linked_module_rejects_unresolved_operations() {
    // process scan(tool: Tools) { finish await tool.missing({})? }
    let bad_operation = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tool", TypeExpr::Ref("Tools".into()))],
            builders::block(vec![builders::finish(builders::unwrap(
                builders::await_expr(builders::receiver_call(
                    builders::var("tool"),
                    "missing",
                    vec![builders::record(Vec::new())],
                )),
            ))]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(bad_operation, full_host_environment()),
        Err(LinkError::UnknownResourceOperation { operation, .. }) if operation == "missing"
    ));
}

/// `sleep` is the one remaining engine ability (FIG-2999): processes, process
/// signals and triggers are no longer gated by the linker at all — whether the
/// host offers them is whether it rendered their tools.
#[test]
fn linked_module_rejects_disabled_sleep() {
    // sleep for "1s"
    let sleep = builders::program(vec![builders::sleep_for(builders::string("1s"))]);
    assert!(matches!(
        LinkedModule::link(
            sleep,
            LashlangHostEnvironment::new(resources(), LashlangAbilities::default())
        ),
        Err(LinkError::FeatureDisabled {
            feature: "sleep",
            ..
        })
    ));

    // The same module links against a host with every ability granted.
    let sleep = builders::program(vec![builders::sleep_for(builders::string("1s"))]);
    LinkedModule::link(sleep, full_host_environment()).expect("granted sleep links");
}

#[test]
fn linked_module_captures_concrete_process_body_resources_statically() {
    // process scan(tick: timer.Tick) {
    //   text = await tools.read_file({ path: tick.fired_at })?
    //   finish text
    // }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: scan, inputs: { tick: trigger.event } })?
    let read_tick_path = |receiver: Expr| {
        builders::block(vec![
            builders::assign(
                "text",
                builders::unwrap(builders::await_expr(builders::receiver_call(
                    receiver,
                    "read_file",
                    vec![builders::record(vec![(
                        "path",
                        builders::field(builders::var("tick"), "fired_at"),
                    )])],
                ))),
            ),
            builders::finish(builders::var("text")),
        ])
    };
    let registration = || {
        vec![
            builders::assign("source", timer_schedule("0 8 * * *")),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("scan")),
                    ("inputs", builders::record(vec![("tick", trigger_event())])),
                ],
            ),
        ]
    };
    let program = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
            read_tick_path(builders::resource(&["tools"])),
        )],
        registration(),
    );
    let linked = LinkedModule::link(program, full_host_environment())
        .expect("process body should capture concrete host resources");
    let process = linked
        .artifact
        .canonical_ir
        .process("scan")
        .expect("scan process");
    fn contains_resource_ref(expr: &Expr, path: &str) -> bool {
        matches!(expr, Expr::ResourceRef(resource) if resource.path_string() == path)
            || expr
                .children()
                .any(|child| contains_resource_ref(child, path))
    }
    assert!(
        contains_resource_ref(&process.body, "tools"),
        "linked process body should contain a persisted tools resource ref"
    );

    // tool = tools
    // process scan(tick: timer.Tick) {
    //   text = await tool.read_file({ path: tick.fired_at })?
    //   finish text
    // }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: scan, inputs: { tick: trigger.event } })?
    let mut shadowed_main = vec![builders::assign("tool", builders::resource(&["tools"]))];
    shadowed_main.extend(registration());
    let shadowed = builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
            read_tick_path(builders::var("tool")),
        )],
        shadowed_main,
    );
    assert!(matches!(
        LinkedModule::link(shadowed, full_host_environment()),
        Err(LinkError::UnknownName { name, .. }) if name == "tool"
    ));
}

#[test]
fn linked_module_infers_process_output_and_validates_return_annotations() {
    // process done(tick: timer.Tick) { finish true }
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // await triggers.register({ source: source, target: done, inputs: { tick: trigger.event } })?
    let inferred = builders::module(
        vec![builders::process(
            "done",
            vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
            builders::block(vec![builders::finish(builders::bool_lit(true))]),
        )],
        vec![
            builders::assign("source", timer_schedule("0 8 * * *")),
            triggers_call(
                "register",
                vec![
                    ("source", builders::var("source")),
                    ("target", builders::var("done")),
                    ("inputs", builders::record(vec![("tick", trigger_event())])),
                ],
            ),
        ],
    );
    let linked = LinkedModule::link(inferred, full_host_environment())
        .expect("source linker should materialize the inferred process output");
    let Some(TypeExpr::Process(process_type)) = linked.artifact.process_type("done") else {
        panic!("linked artifact should export a process signature");
    };
    let signature = process_type
        .as_signature()
        .expect("linked artifact process signature should be complete");
    assert_eq!(signature.params()[0].name.as_str(), "tick");
    assert_eq!(signature.output(), &TypeExpr::Bool);

    // process done(tick: timer.Tick) -> bool {
    //   if true { finish true }
    //   finish "done"
    // }
    let union_mismatch = builders::module(
        vec![builders::process_returning(
            "done",
            vec![builders::param("tick", TypeExpr::Ref("timer.Tick".into()))],
            TypeExpr::Bool,
            builders::block(vec![
                builders::if_else(
                    builders::bool_lit(true),
                    builders::block(vec![builders::finish(builders::bool_lit(true))]),
                    builders::block(Vec::new()),
                ),
                builders::finish(builders::string("done")),
            ]),
        )],
        Vec::new(),
    );
    assert!(matches!(
        LinkedModule::link(union_mismatch, full_host_environment()),
        Err(LinkError::IncompatibleProcessReturn { .. })
    ));
}

#[test]
fn linked_module_hash_ignores_unused_host_abilities() {
    // finish 1
    let program = builders::program(vec![builders::finish(builders::num(1.0))]);
    let minimal = LinkedModule::link(
        program.clone(),
        LashlangHostEnvironment::new(resources(), LashlangAbilities::default()),
    )
    .expect("link minimal");
    let processes = LinkedModule::link(
        program,
        LashlangHostEnvironment::new(resources(), LashlangAbilities::default()),
    )
    .expect("link process ability");

    assert_eq!(minimal.module_ref, processes.module_ref);
    assert_eq!(
        minimal.host_requirements_ref,
        processes.host_requirements_ref
    );
}

#[tokio::test]
async fn module_artifact_store_bytes_reject_corruption() {
    use crate::LashlangArtifactStore;

    // process scan() { finish 1 }
    let linked = LinkedModule::link(
        builders::module(
            vec![builders::process(
                "scan",
                Vec::new(),
                builders::block(vec![builders::finish(builders::num(1.0))]),
            )],
            Vec::new(),
        ),
        full_host_environment(),
    )
    .expect("link module");
    let store = crate::InMemoryLashlangArtifactStore::new();

    store
        .publish_module_artifact(
            &lash_core::ArtifactOwner::host("corruption-test"),
            &linked.artifact,
        )
        .await
        .expect("put artifact");
    assert_eq!(
        store
            .get_module_artifact(&linked.module_ref)
            .await
            .expect("get artifact")
            .expect("artifact exists")
            .module_ref,
        linked.module_ref
    );

    assert!(ModuleArtifact::from_store_bytes(b"not json").is_err());
}
