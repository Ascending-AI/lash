fn continuation_test_vm<'a>(program: &'a CompiledProgram, host: &'a Host) -> Vm<'a, Host> {
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    Vm::new_with_mode(&program.chunk, slots, host, ExecutionMode::Foreground)
}

async fn uninterrupted_continuation_result(program: &CompiledProgram) -> ExecutionOutcome {
    execute_compiled(program, &mut State::new(), &Host)
        .await
        .expect("uninterrupted execution should succeed")
}

async fn suspend_after_instruction_budget(
    program: &CompiledProgram,
    budget: usize,
) -> VmContinuation {
    let host = Host;
    let mut vm = continuation_test_vm(program, &host);
    vm.suspend_after_instructions(budget);
    assert_eq!(
        vm.run_for_mode().await.expect("execution should suspend"),
        ExecutionOutcome::Continued
    );
    vm.suspend().expect("VM state should be capturable")
}

async fn round_trip_and_resume(
    program: &CompiledProgram,
    continuation: VmContinuation,
) -> ExecutionOutcome {
    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored = serde_json::from_slice(&bytes).expect("continuation should deserialize");
    let host = Host;
    let mut vm = Vm::resume_from(restored, program, &host).expect("continuation should resume");
    vm.run_for_mode().await.expect("resumed VM should finish")
}

async fn find_instruction_continuation(
    program: &CompiledProgram,
    predicate: impl Fn(&VmContinuation) -> bool,
) -> VmContinuation {
    for budget in 1..=program.chunk.code.len() * 20 {
        let continuation = suspend_after_instruction_budget(program, budget).await;
        if predicate(&continuation) {
            return continuation;
        }
    }
    panic!("no instruction boundary matched the requested live state")
}

fn slot_number(program: &CompiledProgram, continuation: &VmContinuation, name: &str) -> Option<f64> {
    let index = program
        .chunk
        .slot_names
        .iter()
        .position(|slot| slot.text.as_ref() == name)?;
    match continuation.slots.get(index)?.as_ref()? {
        Value::Number(value) => Some(*value),
        _ => None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_resumes_jump_based_while_with_accumulator() {
    let program = compile_source(
        r#"
        n = 0
        total = 0
        while n < 6 {
          total = total + n
          n = n + 1
        }
        finish { n: n, total: total }
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;
    let continuation = find_instruction_continuation(&program, |continuation| {
        let Some(n @ 2.0..=4.0) = slot_number(&program, continuation, "n") else {
            return false;
        };
        slot_number(&program, continuation, "total") == Some(n * (n + 1.0) / 2.0)
    })
    .await;

    assert_eq!(round_trip_and_resume(&program, continuation).await, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_resumes_for_iterator_at_saved_cursor() {
    let program = compile_source(
        r#"
        seen = []
        for item in [2, 4, 6, 8] {
          seen = seen + [item]
        }
        finish seen
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;
    let continuation = find_instruction_continuation(&program, |continuation| {
        matches!(
            continuation.iterator_stack.as_slice(),
            [VmIteratorContinuation {
                cursor: VmIteratorCursor::List { next_index: 2, .. },
                ..
            }]
        )
    })
    .await;

    assert_eq!(round_trip_and_resume(&program, continuation).await, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_resumes_nested_inner_iterator() {
    let program = compile_source(
        r#"
        total = 0
        for outer in [1, 2, 3] {
          for inner in [10, 20, 30] {
            total = total + outer + inner
          }
        }
        finish total
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.iterator_stack.len() == 2
            && matches!(
                &continuation.iterator_stack[1].cursor,
                VmIteratorCursor::List { next_index: 2, .. }
            )
    })
    .await;

    assert_eq!(round_trip_and_resume(&program, continuation).await, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_suspends_at_quiescent_post_effect_point() {
    let program = compile_source(
        r#"
        value = await tools.echo({ value: 7 })?
        finish value + 1
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("execution should suspend"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("post-effect state should capture");

    assert_eq!(round_trip_and_resume(&program, continuation).await, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_distinguishes_present_null_from_unset_slot() {
    let program = compile_source(
        r#"
        value = null
        ignored = await tools.echo({ value: 7 })?
        finish value
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("execution should suspend"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("post-effect state should capture");
    let value_slot = program
        .chunk
        .slot_names
        .iter()
        .position(|name| name.text.as_ref() == "value")
        .expect("value slot");
    assert_eq!(continuation.slots[value_slot], Some(Value::Null));

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");
    assert_eq!(restored.slots[value_slot], Some(Value::Null));
    assert_eq!(round_trip_and_resume(&program, restored).await, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_preserves_record_insertion_order() {
    let program = compile_source(
        r#"
        ordered = { zebra: 1, alpha: 2, middle: 3 }
        ignored = await tools.echo({ value: 7 })?
        finish ordered
        "#,
    )
    .expect("program should compile");
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("execution should suspend"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("post-effect state should capture");
    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");
    let ordered_slot = program
        .chunk
        .slot_names
        .iter()
        .position(|name| name.text.as_ref() == "ordered")
        .expect("ordered slot");
    let Value::Record(record) = restored.slots[ordered_slot]
        .as_ref()
        .expect("ordered value")
    else {
        panic!("ordered slot must contain a record");
    };
    assert_eq!(record.keys().collect::<Vec<_>>(), ["zebra", "alpha", "middle"]);
}

#[test]
fn resume_rejects_invalid_iterator_binding_and_zero_range_step() {
    let program = compile_source("value = null\nfinish value").expect("program should compile");
    let slot_count = program.chunk.slot_names.len();
    let base = VmContinuation {
        instruction_pointer: 0,
        operand_stack: Vec::new(),
        last_value: None,
        slots: vec![None; slot_count],
        projected_slots: vec![false; slot_count],
        globals: Record::new(),
        iterator_stack: Vec::new(),
        occurrence_counters: Default::default(),
        mode: ExecutionMode::Process,
        profile: None,
        pending_error_span: None,
        instructions_executed: 0,
        active_execution_elapsed: std::time::Duration::ZERO,
    };
    let host = Host;
    let mut invalid_binding = base.clone();
    invalid_binding.iterator_stack.push(VmIteratorContinuation {
        cursor: VmIteratorCursor::List {
            values: Vec::new(),
            next_index: 0,
        },
        binding_slot: slot_count,
        restore_value: None,
    });
    assert!(matches!(
        Vm::resume_from(invalid_binding, &program, &host),
        Err(ContinuationError::IteratorBindingOutOfBounds { .. })
    ));

    let mut zero_step = base;
    zero_step.iterator_stack.push(VmIteratorContinuation {
        cursor: VmIteratorCursor::Range {
            next: 0,
            end: 10,
            step: 0,
        },
        binding_slot: 0,
        restore_value: None,
    });
    assert!(matches!(
        Vm::resume_from(zero_step, &program, &host),
        Err(ContinuationError::ZeroRangeStep { iterator: 0 })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_multi_effect_determinism_sweep() {
    let program = compile_source(
        r#"
        a = await tools.echo({ value: 2 })?
        b = await tools.echo({ value: a + 3 })?
        c = await tools.echo({ value: b * 4 })?
        finish [a, b, c]
        "#,
    )
    .expect("program should compile");
    let expected = uninterrupted_continuation_result(&program).await;

    for effect_count in 1..=3 {
        let host = Host;
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_effects(effect_count);
        assert_eq!(
            vm.run_for_mode().await.expect("execution should suspend"),
            ExecutionOutcome::Continued
        );
        let continuation = vm.suspend().expect("post-effect state should capture");
        assert_eq!(
            round_trip_and_resume(&program, continuation).await,
            expected,
            "resume after effect {effect_count} diverged"
        );
    }
}

#[test]
fn continuation_declines_projected_host_state_with_typed_error() {
    let program = compile_source("finish input").expect("program should compile");
    let mut projected = ProjectedBindings::new();
    projected.insert("input", ProjectedValue::scalar("input", Value::Number(3.0)));
    let slots = SlotState::from_globals(Record::new(), &program.chunk.slot_names, &projected);
    let host = Host;
    let vm = Vm::new_with_mode(
        &program.chunk,
        slots,
        &host,
        ExecutionMode::Foreground,
    );

    assert_eq!(
        vm.suspend(),
        Err(ContinuationError::UnserializableValue {
            location: "slot 0".to_string(),
            variant: "Projected",
        })
    );
}

#[derive(Default)]
struct SegmentRecordingHost {
    effects: Mutex<Vec<Value>>,
}

impl ExecutionHost for SegmentRecordingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let value = Host::perform_resource_operation(operation)?;
                self.effects.lock().expect("effects lock").push(value.clone());
                Ok(AbilityResult::Value(value))
            }
            other => Host.perform(other).await,
        }
    }
}

async fn run_with_segment_budget(
    program: &CompiledProgram,
    every: Option<usize>,
) -> (ExecutionOutcome, Vec<Value>, usize) {
    let host = SegmentRecordingHost::default();
    let mut state = State::new();
    let mut vm = Vm::from_state(program, &mut state, &host);
    let mut effects_in_segment = 0;
    let mut boundaries = 0;
    loop {
        match vm
            .run_process_until_effect()
            .await
            .expect("segmented execution should succeed")
        {
            VmRunOutcome::Complete(output) => {
                return (
                    output,
                    host.effects.lock().expect("effects lock").clone(),
                    boundaries,
                );
            }
            VmRunOutcome::EffectCompleted => {
                effects_in_segment += 1;
                if every.is_some_and(|budget| effects_in_segment == budget) {
                    let continuation = vm.suspend().expect("post-effect state should capture");
                    let bytes = serde_json::to_vec(&continuation)
                        .expect("segment continuation should serialize");
                    let restored = serde_json::from_slice(&bytes)
                        .expect("segment continuation should deserialize");
                    vm = Vm::resume_from(restored, program, &host)
                        .expect("segment continuation should resume");
                    effects_in_segment = 0;
                    boundaries += 1;
                }
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn segmented_multi_effect_run_preserves_result_and_observable_effects() {
    let program = compile_source(
        r#"
        a = await tools.echo({ value: 2 })?
        b = await tools.echo({ value: a + 3 })?
        c = await tools.echo({ value: b * 4 })?
        finish [a, b, c]
        "#,
    )
    .expect("program should compile");
    let unsegmented = run_with_segment_budget(&program, None).await;
    let segmented = run_with_segment_budget(&program, Some(1)).await;

    assert_eq!(segmented.0, unsegmented.0);
    assert_eq!(segmented.1, unsegmented.1);
    assert!(segmented.2 >= 1, "the run must cross a non-terminal boundary");
    assert_eq!(unsegmented.2, 0, "the default path must not segment");
}

#[tokio::test(flavor = "current_thread")]
async fn requested_boundary_at_non_capturable_point_is_safely_skipped() {
    let source = r#"
        value = await tools.echo({ value: 7 })?
        finish input
        "#;
    let linked = crate::LinkedModule::link(
        crate::parse(source).expect("program should parse"),
        runtime_test_environment().with_globals(["input"]),
    )
    .expect("program should link");
    let program = crate::compile_linked(&linked);
    let mut projected = ProjectedBindings::new();
    projected.insert("input", ProjectedValue::scalar("input", Value::Number(3.0)));
    let slots = SlotState::from_globals(Record::new(), &program.chunk.slot_names, &projected);
    let host = Host;
    let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Process);

    assert_eq!(
        vm.run_process_until_effect().await.expect("effect should succeed"),
        VmRunOutcome::EffectCompleted
    );
    assert!(matches!(
        vm.suspend(),
        Err(ContinuationError::UnserializableValue { variant: "Projected", .. })
    ));
    assert_eq!(
        vm.run_process_until_effect().await.expect("skip should continue"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::Number(3.0)))
    );
}

struct BoundedContinuationHost {
    bounds: ExecutionBounds,
}

impl ExecutionHost for BoundedContinuationHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.bounds
    }
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_resume_accounts_for_pre_park_instruction_and_time_meters() {
    let program = compile_source("i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i")
        .expect("program should compile");
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host);
    vm.suspend_after_instructions(1204);
    assert_eq!(
        vm.run_process_until_effect().await.expect("suspend run"),
        VmRunOutcome::EffectCompleted
    );
    let continuation = vm.suspend().expect("continuation should capture");
    assert_eq!(continuation.instructions_executed, 1204);
    assert!(continuation.active_execution_elapsed > std::time::Duration::ZERO);

    let instruction_host = BoundedContinuationHost {
        bounds: ExecutionBounds::new(
            ExecutionBound::instructions(602),
            ExecutionBound::Unbounded,
        ),
    };
    assert!(matches!(
        Vm::resume_from(continuation.clone(), &program, &instruction_host),
        Err(ContinuationError::InstructionBudgetExceeded { limit: 602 })
    ));

    let deadline_host = BoundedContinuationHost {
        bounds: ExecutionBounds::new(
            ExecutionBound::Unbounded,
            ExecutionBound::millis(0),
        ),
    };
    assert!(matches!(
        Vm::resume_from(continuation, &program, &deadline_host),
        Err(ContinuationError::ExecutionDeadlineExceeded { limit_ms: 0 })
    ));
}
