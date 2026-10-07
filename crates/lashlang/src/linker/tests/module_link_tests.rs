use super::*;
use crate::ast::CoercingBinaryOp;

#[test]
fn linked_module_accepts_board_process_with_imported_schemas() {
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
    // finish "done via board E2E"
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
                    builders::logical(
                        builders::binary(
                            builders::field(builders::var("state"), "turn"),
                            CoercingBinaryOp::StrictEqual,
                            builders::string("O"),
                        ),
                        crate::ast::OperandLogicalOp::And,
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
            builders::finish(builders::string("done via board E2E")),
        ],
    );

    LinkedModule::link(program, environment.clone())
        .expect("link board process with imported schemas");

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
    // await triggers.register({ source: source, target: { definition: scan }, inputs: { tick: trigger.event } })?
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
                    (
                        "target",
                        builders::record(vec![("definition", builders::var("scan"))]),
                    ),
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
    let process = linked.artifact.ir().process("scan").expect("scan process");
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
    // await triggers.register({ source: source, target: { definition: scan }, inputs: { tick: trigger.event } })?
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
    // await triggers.register({ source: source, target: { definition: done }, inputs: { tick: trigger.event } })?
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
                    (
                        "target",
                        builders::record(vec![("definition", builders::var("done"))]),
                    ),
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

/// The artifact port keys store bytes by the reference its caller names, so a
/// module's bytes can sit under another module's reference. The typed view
/// refuses to hand them back as that other module.
#[tokio::test]
async fn a_module_stored_under_another_reference_is_refused() {
    use lash_core_execution::ModuleArtifactStore as _;

    let module = |value: f64| {
        ModuleArtifact::from_program(builders::program(vec![builders::finish(builders::num(
            value,
        ))]))
        .expect("build module artifact")
    };
    let (stored, named) = (module(1.0), module(2.0));
    let port = std::sync::Arc::new(crate::InMemoryLashlangArtifactStore::new());
    port.publish_module_artifact(
        &lash_core_execution::ReferrerClaim::unguarded(
            lash_core_execution::ArtifactReferrer::HostPin(
                lash_core_execution::HostArtifactPin::mint(),
            ),
        )
        .expect("a host pin is unguarded"),
        named.module_ref().as_str(),
        &stored.to_store_bytes().expect("encode module"),
    )
    .await
    .expect("the port stores bytes under any reference");

    let read = crate::LashlangArtifacts::new(port)
        .get_module_artifact(named.module_ref())
        .await;
    assert!(
        matches!(
            read,
            Err(lash_core_execution::ArtifactStoreError::StoredDataCorrupt { .. })
        ),
        "{read:?}"
    );
}
