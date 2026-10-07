use super::*;
use crate::runtime::HeapId;
use lash_sansio::handle::HandleId;

fn empty_continuation(heap: Heap) -> VmContinuation {
    VmContinuation {
        format_version: VM_CONTINUATION_FORMAT_VERSION,
        executable: crate::ExecutableIdentity::unlinked(),
        reference_semantics: false,
        instruction_pointer: 0,
        active_function: None,
        pending_tools: Default::default(),
        execution_nonce: 0,
        operand_stack: Vec::new(),
        last_value: None,
        slots: Vec::new(),
        globals: Record::new(),
        iterator_stack: Vec::new(),
        frame_stack: Vec::new(),
        handler_stack: Vec::new(),
        finally_stack: Vec::new(),
        occurrence_counters: Default::default(),
        mode: ExecutionMode::Process,
        profile: None,
        pending_error_span: None,
        instructions_executed: 0,
        heap: VmHeapContinuation::new(heap),
        resume: VmResumePoint::NextInstruction,
        expired_functions: std::collections::BTreeSet::new(),
    }
}

mod program_validation;
mod structural_validation;

/// The version fence refuses the format one step behind the current one,
/// not just an absurd number.
///
/// Off-by-one is the version a fence actually meets in production — the
/// deploy that straddles a bump — and it is the one a fence written as
/// `< SOME_FLOOR` or `!= 0` would wave through. Both the structural
/// validator and the wire decoder are checked; `resume_from` re-checks the
/// same comparison a third time.
// Pins N's version and bytes; the synthetic N+1 moves them.
#[cfg(not(feature = "synthetic-next"))]
#[test]
fn a_continuation_one_format_version_behind_is_refused() {
    let previous = VM_CONTINUATION_FORMAT_VERSION - 1;
    let mut continuation = empty_continuation(Heap::default());
    continuation.format_version = previous;

    let error = validate_continuation(&continuation)
        .expect_err("the previous format version must be refused");
    assert_eq!(
        error,
        ContinuationError::FormatVersionMismatch {
            expected: VM_CONTINUATION_FORMAT_VERSION,
            found: previous,
        }
    );

    // The same wire deserialized rather than hand-built: the decode refuses
    // before anything reads a field it might not have.
    let wire = serde_json::to_string(&continuation).expect("serialize");
    let decode_error = serde_json::from_str::<VmContinuation>(&wire)
        .expect_err("decoding the previous format version must fail");
    assert!(
        decode_error.to_string().contains(&previous.to_string())
            && decode_error
                .to_string()
                .contains(&VM_CONTINUATION_FORMAT_VERSION.to_string()),
        "the decode error must name both versions: {decode_error}"
    );

    // And the current version still validates, so the assertion above is
    // about the version and nothing else.
    validate_continuation(&empty_continuation(Heap::default()))
        .expect("the current format version validates");
}

/// The last format that metered intrinsics on the old fuel schedule — before
/// `JSON.parse` and `JSON.stringify` charged per byte — is refused typed, so a
/// continuation parked by an older build never resumes under a meter it was
/// not recorded against (FIG-3672).
#[test]
fn a_continuation_from_before_the_intrinsic_fuel_schedule_is_refused() {
    const BEFORE_INTRINSIC_FUEL: u32 = 22;
    let mut continuation = empty_continuation(Heap::default());
    continuation.format_version = BEFORE_INTRINSIC_FUEL;
    assert_eq!(
        validate_continuation(&continuation),
        Err(ContinuationError::FormatVersionMismatch {
            expected: VM_CONTINUATION_FORMAT_VERSION,
            found: BEFORE_INTRINSIC_FUEL,
        })
    );
    let wire = serde_json::to_string(&continuation).expect("serialize");
    let decode_error = serde_json::from_str::<VmContinuation>(&wire)
        .expect_err("a pre-fuel-schedule continuation must not decode");
    assert!(
        decode_error
            .to_string()
            .contains(&BEFORE_INTRINSIC_FUEL.to_string()),
        "the decode error must name the refused version: {decode_error}"
    );
}

/// When an older build reads continuation bytes produced by a newer build that
/// carries enum variants unknown to the older build (e.g. newly minted error brands),
/// the version mismatch must be caught before serde attempts to deserialize
/// variant names into unknown enum values.
#[test]
fn a_continuation_one_format_version_ahead_with_unknown_variant_is_refused_as_version_mismatch() {
    let mut heap = Heap::default();
    let error = heap
        .allocate_error(ErrorKind::EffectError, Some("boom".to_string()), None, None)
        .expect("EffectError");
    let mut continuation = empty_continuation(heap);
    continuation.slots = vec![Some(error)];
    let bytes = serde_json::to_vec(&continuation).expect("serialize continuation");
    let mut future_bytes = bytes.clone();
    let effect_error_pos = future_bytes
        .windows(b"EffectError".len())
        .position(|window| window == b"EffectError")
        .expect("find EffectError");
    future_bytes[effect_error_pos..effect_error_pos + b"EffectError".len()]
        .copy_from_slice(b"FutureError");
    let format_version_needle = b"\"format_version\":";
    let version_pos = future_bytes
        .windows(format_version_needle.len())
        .position(|window| window == format_version_needle)
        .expect("find format_version");
    let version_val_pos = version_pos + format_version_needle.len();
    let version_end = future_bytes[version_val_pos..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .expect("version delimiter")
        + version_val_pos;
    assert_eq!(
        &future_bytes[version_val_pos..version_end],
        VM_CONTINUATION_FORMAT_VERSION.to_string().as_bytes()
    );
    future_bytes.splice(
        version_val_pos..version_end,
        (VM_CONTINUATION_FORMAT_VERSION + 1).to_string().bytes(),
    );

    let decode_error = serde_json::from_slice::<VmContinuation>(&future_bytes).expect_err(
        "newer version with unknown variant must be refused with FormatVersionMismatch",
    );
    let error_msg = decode_error.to_string();
    let next_version = VM_CONTINUATION_FORMAT_VERSION + 1;
    assert!(
        error_msg.contains(&next_version.to_string())
            && error_msg.contains(&VM_CONTINUATION_FORMAT_VERSION.to_string()),
        "the decode error must name both versions: {decode_error}"
    );
    assert!(
        !error_msg.contains("unknown variant"),
        "the decode error must not be misreported as an unknown variant: {decode_error}"
    );
}

#[test]
fn continuation_heap_round_trip_is_canonical_and_rejects_cycles() {
    // A cyclic object used to validate "by identity" and resume. Under the
    // forest invariant it cannot be expressed: the object holds itself, so
    // it has an owner no root can account for.
    let mut cyclic = Heap::default();
    let Value::Ref(root) = cyclic
        .allocate(HeapObject::list(Vec::new()))
        .expect("allocate cyclic root")
    else {
        unreachable!()
    };
    cyclic
        .replace_object(root, HeapObject::list(vec![Value::Ref(root)]))
        .expect("close cycle");
    let mut continuation = empty_continuation(cyclic);
    continuation.slots = vec![Some(Value::Ref(root))];
    let error =
        validate_continuation(&continuation).expect_err("a cyclic continuation must be rejected");
    assert!(
        error.to_string().contains("must have one owner"),
        "unexpected rejection: {error}"
    );

    // The acyclic case still round-trips byte-for-byte, negative zero and
    // all.
    let mut heap = Heap::default();
    let Value::Ref(id) = heap
        .allocate(HeapObject::list(vec![Value::Number(-0.0)]))
        .expect("allocate root")
    else {
        unreachable!()
    };
    let mut continuation = empty_continuation(heap);
    continuation.slots = vec![Some(Value::Ref(id))];
    validate_continuation(&continuation).expect("an owned tree validates");
    let bytes = serde_json::to_vec(&continuation).expect("serialize heap");
    let restored: VmContinuation = serde_json::from_slice(&bytes).expect("restore heap");
    assert_eq!(serde_json::to_vec(&restored).expect("redump heap"), bytes);
    let HeapObject::List { items: values, .. } = restored.heap.heap.get(id).expect("restored root")
    else {
        panic!("root should remain a list")
    };
    let Value::Number(number) = values[0] else {
        panic!("first member should be a number")
    };
    assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
}

/// FIG-3657: same ownness contract as the snapshot wire — the heap error's
/// `message` is `Option<String>` on this wire too, so an absent `message`
/// restores as absent (`Object.hasOwn(e, "message")` is false) and an
/// explicitly empty one restores as present.
#[test]
fn error_message_presence_round_trips_through_the_continuation_wire() {
    let mut heap = Heap::default();
    let absent = heap
        .allocate_error(ErrorKind::Error, None, None, None)
        .expect("new Error()");
    let empty = heap
        .allocate_error(ErrorKind::Error, Some(String::new()), None, None)
        .expect("new Error('')");
    let message = heap
        .allocate_error(ErrorKind::Error, Some("m".to_string()), None, None)
        .expect("new Error('m')");
    let caused = heap
        .allocate_error(
            ErrorKind::Error,
            Some("m".to_string()),
            Some(Value::String("why".into())),
            None,
        )
        .expect("new Error('m', { cause })");
    let mut continuation = empty_continuation(heap);
    // The forest form refuses TypeScript objects outright; a real suspension
    // records this heap in the shared-graph form (`Vm::continuation` retries
    // with `reference_semantics` when the forest validator fails).
    continuation.reference_semantics = true;
    continuation.slots = vec![Some(absent), Some(empty), Some(message), Some(caused)];
    validate_continuation(&continuation).expect("a slot-rooted error heap validates");

    let bytes = serde_json::to_vec(&continuation).expect("serialize continuation");
    let restored: VmContinuation = serde_json::from_slice(&bytes).expect("restore continuation");
    assert_eq!(
        serde_json::to_vec(&restored).expect("redump continuation"),
        bytes,
        "the continuation re-encodes byte for byte"
    );

    let restored_error = |index: usize| -> &ErrorObject {
        let Some(Some(Value::Ref(id))) = restored.slots.get(index) else {
            panic!("slot {index} should restore as a heap reference");
        };
        let HeapObject::Error(error) = restored.heap.heap.get(*id).expect("restored object") else {
            panic!("slot {index} should restore as an error");
        };
        error
    };
    assert_eq!(restored_error(0).message, None);
    assert_eq!(restored_error(1).message.as_deref(), Some(""));
    assert_eq!(restored_error(2).message.as_deref(), Some("m"));
    let caused = restored_error(3);
    assert_eq!(caused.message.as_deref(), Some("m"));
    assert_eq!(caused.cause, Some(Value::String("why".into())));
}

#[test]
fn continuation_numbers_canonicalize_nan_and_preserve_negative_zero() {
    let mut left = empty_continuation(Heap::default());
    left.operand_stack = vec![
        Value::Number(f64::from_bits(0x7ff0_0000_0000_0001)),
        Value::Number(-0.0),
    ];
    let mut right = empty_continuation(Heap::default());
    right.operand_stack = vec![
        Value::Number(f64::from_bits(0xfff8_0000_0000_0042)),
        Value::Number(-0.0),
    ];

    let left_bytes = serde_json::to_vec(&left).expect("serialize NaN continuation");
    let right_bytes = serde_json::to_vec(&right).expect("serialize NaN continuation");
    assert_eq!(left_bytes, right_bytes, "all NaN payloads canonicalize");
    let restored: VmContinuation =
        serde_json::from_slice(&left_bytes).expect("restore NaN continuation");
    assert_eq!(
        serde_json::to_vec(&restored).expect("redump continuation"),
        left_bytes
    );
    let Value::Number(nan) = restored.operand_stack[0] else {
        panic!("expected NaN")
    };
    let Value::Number(negative_zero) = restored.operand_stack[1] else {
        panic!("expected negative zero")
    };
    assert_eq!(nan.to_bits(), 0x7ff8_0000_0000_0000);
    assert_eq!(negative_zero.to_bits(), (-0.0_f64).to_bits());
}

#[test]
fn continuation_decode_rejects_descending_counters_and_dangling_refs() {
    let mut heap = Heap::default();
    heap.allocate(HeapObject::list(Vec::new())).expect("first");
    heap.allocate(HeapObject::list(Vec::new())).expect("second");
    let continuation = empty_continuation(heap);
    let mut descending = serde_json::to_value(&continuation).expect("wire");
    descending["heap"]["objects"]
        .as_array_mut()
        .expect("heap objects")
        .reverse();
    assert!(
        serde_json::from_value::<VmContinuation>(descending)
            .expect_err("descending IDs must fail")
            .to_string()
            .contains("strictly ordered by ID")
    );

    let mut counter = serde_json::to_value(&continuation).expect("wire");
    counter["heap"]["allocation_counter"] = serde_json::json!(u64::MAX);
    assert!(
        serde_json::from_value::<VmContinuation>(counter)
            .expect_err("counter mismatch must fail")
            .to_string()
            .contains("allocation counter cannot advance")
    );

    let mut dangling = empty_continuation(Heap::default());
    dangling.operand_stack = vec![Value::Ref(HeapId::from_counter(99))];
    let bytes = serde_json::to_vec(&dangling).expect("dangling wire");
    assert!(
        serde_json::from_slice::<VmContinuation>(&bytes)
            .expect_err("dangling continuation root must fail")
            .to_string()
            .contains("dangling heap reference")
    );
}

/// FIG-2865: the continuation wire refused `Value::Projected` recursively, so a
/// slot holding `[report]` could not park at all while the `State` snapshot
/// wrote the identical value without complaint. Both writers now use the same
/// canonical shape, a resource projection's name, type and `ResourceRef`
/// (ADR 0132 §9), and a nested occurrence rides it too.
#[test]
fn nested_projection_survives_the_continuation_wire() {
    let report = crate::runtime::ResourceRef {
        projection: crate::runtime::ProjectionType::new("report"),
        id: "7".into(),
        revision: Some("r1".into()),
    };
    let mut continuation = empty_continuation(Heap::default());
    continuation.operand_stack = vec![Value::List(
        vec![Value::Projected(crate::runtime::ProjectedValue::resource(
            "report",
            "string",
            report.clone(),
        ))]
        .into(),
    )];

    let wire = serde_json::to_value(&continuation).expect("continuation should serialize");
    assert_eq!(
        wire["operand_stack"][0]["value"][0],
        serde_json::json!({
            "kind": "projected",
            "value": {
                "kind": "resource",
                "name": "report",
                "type_name": "string",
                "resource": { "projection": "report", "id": "7", "revision": "r1" },
            },
        }),
        "the nested projection must carry the snapshot wire's canonical shape"
    );

    let restored: VmContinuation =
        serde_json::from_value(wire).expect("continuation should deserialize");
    let Some(Value::List(rows)) = restored.operand_stack.first() else {
        panic!("expected the nested container back");
    };
    let Some(Value::Projected(nested)) = rows.first() else {
        panic!("expected a nested projection");
    };
    assert_eq!(nested.name(), "report");
    assert_eq!(nested.value_type_name(), "string");
    assert_eq!(
        nested.resource_ref(),
        Some(&report),
        "the resource must cross the wire unchanged"
    );
}

#[test]
fn heap_header_law_stores_only_the_allocation_counter() {
    let mut heap = Heap::default();
    let root = heap.allocate_list(vec![Value::Bool(true)]).expect("list");
    let mut continuation = empty_continuation(heap.clone());
    continuation.slots.push(Some(root.clone()));
    let wire = serde_json::to_value(&continuation).expect("wire");
    let mut header = wire["heap"].as_object().expect("heap").clone();
    header.remove("objects");
    assert_eq!(
        header,
        serde_json::json!({"allocation_counter": 1})
            .as_object()
            .expect("header")
            .clone()
    );
    let mut state = crate::runtime::State::new();
    state
        .install_runtime([("a".into(), root)].into_iter().collect(), heap)
        .expect("state");
    let snapshot: serde_json::Value =
        rmp_serde::from_slice(&state.snapshot().to_canonical_bytes().expect("snapshot"))
            .expect("snapshot wire");
    for field in [
        "next_id",
        "live_logical_bytes",
        "size_schedule_version",
        "list_holes",
    ] {
        assert!(
            snapshot["heap"].get(field).is_none(),
            "snapshot repeats {field}"
        );
    }
    let parts = state
        .durable_parts(
            &crate::runtime::DurableBaseline::default(),
            lash_core_execution::FleetFormat::current(),
        )
        .expect("parts");
    let header: serde_json::Value = rmp_serde::from_slice(&parts.header).expect("header");
    assert_eq!(
        header["heap"],
        serde_json::json!({"reference_semantics":false,"allocation_counter":1})
    );
    let mut legacy_snapshot = snapshot;
    legacy_snapshot["heap"]["live_logical_bytes"] = serde_json::json!(1);
    assert!(
        crate::runtime::Snapshot::from_canonical_bytes(
            &rmp_serde::to_vec_named(&legacy_snapshot).expect("legacy snapshot")
        )
        .is_err()
    );
    let mut legacy_header = header;
    legacy_header["heap"]["size_schedule_version"] = serde_json::json!(1);
    assert!(
        crate::runtime::State::from_durable_parts(
            &rmp_serde::to_vec_named(&legacy_header).expect("legacy header"),
            std::iter::empty(),
            lash_core_execution::FleetFormat::current(),
        )
        .is_err()
    );
    let mut legacy = wire.clone();
    legacy["heap"]["next_id"] = serde_json::json!(2);
    assert!(
        serde_json::from_value::<VmContinuation>(legacy).is_err(),
        "the old heap shape is refused"
    );
    let restored: VmContinuation = serde_json::from_value(wire).expect("restore");
    assert_eq!(restored.heap.heap.next_id, 2);
    assert_eq!(
        restored.heap.live_logical_bytes(),
        continuation.heap.live_logical_bytes()
    );
}

#[test]
fn sparse_list_law_owns_its_holes_on_the_wire() {
    let mut heap = Heap::default();
    let Value::Ref(id) = heap
        .allocate_list(vec![Value::Undefined, Value::Undefined])
        .expect("list")
    else {
        panic!("reference")
    };
    heap.mark_list_holes(id, [0].into_iter().collect());
    let mut continuation = empty_continuation(heap);
    continuation.slots.push(Some(Value::Ref(id)));
    let wire = serde_json::to_value(&continuation).expect("wire");
    assert_eq!(
        wire["heap"]["objects"][0]["object"]["holes"],
        serde_json::json!([0])
    );
    assert!(wire["heap"].get("list_holes").is_none());
    let restored: VmContinuation = serde_json::from_value(wire.clone()).expect("restore");
    assert!(restored.heap.heap.is_list_hole(id, 0));
    assert!(!restored.heap.heap.is_list_hole(id, 1));
    for holes in [serde_json::json!([0, 0]), serde_json::json!([2])] {
        let mut invalid = wire.clone();
        invalid["heap"]["objects"][0]["object"]["holes"] = holes;
        assert!(serde_json::from_value::<VmContinuation>(invalid).is_err());
    }
}

#[test]
fn pending_operation_law_has_a_tagged_site_and_operands() {
    let mut continuation = empty_continuation(Heap::default());
    let tool = HandleId::tool(0, 0);
    let timer = HandleId::tool(0, 1);
    continuation.pending_tools.insert(
        tool.clone(),
        Some(PendingOperation::Tool {
            site: 3,
            receiver: Value::Null,
            args: vec![Value::Bool(true)],
        }),
    );
    continuation.pending_tools.insert(
        timer.clone(),
        Some(PendingOperation::Timer {
            site: 4,
            duration: Value::Number(10.0),
        }),
    );
    let wire = serde_json::to_value(&continuation).expect("wire");
    assert_eq!(
        wire["pending_tools"][tool.as_str()],
        serde_json::json!({"kind":"tool","site":3,"receiver":{"kind":"null"},"args":[{"kind":"bool","value":true}]})
    );
    assert_eq!(wire["pending_tools"][timer.as_str()]["kind"], "timer");
    assert_eq!(wire["pending_tools"][timer.as_str()]["site"], 4);
    let restored: VmContinuation = serde_json::from_value(wire.clone()).expect("restore");
    assert_eq!(restored.pending_tools, continuation.pending_tools);
    for invalid in [
        serde_json::json!({"kind":"tool","site":3.5,"receiver":{"kind":"null"},"args":[]}),
        serde_json::json!({"kind":"tool","site":3,"receiver":{"kind":"null"},"args":[],"operation":7}),
        serde_json::json!({"kind":"set","value":{"kind":"list","value":[]}}),
    ] {
        let mut corrupted = wire.clone();
        corrupted["pending_tools"][tool.as_str()] = invalid;
        assert!(serde_json::from_value::<VmContinuation>(corrupted).is_err());
    }
}

#[test]
fn execution_nonces_follow_the_recorded_splitmix64_identity() {
    for (seed, expected) in [(0, 0xe220_a839_7b1d_cdafu64), (1, 0x910a_2dec_8902_5cc1)] {
        assert_eq!(mint_execution_nonce(seed), expected);
    }
    let nonces = (0..4096)
        .map(mint_execution_nonce)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(nonces.len(), 4096);
}

#[test]
fn frame_depth_counts_parked_callers() {
    let mut continuation = empty_continuation(Heap::default());
    for depth in 0..=2 {
        assert_eq!(continuation.frame_depth(), depth);
        continuation.frame_stack.push(VmFrameContinuation {
            return_instruction_pointer: 0,
            function: None,
            operand_stack_base: 0,
            slots: Vec::new(),
            globals: Record::new(),
            iterator_stack: Vec::new(),
            return_target: VmFrameReturnContinuation::Direct,
        });
    }
}
