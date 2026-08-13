use crate::ast::{AssignTarget, BinaryOp, FunctionExpr};
use crate::runtime::entry_points::compile_program_internal;

fn variable(name: &str) -> Expr {
    Expr::Variable(name.into())
}

fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

fn call(function: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call {
        function: Box::new(function),
        args,
    }
}

fn function(name: Option<&str>, params: &[&str], captures: &[&str], body: Expr) -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: name.map(Into::into),
        params: params.iter().map(|name| (*name).into()).collect(),
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

fn factorial_program(n: f64) -> Program {
    let recursive = call(
        variable("factorial"),
        vec![Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::Subtract,
            right: Box::new(Expr::Number(1.0)),
        }],
    );
    let body = Expr::If {
        condition: Box::new(Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::LessEqual,
            right: Box::new(Expr::Number(1.0)),
        }),
        then_block: Box::new(Expr::Number(1.0)),
        else_block: Box::new(Expr::Binary {
            left: Box::new(variable("n")),
            op: BinaryOp::Multiply,
            right: Box::new(recursive),
        }),
    };
    Program::block(vec![
        assign("factorial", function(Some("factorial"), &["n"], &[], body)),
        Expr::Finish(Box::new(call(variable("factorial"), vec![Expr::Number(n)]))),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn user_function_closure_capture_is_deep_by_value_and_recursion_is_stackless() {
    let closure_program = Program::block(vec![
        assign("captured", Expr::List(vec![Expr::Number(10.0)])),
        assign(
            "add_capture",
            function(
                None,
                &["value"],
                &["captured"],
                Expr::Binary {
                    left: Box::new(Expr::Index {
                        target: Box::new(variable("captured")),
                        index: Box::new(Expr::Number(0.0)),
                    }),
                    op: BinaryOp::Add,
                    right: Box::new(variable("value")),
                },
            ),
        ),
        assign("captured", Expr::List(vec![Expr::Number(99.0)])),
        Expr::Finish(Box::new(call(
            variable("add_capture"),
            vec![Expr::Number(5.0)],
        ))),
    ]);
    let closure = compile_program_internal(&closure_program);
    assert_eq!(
        execute_compiled(&closure, &mut State::new(), &Host)
            .await
            .expect("closure program"),
        ExecutionOutcome::Finished(Value::Number(15.0))
    );

    let factorial = compile_program_internal(&factorial_program(8.0));
    assert_eq!(
        execute_compiled(&factorial, &mut State::new(), &Host)
            .await
            .expect("recursive program"),
        ExecutionOutcome::Finished(Value::Number(40_320.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_map_reenters_the_flat_vm_and_rejects_effectful_callbacks() {
    let pure = Program::block(vec![
        assign(
            "double",
            function(
                None,
                &["value"],
                &[],
                Expr::Binary {
                    left: Box::new(variable("value")),
                    op: BinaryOp::Multiply,
                    right: Box::new(Expr::Number(2.0)),
                },
            ),
        ),
        Expr::Finish(Box::new(Expr::Map {
            items: Box::new(Expr::List(vec![
                Expr::Number(1.0),
                Expr::Number(2.0),
                Expr::Number(3.0),
            ])),
            function: Box::new(variable("double")),
        })),
    ]);
    assert_eq!(
        execute(&pure, &mut State::new(), &Host)
            .await
            .expect("pure callback"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(2.0), Value::Number(4.0), Value::Number(6.0)].into()
        ))
    );

    let effectful = Program::block(vec![
        assign(
            "observe",
            function(
                None,
                &["value"],
                &[],
                Expr::Block(vec![
                    Expr::Print(Box::new(variable("value"))),
                    variable("value"),
                ]),
            ),
        ),
        Expr::Map {
            items: Box::new(Expr::List(vec![Expr::Number(1.0)])),
            function: Box::new(variable("observe")),
        },
    ]);
    assert!(matches!(
        execute(&effectful, &mut State::new(), &Host).await,
        Err(RuntimeError::EffectInBuiltinCallback)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn effect_suspension_inside_a_user_function_round_trips_the_frame_stack() {
    let program = compile_program_internal(&Program::block(vec![
        assign(
            "observe",
            function(
                None,
                &["value"],
                &[],
                Expr::Block(vec![
                    Expr::Print(Box::new(variable("value"))),
                    variable("value"),
                ]),
            ),
        ),
        Expr::Finish(Box::new(call(
            variable("observe"),
            vec![Expr::Number(17.0)],
        ))),
    ]));
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode()
            .await
            .expect("suspend after function effect"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("capture function continuation");
    assert_eq!(continuation.frame_stack.len(), 1);
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        ExecutionOutcome::Finished(Value::Number(17.0))
    );
}

struct FunctionBoundsHost {
    bounds: ExecutionBounds,
    collect_every_allocation: bool,
}

impl ExecutionHost for FunctionBoundsHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected effect")),
        }
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.bounds
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.collect_every_allocation
    }
}

#[tokio::test(flavor = "current_thread")]
async fn frame_depth_is_a_typed_execution_bound_and_gc_stress_preserves_closures() {
    let bounded = FunctionBoundsHost {
        bounds: ExecutionBounds::unbounded()
            .with_max_frame_depth(std::num::NonZeroU64::new(4).expect("nonzero")),
        collect_every_allocation: false,
    };
    assert!(matches!(
        execute(&factorial_program(10.0), &mut State::new(), &bounded).await,
        Err(RuntimeError::FrameDepthExceeded { limit: 4 })
    ));

    let stress = FunctionBoundsHost {
        bounds: ExecutionBounds::unbounded(),
        collect_every_allocation: true,
    };
    assert_eq!(
        execute(&factorial_program(7.0), &mut State::new(), &stress)
            .await
            .expect("GC stress recursion"),
        ExecutionOutcome::Finished(Value::Number(5_040.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closure_heap_objects_round_trip_through_state_snapshot_v3() {
    let program = Program::block(vec![Expr::If {
        condition: Box::new(variable("initialize")),
        then_block: Box::new(Expr::Block(vec![
            assign("captured", Expr::List(vec![Expr::Number(7.0)])),
            assign(
                "closure",
                function(None, &[], &["captured"], variable("captured")),
            ),
            assign("initialize", Expr::Bool(false)),
        ])),
        else_block: Box::new(Expr::Finish(Box::new(call(
            variable("closure"),
            Vec::new(),
        )))),
    }]);
    let compiled = compile_program_internal(&program);
    let mut state = State::new();
    state
        .insert_global("initialize", Value::Bool(true))
        .expect("seed initialization flag");
    assert_eq!(
        execute_compiled(&compiled, &mut state, &Host)
            .await
            .expect("store closure"),
        ExecutionOutcome::Continued
    );
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("serialize closure heap");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("restore closure heap");
    assert_eq!(
        restored.to_canonical_bytes().expect("redump closure heap"),
        bytes
    );
    let mut restored_state = State::from_snapshot(restored);
    assert_eq!(
        execute_compiled(&compiled, &mut restored_state, &Host)
            .await
            .expect("call restored closure"),
        ExecutionOutcome::Finished(Value::List(vec![Value::Number(7.0)].into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_round_trip_mid_recursion_preserves_frames_heap_and_meter() {
    let program = compile_program_internal(&factorial_program(9.0));
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.frame_stack.len() >= 3 && continuation.instructions_executed > 8
    })
    .await;
    let old_instructions = continuation.instructions_executed;
    let old_allocations = continuation.heap.allocation_counter();
    let bytes = serde_json::to_vec(&continuation).expect("serialize recursive continuation");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("deserialize recursive continuation");
    let host = Host;
    let mut vm = Vm::resume_from(restored, &program, &host).expect("resume recursive continuation");
    assert_eq!(
        vm.run_for_mode().await.expect("finish resumed recursion"),
        ExecutionOutcome::Finished(Value::Number(362_880.0))
    );
    let finished = vm.suspend().expect("capture finished meter");
    assert!(finished.instructions_executed > old_instructions);
    assert!(finished.heap.allocation_counter() >= old_allocations);
}

#[tokio::test(flavor = "current_thread")]
async fn recursive_calls_keep_occurrence_counters_stable_across_resume() {
    let linked = crate::LinkedModule::link(factorial_program(8.0), runtime_test_environment())
        .expect("AST-only function program links");
    let program = crate::compile_linked(&linked);
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.frame_stack.len() >= 3 && !continuation.occurrence_counters.is_empty()
    })
    .await;
    let counters = continuation.occurrence_counters.clone();
    assert!(counters.values().any(|count| *count > 1));
    let restored: VmContinuation = serde_json::from_slice(
        &serde_json::to_vec(&continuation).expect("serialize occurrence continuation"),
    )
    .expect("restore occurrence continuation");
    assert_eq!(restored.occurrence_counters, counters);
    assert_eq!(
        round_trip_and_resume(&program, restored).await,
        ExecutionOutcome::Finished(Value::Number(40_320.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_callback_continuation_preserves_reentry_and_occurrence_counters() {
    let linked = crate::LinkedModule::link(
        Program::block(vec![
            assign(
                "increment",
                function(
                    None,
                    &["value"],
                    &[],
                    Expr::Binary {
                        left: Box::new(variable("value")),
                        op: BinaryOp::Add,
                        right: Box::new(Expr::Number(1.0)),
                    },
                ),
            ),
            assign(
                "callback",
                function(
                    None,
                    &["value"],
                    &["increment"],
                    call(variable("increment"), vec![variable("value")]),
                ),
            ),
            Expr::Finish(Box::new(Expr::Map {
                items: Box::new(Expr::List(vec![
                    Expr::Number(2.0),
                    Expr::Number(4.0),
                    Expr::Number(6.0),
                ])),
                function: Box::new(variable("callback")),
            })),
        ]),
        runtime_test_environment(),
    )
    .expect("AST-only callback program links");
    let program = crate::compile_linked(&linked);
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation
            .frame_stack
            .iter()
            .any(|frame| matches!(frame.return_target, VmFrameReturnContinuation::Map { .. }))
    })
    .await;
    let counters = continuation.occurrence_counters.clone();
    let restored: VmContinuation = serde_json::from_slice(
        &serde_json::to_vec(&continuation).expect("serialize callback continuation"),
    )
    .expect("restore callback continuation");
    assert_eq!(restored.occurrence_counters, counters);
    let host = Host;
    let mut vm = Vm::resume_from(restored, &program, &host).expect("resume callback continuation");
    assert_eq!(
        vm.run_for_mode().await.expect("finish resumed callbacks"),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(3.0), Value::Number(5.0), Value::Number(7.0)].into()
        ))
    );
    let finished = vm.suspend().expect("capture callback occurrence counters");
    assert!(
        finished
            .occurrence_counters
            .values()
            .any(|count| *count > 1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn function_recursion_determinism_process_probe() {
    if std::env::var_os("LASHLANG_FUNCTION_DETERMINISM_PROBE").is_none() {
        return;
    }
    let program = compile_program_internal(&factorial_program(9.0));
    let mut continuation =
        find_instruction_continuation(&program, |continuation| continuation.frame_stack.len() >= 3)
            .await;
    continuation.active_execution_elapsed = std::time::Duration::ZERO;
    let bytes = serde_json::to_vec(&continuation).expect("serialize recursive continuation");
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("FUNCTION_DETERMINISM_DUMP={hex}");
}

#[test]
fn independent_os_processes_dump_identical_mid_recursion_continuations() {
    let executable = std::env::current_exe().expect("current test executable");
    let run_probe = || {
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "runtime::tests::function_recursion_determinism_process_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASHLANG_FUNCTION_DETERMINISM_PROBE", "1")
            .output()
            .expect("spawn function determinism probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("probe output UTF-8")
            .lines()
            .find_map(|line| {
                line.find("FUNCTION_DETERMINISM_DUMP=")
                    .map(|index| line[index + "FUNCTION_DETERMINISM_DUMP=".len()..].to_string())
            })
            .expect("function determinism dump")
    };
    assert_eq!(run_probe(), run_probe());
}
