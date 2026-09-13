use super::*;
use crate::runtime::HeapId;

fn empty_continuation(heap: Heap) -> VmContinuation {
    VmContinuation {
        format_version: VM_CONTINUATION_FORMAT_VERSION,
        reference_semantics: false,
        instruction_pointer: 0,
        active_function: None,
        pending_tools: Vec::new(),
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
        active_execution_elapsed: std::time::Duration::ZERO,
        heap: VmHeapContinuation::new(heap),
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
/// validator and the wire decoder are checked; `resume_from` restates the
/// same comparison a third time.
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

/// When an older build reads continuation bytes produced by a newer build that
/// carries enum variants unknown to the older build (e.g. newly minted error brands),
/// the version mismatch must be caught before serde attempts to deserialize
/// variant names into unknown enum values.
#[test]
fn a_continuation_one_format_version_ahead_with_unknown_variant_is_refused_as_version_mismatch() {
    let mut heap = Heap::default();
    let error = heap
        .allocate_error(ErrorKind::EffectError, "boom".to_string(), None, None)
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
        .allocate(HeapObject::List(Vec::new()))
        .expect("allocate cyclic root")
    else {
        unreachable!()
    };
    cyclic
        .replace_object(root, HeapObject::List(vec![Value::Ref(root)]))
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
        .allocate(HeapObject::List(vec![Value::Number(-0.0)]))
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
    let HeapObject::List(values) = restored.heap.heap.get(id).expect("restored root") else {
        panic!("root should remain a list")
    };
    let Value::Number(number) = values[0] else {
        panic!("first member should be a number")
    };
    assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
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
fn continuation_decode_rejects_regexp_last_index_above_maximum_safe_length() {
    let mut heap = Heap::default();
    let regexp = heap
        .allocate_regexp("a+".to_string(), "g".to_string())
        .expect("RegExp");
    let mut continuation = empty_continuation(heap);
    continuation.reference_semantics = true;
    continuation.operand_stack.push(regexp);
    let mut wire = serde_json::to_value(&continuation).expect("continuation wire");
    wire["heap"]["objects"][0]["object"]["last_index"] =
        serde_json::json!(crate::runtime::heap::MAX_JAVASCRIPT_LENGTH + 1);
    let error = serde_json::from_value::<VmContinuation>(wire)
        .expect_err("out-of-range lastIndex must not decode");
    assert!(error.to_string().contains("maximum safe length"), "{error}");
}

#[test]
fn continuation_decode_rejects_descending_counters_and_dangling_refs() {
    let mut heap = Heap::default();
    heap.allocate(HeapObject::List(Vec::new())).expect("first");
    heap.allocate(HeapObject::List(Vec::new())).expect("second");
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
    counter["heap"]["next_id"] = serde_json::json!(1000);
    assert!(
        serde_json::from_value::<VmContinuation>(counter)
            .expect_err("counter mismatch must fail")
            .to_string()
            .contains("allocation counter plus one")
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

/// A host descriptor that answers nothing: only its identity crosses the wire.
#[derive(Default)]
struct WireProbeDescriptor;

impl crate::runtime::ProjectedHostDescriptor for WireProbeDescriptor {
    fn type_name(&self) -> &str {
        "string"
    }
}

/// FIG-2865: the continuation wire refused `Value::Projected` recursively, so a
/// slot holding `[report]` could not park at all while the `State` snapshot
/// wrote the identical value without complaint. Both writers now use the same
/// canonical three-field shape, and a nested occurrence rides it too.
#[test]
fn nested_projection_survives_the_continuation_wire() {
    let mut continuation = empty_continuation(Heap::default());
    continuation.operand_stack = vec![Value::List(
        vec![Value::Projected(
            crate::runtime::ProjectedValue::custom_with_projection_ref(
                "report",
                std::sync::Arc::new(WireProbeDescriptor),
                serde_json::json!({ "kind": "report", "id": 7 }),
            ),
        )]
        .into(),
    )];

    let wire = serde_json::to_value(&continuation).expect("continuation should serialize");
    assert_eq!(
        wire["operand_stack"][0]["value"][0],
        serde_json::json!({
            "kind": "projected",
            "value": {
                "name": "report",
                "type_name": "string",
                "projection_ref": {
                    "kind": "object",
                    "fields": [
                        { "name": "id", "value": { "kind": "number", "value": 7 } },
                        { "name": "kind", "value": { "kind": "string", "value": "report" } },
                    ],
                },
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
        panic!("expected a nested projected placeholder");
    };
    assert_eq!(nested.name(), "report");
    assert_eq!(nested.value_type_name(), "string");
    assert_eq!(
        nested.projection_ref(),
        Some(&serde_json::json!({ "kind": "report", "id": 7 })),
        "`projection_ref` must cross the wire unchanged"
    );
}
