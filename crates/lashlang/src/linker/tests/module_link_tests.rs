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
    let environment = LashlangHostEnvironment::new(catalog);
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
