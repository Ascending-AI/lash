// Continuation wire and equivalence cases: what the decoder accepts, what an
// authored wire must encode back to, and whether parking is observationally
// invisible.

/// The continuation wire, written out by hand from the wire schema rather than
/// captured from the serializer under test.
///
/// Reading the schema off the type definitions: a continuation is a map with
/// the fields in declaration order; every value is tagged `{"kind", "value"}`;
/// numbers carry `{"version", "bits"}` with the bits of the IEEE-754 double; a
/// heap object instead names its members `items` or `fields` beside its kind;
/// optional values are `{"kind": "unset"}` or `{"kind": "set", "value": …}`;
/// the heap carries its counters, its logical byte total, its size-schedule
/// version, and its objects in ascending ID order. The state described here is a
/// process-mode VM parked at instruction 3 holding one two-element list object,
/// whose 57 logical bytes are the 16-byte object header plus a 16-byte slot and
/// 8-byte payload for the number and a 16-byte slot and 1-byte payload for the
/// boolean.
///
/// This exists so the encoding has an oracle that is independent of the code
/// that produces it: if the serializer and the decoder drifted together to a
/// different encoding, the round-trip tests would stay green and this one would
/// not.
const AUTHORED_CONTINUATION: &str = r#"{"instruction_pointer":3,"operand_stack":[{"kind":"ref","value":1}],"last_value":{"kind":"unset"},"slots":[{"kind":"set","value":{"kind":"ref","value":1}}],"projected_slots":[false],"globals":{"kind":"record","value":[["total",{"kind":"number","value":{"version":1,"bits":4613937818241073152}}]]},"iterator_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":3,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":2,"allocation_counter":1,"live_logical_bytes":57,"size_schedule_version":1,"objects":[{"id":1,"object":{"kind":"list","items":[{"kind":"number","value":{"version":1,"bits":4607182418800017408}},{"kind":"bool","value":true}]}}]}}"#;

#[test]
fn authored_continuation_fixture_decodes_and_re_encodes_exactly() {
    let continuation: VmContinuation =
        serde_json::from_str(AUTHORED_CONTINUATION).expect("authored wire should decode");

    assert_eq!(continuation.instruction_pointer, 3);
    assert_eq!(continuation.instructions_executed, 3);
    assert_eq!(continuation.heap.allocation_counter(), 1);
    assert_eq!(continuation.heap.live_logical_bytes(), 57);
    assert_eq!(continuation.heap.size_schedule_version(), 1);
    assert_eq!(
        continuation
            .heap
            .materialize(&Value::Ref(HeapId::from_counter(1)))
            .expect("the authored object should materialize"),
        Value::List(vec![Value::Number(1.0), Value::Bool(true)].into())
    );
    assert_eq!(
        continuation.globals.get("total"),
        Some(&Value::Number(3.0))
    );

    let re_encoded =
        serde_json::to_string(&continuation).expect("decoded continuation should encode");
    assert_eq!(re_encoded, AUTHORED_CONTINUATION);
}

#[test]
fn continuation_decode_rejects_inline_compound_heap_members() {
    // The malformed continuation from the re-review: object 1 holds an inline
    // list whose member references object 2. Its IDs, counters and byte
    // accounting are all internally consistent, so only the member-shape rule
    // rejects it. Accepting it used to cost object 2 at the next collection,
    // because tracing looked at direct members while validation recursed.
    let nested = r#"{"instruction_pointer":0,"operand_stack":[{"kind":"ref","value":1}],"last_value":{"kind":"unset"},"slots":[],"projected_slots":[],"globals":{"kind":"record","value":[]},"iterator_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":3,"allocation_counter":2,"live_logical_bytes":88,"size_schedule_version":1,"objects":[{"id":1,"object":{"kind":"list","items":[{"kind":"list","value":[{"kind":"ref","value":2}]}]}},{"id":2,"object":{"kind":"list","items":[]}}]}}"#;

    let error = serde_json::from_str::<VmContinuation>(nested)
        .expect_err("an inline compound member must be rejected");
    assert!(
        error
            .to_string()
            .contains("heap object members must be scalars or heap references"),
        "unexpected rejection: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_continuation_wire_survives_resume_suspend_and_re_encode() {
    // An accepted wire has to stay accepted across a full round of execution:
    // decode, resume, run, park again, and both re-encode and re-decode. The
    // reported failure decoded and resumed, then lost a live object to the next
    // collection and could not be serialized at all.
    let program = compile_source(
        "rows = []\nfor n in range(0, 4) { rows = rows + [[n]] }\ntotal = len(rows)\nfinish total",
    )
    .expect("round-trip program should compile");
    let host = Host;
    let mut vm = continuation_test_vm(&program, &host);
    vm.suspend_after_instructions(18);
    assert_eq!(
        vm.run_for_mode().await.expect("run to the park point"),
        ExecutionOutcome::Continued
    );
    let parked = vm.suspend().expect("park should capture");
    let bytes = serde_json::to_vec(&parked).expect("park should encode");

    let decoded: VmContinuation = serde_json::from_slice(&bytes).expect("park should decode");
    let mut resumed = Vm::resume_from(decoded, &program, &host).expect("park should resume");
    resumed.suspend_after_instructions(6);
    assert_eq!(
        resumed.run_for_mode().await.expect("resume to a second park"),
        ExecutionOutcome::Continued
    );
    let re_parked = resumed.suspend().expect("second park should capture");
    let re_encoded = serde_json::to_vec(&re_parked).expect("second park should encode");
    let re_decoded: VmContinuation =
        serde_json::from_slice(&re_encoded).expect("second park should decode");
    assert_eq!(
        serde_json::to_vec(&re_decoded).expect("re-decoded park should encode"),
        re_encoded,
        "accepted continuation bytes are a fixed point"
    );

    let mut finished = Vm::resume_from(re_decoded, &program, &host).expect("second resume");
    assert_eq!(
        finished.run_for_mode().await.expect("resumed run finishes"),
        ExecutionOutcome::Finished(Value::Number(4.0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn park_and_resume_is_invisible_to_a_straight_through_run() {
    // The control VM never calls `suspend`: it runs from the same starting
    // point straight to completion. Comparing two collected executions would
    // hide any bug the collection itself introduces, which is exactly what
    // parking is supposed to be free of.
    let source = r#"
        garbage = []
        rows = []
        for n in range(0, 60) {
          garbage = [n, n + 1]
          rows = rows + [[n]]
        }
        finish len(rows)
        "#;
    let program = compile_source(source).expect("equivalence program should compile");
    let host = Host;

    let mut control = continuation_test_vm(&program, &host);
    let control_outcome = control.run_for_mode().await;

    let mut parked_vm = continuation_test_vm(&program, &host);
    parked_vm.suspend_after_instructions(120);
    assert_eq!(
        parked_vm.run_for_mode().await.expect("run to the park point"),
        ExecutionOutcome::Continued
    );
    let continuation = parked_vm.suspend().expect("park should capture");
    let bytes = serde_json::to_vec(&continuation).expect("park should encode");
    let decoded: VmContinuation = serde_json::from_slice(&bytes).expect("park should decode");
    let mut resumed = Vm::resume_from(decoded, &program, &host).expect("park should resume");
    let resumed_outcome = resumed.run_for_mode().await;

    assert_eq!(control_outcome, resumed_outcome);
    assert_eq!(
        control_outcome.expect("control run finishes"),
        ExecutionOutcome::Finished(Value::Number(60.0))
    );
    assert_eq!(
        control.instructions_executed(),
        resumed.instructions_executed(),
        "parking must not change the instruction meter"
    );

    // Accounting is compared over the reachable heap. Collecting both sides at
    // the end is symmetric — it measures the two finished VMs the same way
    // rather than putting a collection inside the control's execution path,
    // which is what would have hidden a collection bug. Live byte totals
    // include objects that are unreachable but not yet swept, and a park is an
    // extra collection point, so the totals before this step differ by exactly
    // the garbage each run happened to be carrying.
    control.suspend().expect("measure the control heap");
    resumed.suspend().expect("measure the resumed heap");
    assert_eq!(
        control.live_logical_bytes(),
        resumed.live_logical_bytes(),
        "parking must not change reachable heap accounting"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn park_and_resume_never_exhausts_memory_earlier_than_a_straight_through_run() {
    let source = r#"
        rows = []
        for n in range(0, 400) {
          rows = rows + [[n, n + 1, n + 2]]
        }
        finish len(rows)
        "#;
    let program = compile_source(source).expect("limit program should compile");
    let probe_host = DynamicMemoryHost::unbounded();
    let mut probe = continuation_test_vm_with_host(&program, &probe_host);
    probe
        .run_for_mode()
        .await
        .expect("unbounded run should finish");
    let full_bytes = probe.live_logical_bytes();

    let host = DynamicMemoryHost::unbounded();
    host.set_limit(full_bytes / 2);

    let mut control = continuation_test_vm_with_host(&program, &host);
    let control_outcome = control.run_for_mode().await;
    assert!(
        matches!(
            control_outcome,
            Err(RuntimeError::MemoryLimitExceeded { .. })
        ),
        "the control run must hit the limit: {control_outcome:?}"
    );

    let mut parked_vm = continuation_test_vm_with_host(&program, &host);
    parked_vm.suspend_after_instructions(60);
    assert_eq!(
        parked_vm.run_for_mode().await.expect("run to the park point"),
        ExecutionOutcome::Continued
    );
    let continuation = parked_vm.suspend().expect("park should capture");
    let bytes = serde_json::to_vec(&continuation).expect("park should encode");
    let decoded: VmContinuation = serde_json::from_slice(&bytes).expect("park should decode");
    let mut resumed = Vm::resume_from(decoded, &program, &host).expect("park should resume");
    let resumed_outcome = resumed.run_for_mode().await;

    assert!(
        matches!(
            resumed_outcome,
            Err(RuntimeError::MemoryLimitExceeded { limit, .. }) if limit == full_bytes / 2
        ),
        "the resumed run must hit the same limit: {resumed_outcome:?}"
    );
    // A park collects, so a resumed run carries no more unswept garbage than
    // the straight-through run at the same point. The limit bounds live plus
    // not-yet-collected bytes, so parking can only postpone exhaustion, never
    // cause it. That one-way relation is the property this asserts; the exact
    // failure instruction is not equal across the two paths and is deliberately
    // not claimed to be.
    assert!(
        resumed.instructions_executed() >= control.instructions_executed(),
        "parking must not make exhaustion arrive earlier: parked {} vs straight-through {}",
        resumed.instructions_executed(),
        control.instructions_executed()
    );
}

fn continuation_test_vm_with_host<'a, H: ExecutionHost>(
    program: &'a CompiledProgram,
    host: &'a H,
) -> Vm<'a, H> {
    let slots = SlotState::from_globals(
        Record::new(),
        &program.chunk.slot_names,
        &ProjectedBindings::new(),
    );
    Vm::new_with_mode(&program.chunk, slots, host, ExecutionMode::Foreground)
}

/// Under stress collection every instruction that can allocate must run inside
/// an open allocation scope.
///
/// A general concat isolates its result, and the isolation commits objects one
/// at a time with a collection between each. If the scope was not open, that
/// collection ran against empty pins and swept everything the VM still held —
/// including bindings the program had not touched. The reported repro left a
/// dangling reference behind an untouched binding.
async fn stress_collected_result(source: &str) -> Result<Value, RuntimeError> {
    let program = compile_source(source).expect("stress program should compile");
    let host = HeapConformanceHost {
        stress_gc: true,
        memory_limit: ExecutionBound::Unbounded,
    };
    match execute_compiled(&program, &mut State::new(), &host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => panic!("expected a finish, got {other:?}"),
    }
}

async fn unstressed_result(source: &str) -> Result<Value, RuntimeError> {
    let program = compile_source(source).expect("program should compile");
    let host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::Unbounded,
    };
    match execute_compiled(&program, &mut State::new(), &host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => panic!("expected a finish, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stress_collection_survives_a_general_concat() {
    // The reported repro: the concat's isolation allocates while `x` is live
    // only from a slot.
    let source = "x = [1]\nz = [2]\ny = [9]\ny = y + z\nfinish [x, y, z]";
    let stressed = stress_collected_result(source)
        .await
        .expect("stress-collected concat should not lose live objects");
    assert_eq!(stressed, unstressed_result(source).await.expect("baseline"));
    assert_eq!(
        stressed,
        Value::List(
            vec![
                Value::List(vec![Value::Number(1.0)].into()),
                Value::List(vec![Value::Number(9.0), Value::Number(2.0)].into()),
                Value::List(vec![Value::Number(2.0)].into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stress_collection_survives_a_slot_concat_and_a_loop_concat() {
    // `acc = acc + other` where the right operand is a bare variable lowers to
    // the fused slot form, which reads both slots without touching the stack.
    let slot_form = "x = [1]\nother = [2]\nacc = [0]\nacc = acc + other\nfinish [x, acc]";
    assert_eq!(
        stress_collected_result(slot_form)
            .await
            .expect("stress-collected slot concat should hold"),
        unstressed_result(slot_form).await.expect("baseline")
    );

    let loop_form = "kept = [[7]]\nacc = []\nfor n in range(0, 6) { acc = acc + [[n]] }\nfinish [kept, acc]";
    assert_eq!(
        stress_collected_result(loop_form)
            .await
            .expect("stress-collected loop concat should hold"),
        unstressed_result(loop_form).await.expect("baseline")
    );
}
