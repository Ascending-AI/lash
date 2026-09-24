use super::*;

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
/// whose 153 logical bytes are the 16-byte object header plus a 64-byte slot
/// and 8-byte payload for the number and a 64-byte slot and 1-byte payload for
/// the boolean, under size schedule 3.
///
/// This exists so the encoding has an oracle that is independent of the code
/// that produces it: if the serializer and the decoder drifted together to a
/// different encoding, the round-trip tests would stay green and this one would
/// not.
const AUTHORED_CONTINUATION: &str = r#"{"format_version":23,"reference_semantics":false,"instruction_pointer":3,"active_function":null,"operand_stack":[{"kind":"ref","value":1}],"pending_tools":{},"execution_nonce":0,"last_value":{"kind":"unset"},"slots":[{"kind":"set","value":{"kind":"ref","value":1}}],"globals":{"kind":"record","value":[["total",{"kind":"number","value":{"version":1,"bits":4613937818241073152}}]]},"iterator_stack":[],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":3,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":2,"allocation_counter":1,"live_logical_bytes":153,"size_schedule_version":3,"objects":[{"id":1,"object":{"kind":"list","items":[{"kind":"number","value":{"version":1,"bits":4607182418800017408}},{"kind":"bool","value":true}]}}]}}"#;

#[test]
fn snapshot_and_continuation_codecs_share_the_prototype_chain_key_guard() {
    let hostile_record = [("__proto__".to_string(), Value::Null)]
        .into_iter()
        .collect::<Record>();
    let expected =
        "record key `__proto__` names the prototype chain, which this value model does not have";

    let snapshot = Snapshot::new(
        [(
            "payload".to_string(),
            Value::Record(Arc::new(hostile_record)),
        )]
        .into_iter()
        .collect(),
    );
    let snapshot_bytes = snapshot
        .to_canonical_bytes()
        .expect("encode hostile snapshot wire");
    assert_eq!(
        Snapshot::from_canonical_bytes(&snapshot_bytes),
        Err(SnapshotDecodeError::InvalidEncoding(expected.to_string()))
    );

    let mut continuation_wire: serde_json::Value =
        serde_json::from_str(AUTHORED_CONTINUATION).expect("authored continuation JSON");
    continuation_wire["globals"] = serde_json::json!({
        "kind": "record",
        "value": [["__proto__", {"kind": "null"}]],
    });
    let continuation_error = serde_json::from_value::<VmContinuation>(continuation_wire)
        .expect_err("prototype-chain record key must not decode in a continuation");
    assert!(
        continuation_error.to_string().contains(expected),
        "both codecs must report the shared guard message: {continuation_error}"
    );
}

#[test]
fn authored_continuation_fixture_decodes_and_re_encodes_exactly() {
    let continuation: VmContinuation =
        serde_json::from_str(AUTHORED_CONTINUATION).expect("authored wire should decode");

    assert_eq!(continuation.instruction_pointer, 3);
    assert_eq!(continuation.instructions_executed, 3);
    assert_eq!(continuation.heap.allocation_counter(), 1);
    assert_eq!(continuation.heap.live_logical_bytes(), 153);
    assert_eq!(continuation.heap.size_schedule_version(), 3);
    assert_eq!(
        continuation
            .heap
            .materialize(&Value::Ref(HeapId::from_counter(1)))
            .expect("the authored object should materialize"),
        Value::List(vec![Value::Number(1.0), Value::Bool(true)].into())
    );
    assert_eq!(continuation.globals.get("total"), Some(&Value::Number(3.0)));

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
    let nested = r#"{"format_version":23,"reference_semantics":false,"instruction_pointer":0,"active_function":null,"operand_stack":[{"kind":"ref","value":1}],"pending_tools":{},"execution_nonce":0,"last_value":{"kind":"unset"},"slots":[],"globals":{"kind":"record","value":[]},"iterator_stack":[],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":3,"allocation_counter":2,"live_logical_bytes":184,"size_schedule_version":3,"objects":[{"id":1,"object":{"kind":"list","items":[{"kind":"list","value":[{"kind":"ref","value":2}]}]}},{"id":2,"object":{"kind":"list","items":[]}}]}}"#;

    let error = serde_json::from_str::<VmContinuation>(nested)
        .expect_err("an inline compound member must be rejected");
    assert!(
        error
            .to_string()
            .contains("heap object members must be scalars or heap references"),
        "unexpected rejection: {error}"
    );
}

#[test]
fn continuation_decode_rejects_an_active_function_without_a_root_frame() {
    // This is the scalar-only shape left after stripping the root frame from a
    // one-deep function call. No heap reachability accident is available to
    // reject it, so the frame-owner invariant itself must do so.
    let wire = r#"{"format_version":23,"reference_semantics":false,"instruction_pointer":1,"active_function":0,"operand_stack":[],"pending_tools":{},"execution_nonce":0,"last_value":{"kind":"unset"},"slots":[],"globals":{"kind":"record","value":[]},"iterator_stack":[],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":1,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":1,"allocation_counter":0,"live_logical_bytes":0,"size_schedule_version":3,"objects":[]}}"#;

    reject_continuation(wire, "must have a root-owned bottom frame");
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_continuation_wire_survives_resume_suspend_and_re_encode() {
    // An accepted wire has to stay accepted across a full round of execution:
    // decode, resume, run, park again, and both re-encode and re-decode. The
    // reported failure decoded and resumed, then lost a live object to the next
    // collection and could not be serialized at all.
    // `rows = []` / `for n in range(0, 4) { rows = rows + [[n]] }`
    // `total = len(rows)` / `finish total`
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign("rows", builders::list(vec![])),
        range_loop(4.0, vec![append_row(vec![builders::var("n")])]),
        builders::assign(
            "total",
            builders::builtin("len", vec![builders::var("rows")]),
        ),
        builders::finish(builders::var("total")),
    ]));
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
        resumed
            .run_for_mode()
            .await
            .expect("resume to a second park"),
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

/// `for n in range(0, <end>) { <body> }`
fn range_loop(end: f64, body: Vec<Expr>) -> Expr {
    builders::for_in(
        "n",
        builders::builtin("range", vec![builders::num(0.0), builders::num(end)]),
        builders::block(body),
    )
}

/// `n + <offset> + 1` — the `n + 1`, `n + 2` terms the loops above build.
fn plus_one(offset: f64) -> Expr {
    builders::binary(
        builders::var("n"),
        BinaryOp::Add,
        builders::num(offset + 1.0),
    )
}

/// `rows = rows + [[<items>]]`
fn append_row(items: Vec<Expr>) -> Expr {
    builders::assign(
        "rows",
        builders::binary(
            builders::var("rows"),
            BinaryOp::Add,
            builders::list(vec![builders::list(items)]),
        ),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn park_and_resume_is_invisible_to_a_straight_through_run() {
    // The control VM never calls `suspend`: it runs from the same starting
    // point straight to completion. Comparing two collected executions would
    // hide any bug the collection itself introduces, which is exactly what
    // parking is supposed to be free of.
    // `garbage = []` / `rows = []`
    // `for n in range(0, 60) { garbage = [n, n + 1]; rows = rows + [[n]] }`
    // `finish len(rows)`
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign("garbage", builders::list(vec![])),
        builders::assign("rows", builders::list(vec![])),
        range_loop(
            60.0,
            vec![
                builders::assign(
                    "garbage",
                    builders::list(vec![builders::var("n"), plus_one(0.0)]),
                ),
                append_row(vec![builders::var("n")]),
            ],
        ),
        builders::finish(builders::builtin("len", vec![builders::var("rows")])),
    ]));
    let host = Host;

    let mut control = continuation_test_vm(&program, &host);
    let control_outcome = control.run_for_mode().await;

    let mut parked_vm = continuation_test_vm(&program, &host);
    parked_vm.suspend_after_instructions(120);
    assert_eq!(
        parked_vm
            .run_for_mode()
            .await
            .expect("run to the park point"),
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
    // `rows = []` / `for n in range(0, 400) { rows = rows + [[n, n + 1, n + 2]] }`
    // `finish len(rows)`
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign("rows", builders::list(vec![])),
        range_loop(
            400.0,
            vec![append_row(vec![
                builders::var("n"),
                plus_one(0.0),
                plus_one(1.0),
            ])],
        ),
        builders::finish(builders::builtin("len", vec![builders::var("rows")])),
    ]));
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
        parked_vm
            .run_for_mode()
            .await
            .expect("run to the park point"),
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
        &program.chunk.private_slots,
        &ProjectedBindings::new(),
        Vec::new(),
    );
    Vm::new(&program.chunk, slots, host, None, ExecutionMode::Foreground)
}

/// Under stress collection every instruction that can allocate must run inside
/// an open allocation scope.
///
/// A general concat isolates its result, and the isolation commits objects one
/// at a time with a collection between each. If the scope was not open, that
/// collection ran against empty pins and swept everything the VM still held —
/// including bindings the program had not touched. The reported repro left a
/// dangling reference behind an untouched binding.
async fn stress_collected_result(program: Program) -> Result<Value, RuntimeError> {
    let program = compile_program_for_tests(program);
    let host = HeapConformanceHost {
        stress_gc: true,
        memory_limit: ExecutionBound::Unbounded,
    };
    match execute_compiled(&program, &mut State::new(), &host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => panic!("expected a finish, got {other:?}"),
    }
}

async fn unstressed_result(program: Program) -> Result<Value, RuntimeError> {
    let program = compile_program_for_tests(program);
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
    // The reported repro: the concat's isolation allocates while `x` is live only from a slot.
    let program = || {
        builders::program(vec![
            builders::assign("x", builders::list(vec![builders::num(1.0)])),
            builders::assign("z", builders::list(vec![builders::num(2.0)])),
            builders::assign("y", builders::list(vec![builders::num(9.0)])),
            builders::assign(
                "y",
                builders::binary(builders::var("y"), BinaryOp::Add, builders::var("z")),
            ),
            builders::finish(builders::list(vec![
                builders::var("x"),
                builders::var("y"),
                builders::var("z"),
            ])),
        ])
    };
    let stressed = stress_collected_result(program())
        .await
        .expect("stress-collected concat should not lose live objects");
    assert_eq!(
        stressed,
        unstressed_result(program()).await.expect("baseline")
    );
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
    // `x = [1]` / `other = [2]` / `acc = [0]` / `acc = acc + other` /
    // `finish [x, acc]`
    let slot_form = || {
        builders::program(vec![
            builders::assign("x", builders::list(vec![builders::num(1.0)])),
            builders::assign("other", builders::list(vec![builders::num(2.0)])),
            builders::assign("acc", builders::list(vec![builders::num(0.0)])),
            builders::assign(
                "acc",
                builders::binary(builders::var("acc"), BinaryOp::Add, builders::var("other")),
            ),
            builders::finish(builders::list(vec![
                builders::var("x"),
                builders::var("acc"),
            ])),
        ])
    };
    assert_eq!(
        stress_collected_result(slot_form())
            .await
            .expect("stress-collected slot concat should hold"),
        unstressed_result(slot_form()).await.expect("baseline")
    );

    // `kept = [[7]]` / `acc = []` / `for n in range(0, 6) { acc = acc + [[n]] }`
    // / `finish [kept, acc]`
    let loop_form = || {
        builders::program(vec![
            builders::assign(
                "kept",
                builders::list(vec![builders::list(vec![builders::num(7.0)])]),
            ),
            builders::assign("acc", builders::list(vec![])),
            builders::for_in(
                "n",
                builders::builtin("range", vec![builders::num(0.0), builders::num(6.0)]),
                builders::block(vec![builders::assign(
                    "acc",
                    builders::binary(
                        builders::var("acc"),
                        BinaryOp::Add,
                        builders::list(vec![builders::list(vec![builders::var("n")])]),
                    ),
                )]),
            ),
            builders::finish(builders::list(vec![
                builders::var("kept"),
                builders::var("acc"),
            ])),
        ])
    };
    assert_eq!(
        stress_collected_result(loop_form())
            .await
            .expect("stress-collected loop concat should hold"),
        unstressed_result(loop_form()).await.expect("baseline")
    );
}

/// The authored continuation wires that must not decode.
///
/// Each is a hand-written wire that is internally consistent — IDs ordered,
/// counters and byte accounting correct, every reference resolvable — so only
/// the ownership rule can reject it. Sol's review authored the first two and
/// found both accepted and resumable; an optimized append through either slot
/// would then have been visible through the other.
fn slots_wire(slots: &str, objects: &str, next_id: u64, counter: u64, bytes: u64) -> String {
    format!(
        r#"{{"format_version":23,"reference_semantics":false,"instruction_pointer":0,"active_function":null,"operand_stack":[],"pending_tools":{{}},"execution_nonce":0,"last_value":{{"kind":"unset"}},"slots":{slots},"globals":{{"kind":"record","value":[]}},"iterator_stack":[],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{{}},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{{"secs":0,"nanos":0}},"heap":{{"next_id":{next_id},"allocation_counter":{counter},"live_logical_bytes":{bytes},"size_schedule_version":3,"objects":{objects}}}}}"#
    )
}

fn reject_continuation(wire: &str, expected: &str) {
    let error =
        serde_json::from_str::<VmContinuation>(wire).expect_err("the wire must be rejected");
    assert!(
        error.to_string().contains(expected),
        "unexpected rejection: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_decode_rejects_shared_and_cyclic_durable_ownership() {
    let empty_list = r#"[{"id":1,"object":{"kind":"list","items":[]}}]"#;
    let empty_bytes = 16;

    // Two durable slots naming one object.
    reject_continuation(
        &slots_wire(
            r#"[{"kind":"set","value":{"kind":"ref","value":1}},{"kind":"set","value":{"kind":"ref","value":1}}]"#,
            empty_list,
            2,
            1,
            empty_bytes,
        ),
        "must have one owner",
    );

    // A slot and a global naming one object.
    let slot_and_global = format!(
        r#"{{"format_version":23,"reference_semantics":false,"instruction_pointer":0,"active_function":null,"operand_stack":[],"pending_tools":{{}},"execution_nonce":0,"last_value":{{"kind":"unset"}},"slots":[{{"kind":"set","value":{{"kind":"ref","value":1}}}}],"globals":{{"kind":"record","value":[["kept",{{"kind":"ref","value":1}}]]}},"iterator_stack":[],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{{}},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{{"secs":0,"nanos":0}},"heap":{{"next_id":2,"allocation_counter":1,"live_logical_bytes":{empty_bytes},"size_schedule_version":3,"objects":{empty_list}}}}}"#
    );
    reject_continuation(&slot_and_global, "must have one owner");

    // A self-referential object: the slot holds it and so does the object.
    reject_continuation(
        &slots_wire(
            r#"[{"kind":"set","value":{"kind":"ref","value":1}},{"kind":"unset"}]"#,
            r#"[{"id":1,"object":{"kind":"list","items":[{"kind":"ref","value":1}]}}]"#,
            2,
            1,
            88,
        ),
        "must have one owner",
    );

    // A diamond inside one durable root: one root, one repeated descendant.
    reject_continuation(
        &slots_wire(
            r#"[{"kind":"set","value":{"kind":"ref","value":1}},{"kind":"unset"}]"#,
            r#"[{"id":1,"object":{"kind":"list","items":[{"kind":"ref","value":2},{"kind":"ref","value":2}]}},{"id":2,"object":{"kind":"list","items":[]}}]"#,
            3,
            2,
            176,
        ),
        "must have one owner",
    );

    // A parked loop binding is durable too: it goes back into its slot when the
    // loop ends, so it cannot share with another slot.
    let restore_and_slot = format!(
        r#"{{"format_version":23,"reference_semantics":false,"instruction_pointer":0,"active_function":null,"operand_stack":[],"pending_tools":{{}},"execution_nonce":0,"last_value":{{"kind":"unset"}},"slots":[{{"kind":"set","value":{{"kind":"ref","value":1}}}}],"globals":{{"kind":"record","value":[]}},"iterator_stack":[{{"cursor":{{"Range":{{"next":0,"end":1,"step":1}}}},"binding_slot":0,"restore_value":{{"kind":"set","value":{{"kind":"ref","value":1}}}}}}],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{{}},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{{"secs":0,"nanos":0}},"heap":{{"next_id":2,"allocation_counter":1,"live_logical_bytes":{empty_bytes},"size_schedule_version":3,"objects":{empty_list}}}}}"#
    );
    reject_continuation(&restore_and_slot, "must have one owner");
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_decode_accepts_transient_duplication() {
    // The other side of the rule: the operand stack, the last-value register
    // and an iterator cursor may all name an object a slot owns. A VM that has
    // just stored a value holds it in exactly that shape.
    let wire = r#"{"format_version":23,"reference_semantics":false,"instruction_pointer":0,"active_function":null,"operand_stack":[{"kind":"ref","value":1}],"pending_tools":{},"execution_nonce":0,"last_value":{"kind":"set","value":{"kind":"ref","value":1}},"slots":[{"kind":"set","value":{"kind":"ref","value":1}}],"globals":{"kind":"record","value":[]},"iterator_stack":[{"cursor":{"List":{"values":[{"kind":"ref","value":1}],"next_index":0,"collection":{"kind":"unset"}}},"binding_slot":0,"restore_value":{"kind":"unset"}}],"frame_stack":[],"handler_stack":[],"finally_stack":[],"occurrence_counters":{},"mode":"Process","profile":null,"pending_error_span":null,"instructions_executed":0,"active_execution_elapsed":{"secs":0,"nanos":0},"heap":{"next_id":2,"allocation_counter":1,"live_logical_bytes":16,"size_schedule_version":3,"objects":[{"id":1,"object":{"kind":"list","items":[]}}]}}"#;

    let continuation: VmContinuation =
        serde_json::from_str(wire).expect("transient duplication must be accepted");
    assert_eq!(continuation.operand_stack.len(), 1);
}

/// A store must not allocate the same object twice.
///
/// The value a store leaves in its slot is also left in the last-value
/// register, and importing it once per holder meant every literal store
/// allocated its object twice and handed one straight to the collector. These
/// counts are the whole point of the transient/durable split: a transient
/// holder may point at what a durable one owns.
#[tokio::test(flavor = "current_thread")]
async fn a_store_allocates_one_object_per_live_object() {
    let one = || builders::list(vec![builders::num(1.0)]);
    let finish_zero = || builders::finish(builders::num(0.0));
    for (source, statements, expected_allocations, expected_live) in [
        (
            "xs = [1]\nfinish 0",
            vec![builders::assign("xs", one())],
            1,
            1,
        ),
        (
            "xs = [[1]]\nfinish 0",
            vec![builders::assign("xs", builders::list(vec![one()]))],
            2,
            2,
        ),
        (
            "xs = { a: [1] }\nfinish 0",
            vec![builders::assign("xs", builders::record(vec![("a", one())]))],
            2,
            2,
        ),
        // Reference semantics (ADR 0096): binding or nesting an existing
        // container shares it instead of allocating an isolated copy, so these
        // three programs allocate one object per literal written, not one per
        // holder.
        (
            "a = [1]\nxs = [a]\nfinish 0",
            vec![
                builders::assign("a", one()),
                builders::assign("xs", builders::list(vec![builders::var("a")])),
            ],
            2,
            2,
        ),
        (
            "xs = [[1]]\nys = xs\nfinish 0",
            vec![
                builders::assign("xs", builders::list(vec![one()])),
                builders::assign("ys", builders::var("xs")),
            ],
            2,
            2,
        ),
        (
            "xs = []\nxs = push(xs, [1])\nfinish 0",
            vec![
                builders::assign("xs", builders::list(vec![])),
                builders::assign(
                    "xs",
                    builders::builtin("push", vec![builders::var("xs"), one()]),
                ),
            ],
            2,
            2,
        ),
    ] {
        let mut expressions = statements;
        expressions.push(finish_zero());
        let program = compile_program_for_tests(builders::program(expressions));
        let mut state = State::new();
        execute_compiled(&program, &mut state, &Host)
            .await
            .expect("probe program should run");
        let (globals, mut heap) = state.take_runtime();
        let roots = globals.values().cloned().collect::<Vec<_>>();
        heap.collect(roots.iter());
        assert_eq!(
            (heap.allocations(), heap.objects_in_id_order().count()),
            (expected_allocations, expected_live),
            "allocation count for {source:?}"
        );
    }
}

/// Reference semantics (ADR 0096) survive the snapshot round trip: a container
/// bound into two literals is one object, a later write through the binding is
/// visible through both holders, and the restored heap says the same. Before
/// the cutover this pinned the opposite -- each literal held an isolated copy --
/// which was Lashlang's value semantics, not the VM's.
#[tokio::test(flavor = "current_thread")]
async fn shared_binding_list_and_record_literals_stay_shared_after_snapshot_round_trip() {
    // `a = [1]` / `xs = [a, a]` / `record = { left: a, right: a }` / `a[0] = 9`
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign("a", builders::list(vec![builders::num(1.0)])),
        builders::assign(
            "xs",
            builders::list(vec![builders::var("a"), builders::var("a")]),
        ),
        builders::assign(
            "record",
            builders::record(vec![
                ("left", builders::var("a")),
                ("right", builders::var("a")),
            ]),
        ),
        builders::assign_path(
            "a",
            vec![builders::index_step(builders::num(0.0))],
            builders::num(9.0),
        ),
    ]));
    let mut state = State::new();
    execute_compiled(&program, &mut state, &Host)
        .await
        .expect("shared-binding literal program should run");

    let encoded = state
        .snapshot()
        .to_canonical_bytes()
        .expect("shared-binding state should encode");
    let decoded =
        Snapshot::from_canonical_bytes(&encoded).expect("shared-binding state should decode");
    let restored = State::from_snapshot(decoded);

    let written = Value::List(vec![Value::Number(9.0)].into());
    assert_eq!(restored.globals().get("a"), Some(&written));
    assert_eq!(
        restored.globals().get("xs"),
        Some(&Value::List(vec![written.clone(), written.clone()].into()))
    );
    assert_eq!(
        restored.globals().get("record"),
        Some(&Value::Record(std::sync::Arc::new({
            let mut record = Record::new();
            record.insert("left".to_string(), written.clone());
            record.insert("right".to_string(), written);
            record
        })))
    );
}

/// A concatenation that runs out of memory partway leaves the accumulator
/// exactly as it was.
///
/// Copying and appending one member at a time meant a bound trip midway left
/// the accumulator holding part of the right operand — and since the state that
/// survives a failure is the state that gets persisted, that half-applied
/// concatenation was durable, and encoded and decoded like any other.
#[tokio::test(flavor = "current_thread")]
async fn a_rejected_concat_leaves_the_accumulator_untouched() {
    // `acc = [0]` / `other = [[1], [2], [3]]`, then `acc = acc + other`
    let setup = compile_program_for_tests(builders::program(vec![
        builders::assign("acc", builders::list(vec![builders::num(0.0)])),
        builders::assign(
            "other",
            builders::list(vec![
                builders::list(vec![builders::num(1.0)]),
                builders::list(vec![builders::num(2.0)]),
                builders::list(vec![builders::num(3.0)]),
            ]),
        ),
    ]));
    let extend = compile_program_for_tests(builders::program(vec![builders::assign(
        "acc",
        builders::binary(builders::var("acc"), BinaryOp::Add, builders::var("other")),
    )]));

    let mut state = State::new();
    execute_compiled(&setup, &mut state, &Host)
        .await
        .expect("seed the accumulator");
    let before = state
        .snapshot()
        .to_canonical_bytes()
        .expect("seeded state should encode");
    let seeded_bytes = {
        let snapshot = state.snapshot();
        let mut probe = State::from_snapshot(snapshot);
        let (_, heap) = probe.take_runtime();
        heap.live_logical_bytes()
    };

    // Enough room for part of the extension, not all of it.
    let host = HeapConformanceHost {
        stress_gc: false,
        memory_limit: ExecutionBound::logical_bytes(seeded_bytes + 100),
    };
    let error = execute_compiled(&extend, &mut state, &host)
        .await
        .expect_err("the extension must exhaust the bound");
    assert!(
        matches!(error, RuntimeError::MemoryLimitExceeded { .. }),
        "unexpected failure: {error:?}"
    );

    assert_eq!(
        state.globals().get("acc"),
        Some(&Value::List(vec![Value::Number(0.0)].into())),
        "a rejected extension must not leave members behind"
    );
    assert_eq!(
        state
            .snapshot()
            .to_canonical_bytes()
            .expect("post-failure state should encode"),
        before,
        "a rejected extension must leave the state byte-identical"
    );
}

#[test]
fn a_continuation_with_integer_out_of_range_format_version_is_refused_as_version_mismatch() {
    let mut raw: serde_json::Value =
        serde_json::from_str(AUTHORED_CONTINUATION).expect("parse authored continuation");
    raw["format_version"] = serde_json::json!(4_294_967_296_u64);

    let decode_error = serde_json::from_value::<VmContinuation>(raw)
        .expect_err("out-of-range format version must be refused as version mismatch");
    let error_msg = decode_error.to_string();
    assert!(
        error_msg.contains("4294967296")
            && error_msg.contains(&VM_CONTINUATION_FORMAT_VERSION.to_string()),
        "the decode error must name both versions: {decode_error}"
    );
    assert!(
        !error_msg.contains("expected u32"),
        "the decode error must be an explicit refusal, not a generic serde type error: {decode_error}"
    );
}
