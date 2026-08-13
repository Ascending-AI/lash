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

fn slot_number(
    program: &CompiledProgram,
    continuation: &VmContinuation,
    name: &str,
) -> Option<f64> {
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

    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
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

    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
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

    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
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

    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
}

#[tokio::test(flavor = "current_thread")]
async fn durable_segment_round_trip_preserves_nan_and_negative_zero() {
    let program = compile_source(
        r#"
        nan = 0 / 0
        negative_zero = -0.0
        marker = await tools.echo({ value: 1 })?
        finish [nan, negative_zero, marker]
        "#,
    )
    .expect("numeric segment program should compile");
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("run to segment boundary"),
        ExecutionOutcome::Continued
    );
    let continuation = vm.suspend().expect("NaN continuation must capture");
    let bytes = serde_json::to_vec(&continuation).expect("NaN continuation must serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("NaN continuation must restore");
    assert_eq!(
        serde_json::to_vec(&restored).expect("NaN continuation must redump"),
        bytes
    );

    let outcome = round_trip_and_resume(&program, restored).await;
    let ExecutionOutcome::Finished(Value::List(values)) = outcome else {
        panic!("expected numeric list result")
    };
    let Value::Number(nan) = values[0] else {
        panic!("expected NaN")
    };
    let Value::Number(negative_zero) = values[1] else {
        panic!("expected negative zero")
    };
    assert!(nan.is_nan());
    assert_eq!(negative_zero.to_bits(), (-0.0_f64).to_bits());
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
    let ordered = restored.slots[ordered_slot]
        .as_ref()
        .expect("ordered value");
    let Value::Record(record) = restored
        .heap
        .materialize(ordered)
        .expect("ordered value should materialize")
    else {
        panic!("ordered slot must contain a record");
    };
    assert_eq!(
        record.keys().collect::<Vec<_>>(),
        ["zebra", "alpha", "middle"]
    );
}

#[test]
fn resume_rejects_invalid_iterator_binding_and_zero_range_step() {
    let program = compile_source("value = null\nfinish value").expect("program should compile");
    let slot_count = program.chunk.slot_names.len();
    let base = VmContinuation {
        format_version: super::super::VM_CONTINUATION_FORMAT_VERSION,
        instruction_pointer: 0,
        active_function: None,
        operand_stack: Vec::new(),
        last_value: None,
        slots: vec![None; slot_count],
        projected_slots: vec![false; slot_count],
        globals: Record::new(),
        iterator_stack: Vec::new(),
        frame_stack: Vec::new(),
        occurrence_counters: Default::default(),
        mode: ExecutionMode::Process,
        profile: None,
        pending_error_span: None,
        instructions_executed: 0,
        active_execution_elapsed: std::time::Duration::ZERO,
        heap: VmHeapContinuation::default(),
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
    let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Foreground);

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
                self.effects.lock_recover().push(value.clone());
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
                return (output, host.effects.lock_recover().clone(), boundaries);
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
    assert!(
        segmented.2 >= 1,
        "the run must cross a non-terminal boundary"
    );
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
        vm.run_process_until_effect()
            .await
            .expect("effect should succeed"),
        VmRunOutcome::EffectCompleted
    );
    assert!(matches!(
        vm.suspend(),
        Err(ContinuationError::UnserializableValue {
            variant: "Projected",
            ..
        })
    ));
    assert_eq!(
        vm.run_process_until_effect()
            .await
            .expect("skip should continue"),
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
            ExecutionBound::Unbounded,
        ),
    };
    assert!(matches!(
        Vm::resume_from(continuation, &program, &deadline_host),
        Err(ContinuationError::ExecutionDeadlineExceeded { limit_ms: 0 })
    ));
}

#[derive(Clone, Copy)]
struct HeapConformanceHost {
    stress_gc: bool,
    memory_limit: ExecutionBound<std::num::NonZeroU64>,
}

impl ExecutionHost for HeapConformanceHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::unbounded().with_memory_limit(self.memory_limit)
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.stress_gc
    }
}

struct DynamicMemoryHost {
    limit: std::sync::atomic::AtomicU64,
}

impl DynamicMemoryHost {
    fn unbounded() -> Self {
        Self {
            limit: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }

    fn set_limit(&self, limit: u64) {
        self.limit.store(limit, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ExecutionHost for DynamicMemoryHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        let limit = self.limit.load(std::sync::atomic::Ordering::SeqCst);
        ExecutionBounds::unbounded().with_memory_limit(if limit == u64::MAX {
            ExecutionBound::Unbounded
        } else {
            ExecutionBound::logical_bytes(limit)
        })
    }
}

async fn heap_conformance_run(stress_gc: bool) -> (ExecutionOutcome, Vec<u8>) {
    let program = compile_source(
        r#"
        retained = { values: [1, 2, 3], label: "stable" }
        garbage = []
        for n in range(0, 30) {
          garbage = [{ n: n }, { n: n + 1 }]
        }
        finish retained
        "#,
    )
    .expect("heap conformance program should compile");
    let host = HeapConformanceHost {
        stress_gc,
        memory_limit: ExecutionBound::Unbounded,
    };
    let mut state = State::new();
    let outcome = execute_compiled(&program, &mut state, &host)
        .await
        .expect("heap conformance program should run");
    let dump = state
        .snapshot()
        .to_canonical_bytes()
        .expect("state should serialize canonically");
    (outcome, dump)
}

#[tokio::test(flavor = "current_thread")]
async fn gc_stress_mode_preserves_results_and_canonical_dumps() {
    let default = heap_conformance_run(false).await;
    let stress = heap_conformance_run(true).await;
    assert_eq!(stress, default);
}

#[tokio::test(flavor = "current_thread")]
async fn logical_memory_exhaustion_is_an_uncatchable_typed_terminal() {
    let program = compile_source("value = [1, 2, 3, 4]\nfinish value")
        .expect("memory-bound program should compile");
    let host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(32),
    };
    let error = execute_compiled(&program, &mut State::new(), &host)
        .await
        .expect_err("logical heap limit should terminate execution");
    assert!(matches!(
        error,
        RuntimeError::MemoryLimitExceeded { limit: 32, .. }
    ));
    assert!(error.is_execution_bound_exhausted());
}

#[tokio::test(flavor = "current_thread")]
async fn failed_heapification_preserves_compound_state_transactionally() {
    let original = Value::List(
        vec![
            Value::Record(Arc::new(Record::from_iter([(
                "nested".to_string(),
                Value::Number(1.0),
            )]))),
            Value::Number(2.0),
        ]
        .into(),
    );
    let mut state = State::new();
    state
        .insert_global("payload", original.clone())
        .expect("seed plain global");
    let program = compile_source("payload[0].nested = 2\nfinish payload")
        .expect("path update should compile");
    let host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(1),
    };

    assert!(matches!(
        execute_compiled(&program, &mut state, &host).await,
        Err(RuntimeError::MemoryLimitExceeded { limit: 1, .. })
    ));
    assert_eq!(state.globals().get("payload"), Some(&original));
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("post-error state must remain encodable");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("post-error state must decode");
    assert_eq!(
        restored
            .to_canonical_bytes()
            .expect("post-error state must re-encode"),
        bytes
    );
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_add_exact_limit_succeeds_and_one_byte_over_preserves_state() {
    let setup = compile_source("counts = {}").expect("setup should compile");
    let update = compile_source("key = \"a-long-new-key\"\ncounts[key] = counts[key] + 1")
        .expect("indexed add should compile");
    let empty_record_bytes = HeapObject::Record(Box::default()).logical_bytes();
    let mut grown = Record::new();
    grown.insert("a-long-new-key".to_string(), Value::Number(1.0));
    let grown_record_bytes = HeapObject::Record(Box::new(grown)).logical_bytes();
    // The cell transition heapifies the persisted empty record once and grows
    // that same object in place, so the peak is the grown record rather than a
    // pre-update copy plus the grown one.
    let exact_limit = grown_record_bytes;
    assert!(exact_limit > empty_record_bytes);

    let mut exact = State::new();
    execute_compiled(&setup, &mut exact, &Host)
        .await
        .expect("seed exact-limit state");
    let exact_host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(exact_limit),
    };
    let exact_result = execute_compiled(&update, &mut exact, &exact_host).await;
    assert!(exact_result.is_ok(), "exact limit result: {exact_result:?}");

    let mut over = State::new();
    execute_compiled(&setup, &mut over, &Host)
        .await
        .expect("seed one-byte-over state");
    let over_host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(exact_limit - 1),
    };
    assert!(matches!(
        execute_compiled(&update, &mut over, &over_host).await,
        Err(RuntimeError::MemoryLimitExceeded { .. })
    ));
    assert_eq!(
        over.globals().get("counts"),
        Some(&Value::Record(Arc::new(Record::new())))
    );
    let bytes = over
        .snapshot()
        .to_canonical_bytes()
        .expect("rejected update must leave an encodable state");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("post-error bytes must decode");
    assert_eq!(
        restored
            .to_canonical_bytes()
            .expect("post-error bytes must be a fixed point"),
        bytes
    );
}

#[tokio::test(flavor = "current_thread")]
async fn suspend_collects_live_heap_before_park_or_keep_running_diverge() {
    let program = compile_source(
        r#"
        garbage = []
        for n in range(0, 100) { garbage = [n] }
        marker = await tools.echo({ value: 1 })?
        finish marker
        "#,
    )
    .expect("divergence program should compile");
    let host = DynamicMemoryHost::unbounded();
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    let mut vm = Vm::new_with_mode(&program.chunk, slots, &host, ExecutionMode::Foreground);
    vm.suspend_after_effects(1);
    assert_eq!(
        vm.run_for_mode().await.expect("run to effect boundary"),
        ExecutionOutcome::Continued
    );

    let continuation = vm.suspend().expect("capture collected heap");
    assert_eq!(
        vm.live_logical_bytes(),
        continuation.heap.live_logical_bytes(),
        "the resident VM and parked continuation must see the same live heap"
    );
    let bytes = serde_json::to_vec(&continuation).expect("serialize continuation");
    let restored = serde_json::from_slice(&bytes).expect("restore continuation");
    host.set_limit(continuation.heap.live_logical_bytes());
    let mut resumed = Vm::resume_from(restored, &program, &host).expect("resume at exact limit");

    let kept_outcome = vm.run_for_mode().await;
    let resumed_outcome = resumed.run_for_mode().await;
    assert_eq!(kept_outcome, resumed_outcome);
    assert!(matches!(kept_outcome, Ok(ExecutionOutcome::Finished(_))));
    assert_eq!(vm.instructions_executed(), resumed.instructions_executed());
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_dump_round_trip_is_byte_identical_and_preserves_heap_meters() {
    let program = compile_source(
        "items = []\nfor n in range(0, 20) { items = items + [{ n: n }] }\nfinish items",
    )
    .expect("meter program should compile");
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_instructions(40);
    assert_eq!(
        vm.run_for_mode().await.expect("meter run should suspend"),
        ExecutionOutcome::Continued
    );
    let before = vm.suspend().expect("meter continuation should capture");
    let bytes = serde_json::to_vec(&before).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should restore");
    let redumped = serde_json::to_vec(&restored).expect("continuation should reserialize");
    assert_eq!(redumped, bytes);
    assert_eq!(
        restored.heap.allocation_counter(),
        before.heap.allocation_counter()
    );
    assert_eq!(
        restored.heap.live_logical_bytes(),
        before.heap.live_logical_bytes()
    );
    assert_eq!(
        restored.heap.size_schedule_version(),
        HEAP_SIZE_SCHEDULE_VERSION
    );

    let prior_allocations = restored.heap.allocation_counter();
    let prior_instructions = restored.instructions_executed;
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    resumed.suspend_after_instructions(prior_instructions as usize + 25);
    assert_eq!(
        resumed
            .run_for_mode()
            .await
            .expect("resumed run should suspend"),
        ExecutionOutcome::Continued
    );
    let after = resumed
        .suspend()
        .expect("second continuation should capture");
    assert!(after.heap.allocation_counter() > prior_allocations);
}

#[tokio::test(flavor = "current_thread")]
async fn determinism_process_probe() {
    if std::env::var_os("LASHLANG_HEAP_DETERMINISM_PROBE").is_none() {
        return;
    }
    let program = compile_source(
        r#"
        special = { nan: 0 / 0, minus_zero: -0.0 }
        retained = []
        for n in range(0, 1300) {
          row = [n]
          if n == 0 { retained = push(retained, row) }
          if n == 1023 { retained = push(retained, row) }
          if n == 1299 { retained = push(retained, row) }
        }
        finish { retained: retained, special: special }
        "#,
    )
    .expect("probe program should compile");
    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host);
    let outcome = vm.run_for_mode().await.expect("probe should execute");
    assert!(matches!(outcome, ExecutionOutcome::Finished(_)));
    let mut continuation = vm.suspend().expect("probe should suspend");
    // Active wall time is intentionally nondeterministic (ADR-0055); normalize
    // only that field so the cross-process probe compares the VM/heap wire.
    continuation.active_execution_elapsed = std::time::Duration::ZERO;
    assert!(continuation.heap.allocation_counter() > 1_024);
    assert!(continuation.heap.storage_slot_count() > 3);
    assert!(continuation.heap.vacant_slot_count() > 0);
    let continuation =
        serde_json::to_vec(&continuation).expect("probe continuation should serialize");
    let restored_continuation: VmContinuation =
        serde_json::from_slice(&continuation).expect("probe continuation should restore");
    assert_eq!(
        serde_json::to_vec(&restored_continuation).expect("probe continuation should redump"),
        continuation
    );

    let (runtime_globals, heap) = vm.into_state_parts().expect("probe VM state");
    state
        .install_runtime(runtime_globals, heap)
        .expect("install probe state");
    let snapshot = state
        .snapshot()
        .to_canonical_bytes()
        .expect("probe snapshot should serialize");
    let restored_snapshot = Snapshot::from_canonical_bytes(&snapshot)
        .expect("post-sweep probe snapshot should restore");
    assert_eq!(
        restored_snapshot
            .to_canonical_bytes()
            .expect("post-sweep probe snapshot should redump"),
        snapshot
    );
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    println!(
        "HEAP_DETERMINISM_DUMP={}|{}",
        hex(&snapshot),
        hex(&continuation)
    );
}

#[test]
fn independent_os_processes_emit_byte_identical_snapshot_and_continuation_dumps() {
    let executable = std::env::current_exe().expect("current test executable");
    let run_probe = || {
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "runtime::tests::determinism_process_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASHLANG_HEAP_DETERMINISM_PROBE", "1")
            .output()
            .expect("spawn determinism probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("probe output should be UTF-8")
            .lines()
            .find_map(|line| {
                line.find("HEAP_DETERMINISM_DUMP=")
                    .map(|index| &line[index + "HEAP_DETERMINISM_DUMP=".len()..])
            })
            .expect("probe dump line")
            .to_string()
    };
    assert_eq!(run_probe(), run_probe());
}

fn decode_test_hex(input: &str) -> Vec<u8> {
    assert!(input.len().is_multiple_of(2));
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("hex should be ASCII");
            u8::from_str_radix(text, 16).expect("valid hex byte")
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn meter_persistence_process_probe() {
    let Some(mode) = std::env::var_os("LASHLANG_METER_PROBE_MODE") else {
        return;
    };
    let program = compile_source(
        "items = []\nfor n in range(0, 20) { items = items + [{ n: n }] }\nfinish items",
    )
    .expect("meter probe program should compile");
    let host = Host;
    if mode == "produce" {
        let mut vm = continuation_test_vm(&program, &host);
        vm.suspend_after_instructions(40);
        assert_eq!(
            vm.run_for_mode().await.expect("producer should suspend"),
            ExecutionOutcome::Continued
        );
        let continuation = vm.suspend().expect("producer continuation");
        let bytes = serde_json::to_vec(&continuation).expect("serialize producer continuation");
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        println!(
            "METER_PROBE={}:{}:{}:{}",
            continuation.instructions_executed,
            continuation.heap.allocation_counter(),
            continuation.heap.live_logical_bytes(),
            hex
        );
        return;
    }
    let encoded =
        std::env::var("LASHLANG_METER_CONTINUATION").expect("consumer continuation input");
    let continuation: VmContinuation = serde_json::from_slice(&decode_test_hex(&encoded))
        .expect("consumer should restore continuation");
    let old_allocations = continuation.heap.allocation_counter();
    let old_live = continuation.heap.live_logical_bytes();
    let old_instructions = continuation.instructions_executed;
    let mut vm = Vm::resume_from(continuation, &program, &host).expect("consumer should resume");
    vm.suspend_after_instructions(old_instructions as usize + 25);
    assert_eq!(
        vm.run_for_mode().await.expect("consumer should suspend"),
        ExecutionOutcome::Continued
    );
    let next = vm.suspend().expect("consumer continuation");
    println!(
        "METER_RESUMED={old_allocations}:{old_live}:{}:{}",
        next.heap.allocation_counter(),
        next.heap.live_logical_bytes()
    );
}

#[test]
fn heap_meters_continue_after_restore_in_a_new_os_process() {
    let executable = std::env::current_exe().expect("current test executable");
    let spawn_probe = |mode: &str, continuation: Option<&str>| {
        let mut command = std::process::Command::new(&executable);
        command
            .args([
                "--exact",
                "runtime::tests::meter_persistence_process_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASHLANG_METER_PROBE_MODE", mode);
        if let Some(continuation) = continuation {
            command.env("LASHLANG_METER_CONTINUATION", continuation);
        }
        let output = command.output().expect("spawn meter probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout).expect("meter probe output should be UTF-8")
    };
    let produced = spawn_probe("produce", None);
    let producer = produced
        .lines()
        .find_map(|line| line.find("METER_PROBE=").map(|index| &line[index + 12..]))
        .expect("producer meter line");
    let fields = producer.splitn(4, ':').collect::<Vec<_>>();
    assert_eq!(fields.len(), 4);
    let old_allocations = fields[1].parse::<u64>().expect("allocation counter");
    let old_live = fields[2].parse::<u64>().expect("live byte counter");
    let consumed = spawn_probe("consume", Some(fields[3]));
    let consumer = consumed
        .lines()
        .find_map(|line| line.find("METER_RESUMED=").map(|index| &line[index + 14..]))
        .expect("consumer meter line");
    let next = consumer
        .split(':')
        .map(|field| field.parse::<u64>().expect("meter field"))
        .collect::<Vec<_>>();
    assert_eq!(next[0], old_allocations);
    assert_eq!(next[1], old_live);
    assert!(next[2] > old_allocations);
}
