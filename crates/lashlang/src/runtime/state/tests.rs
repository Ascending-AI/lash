use super::*;
use crate::ast::{AssignTarget, Expr, FunctionExpr, Program};
use crate::runtime::HEAP_SIZE_SCHEDULE_VERSION;
use crate::runtime::entry_points::compile_program_internal;

#[test]
fn decoded_snapshots_validate_closure_metadata_when_paired_with_a_program() {
    let program = compile_program_internal(&Program::block(vec![
        Expr::Assign {
            target: AssignTarget::variable("captured".into()),
            expr: Box::new(Expr::Number(1.0)),
        },
        Expr::Assign {
            target: AssignTarget::variable("f".into()),
            expr: Box::new(Expr::Function(Box::new(FunctionExpr {
                name: None,
                params: Vec::new(),
                captures: vec!["captured".into()],
                body: Box::new(Expr::Variable("captured".into())),
            }))),
        },
    ]));

    for captures in [Vec::new(), vec![Value::Null, Value::Bool(true)]] {
        let mut heap = Heap::default();
        let closure = heap
            .allocate(HeapObject::Closure {
                function: 0,
                captures,
            })
            .expect("allocate malformed snapshot closure");
        let mut runtime_globals = Record::new();
        runtime_globals.insert("f".to_string(), closure);
        let snapshot = Snapshot {
            globals: Record::new(),
            runtime_globals,
            heap,
            reference_semantics: false,
        };
        let bytes = snapshot
            .to_canonical_bytes()
            .expect("program-independent snapshot encoding accepts closure metadata");
        let decoded = Snapshot::from_canonical_bytes(&bytes)
            .expect("program-independent snapshot decoding accepts closure metadata");
        let mut state = State::from_snapshot(decoded);
        assert!(matches!(
            state.validate_program(&program),
            Err(RuntimeError::ClosureCaptureCountMismatch {
                index: 0,
                expected: 1,
                ..
            })
        ));
    }

    let mut heap = Heap::default();
    let closure = heap
        .allocate(HeapObject::Closure {
            function: 99,
            captures: Vec::new(),
        })
        .expect("allocate unknown snapshot closure");
    let mut runtime_globals = Record::new();
    runtime_globals.insert("f".to_string(), closure);
    let bytes = Snapshot {
        globals: Record::new(),
        runtime_globals,
        heap,
        reference_semantics: false,
    }
    .to_canonical_bytes()
    .expect("program-independent snapshot encoding accepts function metadata");
    let decoded = Snapshot::from_canonical_bytes(&bytes)
        .expect("program-independent snapshot decoding accepts function metadata");
    assert!(matches!(
        State::from_snapshot(decoded).validate_program(&program),
        Err(RuntimeError::UnknownFunction { index: 99 })
    ));
}

#[test]
fn canonical_encoding_is_deterministic_for_map_order_and_nan_payload() {
    let left_nan = f64::from_bits(0x7ff0_0000_0000_0001);
    let right_nan = f64::from_bits(0xfff8_0000_0000_0042);

    let mut left_record = Record::new();
    left_record.insert("z".to_string(), Value::Number(left_nan));
    left_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
    let mut left_globals = Record::new();
    left_globals.insert("z-last".to_string(), Value::Bool(true));
    left_globals.insert("session".to_string(), Value::Record(Arc::new(left_record)));

    let mut right_record = Record::new();
    right_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
    right_record.insert("z".to_string(), Value::Number(right_nan));
    let mut right_globals = Record::new();
    right_globals.insert("session".to_string(), Value::Record(Arc::new(right_record)));
    right_globals.insert("z-last".to_string(), Value::Bool(true));

    let left = Snapshot::new(left_globals)
        .to_canonical_bytes()
        .expect("left encode");
    let right = Snapshot::new(right_globals)
        .to_canonical_bytes()
        .expect("right encode");

    assert_eq!(left, right);
}

#[test]
fn canonical_decode_rejects_non_minimal_integer_width_with_location() {
    let snapshot = Snapshot::new(
        [(
            "root".to_string(),
            Value::Projected(
                ProjectedValue::unavailable_after_restore_with_projection_ref(
                    "root",
                    "number",
                    Some(serde_json::json!(1)),
                ),
            ),
        )]
        .into_iter()
        .collect(),
    );
    let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
    let needle = [0xa5, b'v', b'a', b'l', b'u', b'e', 0x01];
    let offset = bytes
        .windows(needle.len())
        .rposition(|window| window == needle)
        .expect("projection JSON integer");
    bytes.splice(
        offset + needle.len() - 1..offset + needle.len(),
        [0xcc, 0x01],
    );

    let error = Snapshot::from_canonical_bytes(&bytes)
        .expect_err("non-minimal integer width must be rejected");
    assert!(
        matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals.root.value.projection_ref.value"
                    && reason.contains("integer width is not minimal")
        ),
        "{error:?}"
    );
}

#[test]
fn canonical_decode_rejects_integer_encoded_runtime_number() {
    let snapshot = Snapshot::new(
        [("root".to_string(), Value::Number(1.0))]
            .into_iter()
            .collect(),
    );
    let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
    let mut needle = vec![0xa5, b'v', b'a', b'l', b'u', b'e', 0xcb];
    needle.extend_from_slice(&1.0_f64.to_bits().to_be_bytes());
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("runtime f64");
    bytes.splice(offset + 6..offset + needle.len(), [0x01]);

    let error = Snapshot::from_canonical_bytes(&bytes)
        .expect_err("integer-encoded runtime number must be rejected");
    assert!(matches!(
        &error,
        SnapshotDecodeError::NonCanonicalEncoding { location, reason }
            if location == "globals.root.value"
                && reason.contains("must use f64 encoding")
    ));
}

#[test]
fn canonical_decode_rejects_sequence_form_structs() {
    let wire = CanonicalSnapshot {
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Null {},
        }]),
        heap: None,
    };
    let bytes = rmp_serde::to_vec(&wire).expect("sequence-form bytes");

    let error =
        Snapshot::from_canonical_bytes(&bytes).expect_err("sequence-form structs must be rejected");
    assert!(
        matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "snapshot" && reason.contains("map form, not sequence form")
        ),
        "{error:?}"
    );
}

#[test]
fn canonical_decode_rejects_unsorted_and_duplicate_dynamic_keys() {
    for names in [["z", "a"], ["same", "same"]] {
        let wire = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: Some(
                names
                    .into_iter()
                    .map(|name| CanonicalBinding {
                        name: name.to_string(),
                        value: CanonicalValue::Null {},
                    })
                    .collect(),
            ),
            heap: None,
        };
        let bytes = rmp_serde::to_vec_named(&wire).expect("non-canonical bytes");

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("dynamic keys must be sorted and unique");
        assert!(matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals"
                    && reason.contains("strictly sorted and unique")
        ));
    }
}

#[test]
fn canonical_encode_error_names_the_nested_value_path() {
    let mut too_deep = Value::Null;
    for _ in 0..=MAX_SNAPSHOT_VALUE_DEPTH {
        too_deep = Value::List(vec![too_deep].into());
    }
    let mut session = Record::new();
    session.insert(
        "items".to_string(),
        Value::List(vec![Value::Null, Value::Null, Value::Null, too_deep].into()),
    );
    let snapshot = Snapshot::new(
        [("session".to_string(), Value::Record(Arc::new(session)))]
            .into_iter()
            .collect(),
    );

    let error = snapshot
        .to_canonical_bytes()
        .expect_err("over-depth value must fail at encode");
    let ContinuationError::UnserializableValue { location, variant } = error else {
        panic!("expected typed unserializable-value error");
    };
    assert!(
        location.starts_with("globals.session.items[3]"),
        "{location}"
    );
    assert_eq!(variant, "value beyond the snapshot depth limit");
}

#[test]
fn canonical_decode_rejects_a_depth_bomb_before_deserializing() {
    let mut value = CanonicalValue::Null {};
    for _ in 0..120 {
        value = CanonicalValue::List { items: vec![value] };
    }
    let bomb = CanonicalSnapshot {
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "bomb".to_string(),
            value,
        }]),
        heap: None,
    };
    let bytes = rmp_serde::to_vec_named(&bomb).expect("construct depth bomb");

    assert_eq!(
        Snapshot::from_canonical_bytes(&bytes),
        Err(SnapshotDecodeError::ValueDepthLimitExceeded {
            limit: MAX_SNAPSHOT_VALUE_DEPTH,
        })
    );
}

#[test]
fn canonical_wire_golden_covers_every_value_kind_and_projection_ref() {
    let image = ImageValue::new(
        "sha256:00ff",
        crate::MediaType::parse("image/png").expect("media type"),
        "pixel",
        2,
        Some(1),
        Some(1),
    );
    let projection_ref = serde_json::json!({
        "array": [null, true, 7, "bytes\u{0000}\u{007f}"],
        "object": {"key": "value"}
    });
    let snapshot = Snapshot::new(
        [
            ("bool".to_string(), Value::Bool(true)),
            ("image".to_string(), Value::Image(Box::new(image))),
            ("list".to_string(), Value::List(vec![Value::Null].into())),
            ("null".to_string(), Value::Null),
            ("number".to_string(), Value::Number(-12.5)),
            (
                "projected".to_string(),
                Value::Projected(
                    ProjectedValue::unavailable_after_restore_with_projection_ref(
                        "memory",
                        "object",
                        Some(projection_ref),
                    ),
                ),
            ),
            (
                "record".to_string(),
                Value::Record(Arc::new(
                    [("field".to_string(), Value::String("body".into()))]
                        .into_iter()
                        .collect(),
                )),
            ),
            (
                "resource".to_string(),
                Value::Resource(ResourceHandle::new("files", "workspace")),
            ),
            (
                "string".to_string(),
                Value::String("body\u{0000}\u{007f}".into()),
            ),
            (
                "tuple".to_string(),
                Value::Tuple(vec![Value::Number(1.0), Value::String("two".into())].into()),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let bytes = snapshot.to_canonical_bytes().expect("golden snapshot");
    use sha2::Digest as _;
    assert_eq!(bytes.len(), 884);
    assert_eq!(
        sha2::Sha256::digest(&bytes).as_slice(),
        &[
            0x1e, 0xee, 0x25, 0xca, 0xe4, 0x1c, 0x2f, 0xe9, 0x46, 0x5f, 0x33, 0x6f, 0x61, 0xc7,
            0xf0, 0xa9, 0x7f, 0x9b, 0x10, 0xca, 0xa1, 0xe9, 0x31, 0x49, 0x36, 0xc6, 0xb4, 0xca,
            0x2d, 0xae, 0x6d, 0xe2,
        ]
    );
}

#[test]
fn snapshot_round_trip_preserves_undefined_cell_global() {
    let mut inner_record = Record::new();
    inner_record.insert("nested_missing".to_string(), Value::Undefined);
    let mut globals = Record::new();
    globals.insert("missing".to_string(), Value::Undefined);
    globals.insert("nested".to_string(), Value::Record(Arc::new(inner_record)));
    globals.insert(
        "list_with_undefined".to_string(),
        Value::List(vec![Value::Undefined, Value::Number(1.0)].into()),
    );
    globals.insert(
        "tuple_with_undefined".to_string(),
        Value::Tuple(vec![Value::Undefined, Value::String("a".into())].into()),
    );
    let snapshot = Snapshot::new(globals);
    let bytes = snapshot
        .to_canonical_bytes()
        .expect("canonical snapshot encode with undefined global");
    let decoded = Snapshot::from_canonical_bytes(&bytes)
        .expect("canonical snapshot decode with undefined global");
    assert_eq!(decoded.globals().get("missing"), Some(&Value::Undefined));
    assert_eq!(
        decoded.globals().get("nested"),
        Some(&Value::Record(Arc::new(
            [("nested_missing".to_string(), Value::Undefined)]
                .into_iter()
                .collect()
        )))
    );
    assert_eq!(
        decoded.globals().get("list_with_undefined"),
        Some(&Value::List(
            vec![Value::Undefined, Value::Number(1.0)].into()
        ))
    );
    assert_eq!(
        decoded.globals().get("tuple_with_undefined"),
        Some(&Value::Tuple(
            vec![Value::Undefined, Value::String("a".into())].into()
        ))
    );
}

#[test]
fn canonical_decode_rejects_extra_fields_on_undefined_value() {
    // A malformed canonical wire where undefined has extra fields
    let wire = CanonicalSnapshot {
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Undefined {},
        }]),
        heap: None,
    };
    let bytes = rmp_serde::to_vec_named(&wire).expect("canonical wire");
    // Change map length from 1 to 2 by patching byte 0x81 -> 0x82 and appending another field
    // Wire structure: 0x82 (map of 2) ... "globals" -> [ { "name": "root", "value": { "kind": "undefined" } } ]
    // Let's locate the undefined value map 0x81 0xa4 "kind" 0xa9 "undefined"
    let needle = [
        0x81, 0xa4, b'k', b'i', b'n', b'd', 0xa9, b'u', b'n', b'd', b'e', b'f', b'i', b'n', b'e',
        b'd',
    ];
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("found undefined wire value");
    // Replace 0x81 (map of 1) with 0x82 (map of 2) and append an extra key-value pair "extra": 1
    let mut patched = bytes[..offset].to_vec();
    patched.push(0x82);
    patched.extend_from_slice(&needle[1..]);
    patched.extend_from_slice(&[0xa5, b'e', b'x', b't', b'r', b'a', 0x01]);
    let suffix_start = offset + needle.len();
    patched.extend_from_slice(&bytes[suffix_start..]);

    let error = Snapshot::from_canonical_bytes(&patched)
        .expect_err("undefined with extra fields must be rejected");
    assert!(
        matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals.root"
                    && reason.contains("undefined value must contain only its kind")
        ),
        "{error:?}"
    );
}

#[test]
fn canonical_runtime_value_validator_covers_every_canonical_value_variant() {
    fn validate_wire_value(value: CanonicalValue) -> Result<Snapshot, SnapshotDecodeError> {
        let wire = CanonicalSnapshot {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: Some(vec![CanonicalBinding {
                name: "root".to_string(),
                value,
            }]),
            heap: None,
        };
        Snapshot::from_canonical_bytes(&rmp_serde::to_vec_named(&wire).expect("serialize wire"))
    }

    let variants = vec![
        CanonicalValue::Null {},
        CanonicalValue::Undefined {},
        CanonicalValue::Bool { value: true },
        CanonicalValue::Number { value: 42.0 },
        CanonicalValue::String {
            value: "hello".to_string(),
        },
        CanonicalValue::Image {
            value: ImageValue::new(
                "sha256:00ff",
                crate::MediaType::parse("image/png").expect("media type"),
                "pixel",
                2,
                Some(1),
                Some(1),
            ),
        },
        CanonicalValue::Resource {
            value: ResourceHandle::new("files", "workspace"),
        },
        CanonicalValue::Tuple {
            items: vec![CanonicalValue::Null {}],
        },
        CanonicalValue::List {
            items: vec![CanonicalValue::Undefined {}],
        },
        CanonicalValue::Record {
            fields: vec![CanonicalBinding {
                name: "field".to_string(),
                value: CanonicalValue::Undefined {},
            }],
        },
        CanonicalValue::Projected {
            value: CanonicalProjectedValue {
                name: "root".to_string(),
                type_name: "object".to_string(),
                projection_ref: Some(CanonicalJsonValue::Null {}),
            },
        },
    ];

    // Compile-time exhaustiveness witness for CanonicalValue variants.
    // If a new variant is added to CanonicalValue without updating this test,
    // this match will fail to compile.
    fn witness_variant_exhaustiveness(variant: &CanonicalValue) {
        match variant {
            CanonicalValue::Null {} => {}
            CanonicalValue::Undefined {} => {}
            CanonicalValue::Bool { .. } => {}
            CanonicalValue::Number { .. } => {}
            CanonicalValue::String { .. } => {}
            CanonicalValue::Image { .. } => {}
            CanonicalValue::Resource { .. } => {}
            CanonicalValue::Ref { .. } => {}
            CanonicalValue::Tuple { .. } => {}
            CanonicalValue::List { .. } => {}
            CanonicalValue::Record { .. } => {}
            CanonicalValue::Projected { .. } => {}
        }
    }

    for variant in variants {
        witness_variant_exhaustiveness(&variant);
        let result = validate_wire_value(variant);
        assert!(
            result.is_ok(),
            "validate_runtime_value must accept every canonical value variant: {result:?}"
        );
    }
}

#[test]
fn canonical_empty_heap_has_exact_golden_bytes() {
    let bytes = Snapshot::default()
        .to_canonical_bytes()
        .expect("empty canonical snapshot");
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(hex, "82a776657273696f6e07a7676c6f62616c7390");
}

#[test]
fn plain_scalar_snapshot_has_no_heap_duplicate() {
    let bytes = Snapshot::new([("value".to_string(), Value::Null)].into_iter().collect())
        .to_canonical_bytes()
        .expect("scalar snapshot");

    assert_eq!(bytes.len(), 48, "scalar snapshot shape changed");
    assert!(!String::from_utf8_lossy(&bytes).contains("heap"));
}

#[test]
fn canonical_decode_accepts_every_max_depth_encode_shape() {
    fn round_trip(value: Value) {
        let snapshot = Snapshot::new([("root".to_string(), value)].into_iter().collect());
        let bytes = snapshot.to_canonical_bytes().expect("max-depth encode");
        let decoded = Snapshot::from_canonical_bytes(&bytes).expect("max-depth decode");
        assert_eq!(decoded, snapshot);
    }

    let mut record = Value::Null;
    for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
        record = Value::Record(Arc::new(
            [("child".to_string(), record)].into_iter().collect(),
        ));
    }
    round_trip(record);

    let mut list = Value::Null;
    for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
        list = Value::List(vec![list].into());
    }
    round_trip(list);

    let mut projection_ref = serde_json::Value::Null;
    // `Projected` enters its JSON payload at depth one, so 63 nested
    // objects place the terminal null at the shared depth limit of 64.
    for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH - 1 {
        projection_ref = serde_json::json!({"child": projection_ref});
    }
    round_trip(Value::Projected(
        ProjectedValue::unavailable_after_restore_with_projection_ref(
            "root",
            "object",
            Some(projection_ref),
        ),
    ));
}

fn canonical_heap_with(
    roots: Vec<CanonicalBinding>,
    objects: Vec<CanonicalHeapEntry>,
    next_id: u64,
    allocation_counter: u64,
    live_logical_bytes: u64,
) -> CanonicalSnapshot {
    CanonicalSnapshot {
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: None,
        heap: Some(CanonicalHeap {
            reference_semantics: false,
            next_id,
            allocation_counter,
            live_logical_bytes,
            size_schedule_version: HEAP_SIZE_SCHEDULE_VERSION,
            roots,
            objects,
        }),
    }
}

fn named_bytes(wire: &CanonicalSnapshot) -> Vec<u8> {
    rmp_serde::to_vec_named(wire).expect("encode test wire")
}

/// The snapshot version fence refuses the format one step behind the current
/// one, not just an absurd number.
///
/// Off-by-one is the version a fence actually meets in production — the deploy
/// that straddles a bump — so the case worth pinning is `current - 1`, on a
/// wire that is otherwise entirely valid and decodes cleanly at the current
/// version.
#[test]
fn a_snapshot_one_version_behind_is_refused_by_the_fence() {
    let mut globals = Record::new();
    globals.insert("total".to_string(), Value::Number(3.0));
    let bytes = Snapshot::new(globals)
        .to_canonical_bytes()
        .expect("encode a snapshot");
    Snapshot::from_canonical_bytes(&bytes).expect("the current version must decode");

    let mut wire: CanonicalSnapshot = rmp_serde::from_slice(&bytes).expect("decode the wire");
    wire.version = LASHLANG_SNAPSHOT_VERSION - 1;
    let error = Snapshot::from_canonical_bytes(&named_bytes(&wire))
        .expect_err("the previous snapshot version must be refused");
    assert_eq!(
        error,
        SnapshotDecodeError::VersionMismatch {
            expected: LASHLANG_SNAPSHOT_VERSION,
            found: LASHLANG_SNAPSHOT_VERSION - 1,
        }
    );
}

/// When an older build reads snapshot bytes produced by a newer build that
/// carries enum variants unknown to the older build (e.g. newly minted error brands),
/// the version mismatch must be caught during the raw-byte validation pass before
/// serde attempts to deserialize variant names into unknown enum values.
#[test]
fn a_snapshot_one_version_ahead_with_unknown_variant_is_refused_as_version_mismatch() {
    let mut heap = Heap::default();
    let error = heap
        .allocate_error(ErrorKind::EffectError, "boom".to_string(), None, None)
        .expect("EffectError");
    let mut roots = Record::new();
    roots.insert("rejection".to_string(), error);
    let snapshot = Snapshot {
        globals: Record::new(),
        runtime_globals: roots,
        heap,
        reference_semantics: true,
    };
    let bytes = snapshot.to_canonical_bytes().expect("encode snapshot");
    let mut future_bytes = bytes.clone();
    let effect_error_pos = future_bytes
        .windows(b"EffectError".len())
        .position(|window| window == b"EffectError")
        .expect("find EffectError");
    future_bytes[effect_error_pos..effect_error_pos + b"EffectError".len()]
        .copy_from_slice(b"FutureError");
    let version_pos = future_bytes
        .windows(b"version".len())
        .position(|window| window == b"version")
        .expect("find version");
    let version_val_pos = version_pos + b"version".len();
    assert_eq!(
        future_bytes[version_val_pos],
        LASHLANG_SNAPSHOT_VERSION as u8
    );
    future_bytes[version_val_pos] = (LASHLANG_SNAPSHOT_VERSION + 1) as u8;

    let error = Snapshot::from_canonical_bytes(&future_bytes)
        .expect_err("newer version with unknown variant must be refused with VersionMismatch");
    assert_eq!(
        error,
        SnapshotDecodeError::VersionMismatch {
            expected: LASHLANG_SNAPSHOT_VERSION,
            found: LASHLANG_SNAPSHOT_VERSION + 1,
        }
    );
}

/// A minted error brand ships on the wire *by name*, which is why adding one is
/// a format bump and not an additive change.
///
/// `error_kind` is serialized as its variant name, so a reader that predates a
/// brand meets an unknown variant while deserializing — strictly before it can
/// compare `version` — and would report a corrupt snapshot instead of a version
/// boundary. Pinning the literal name on the wire keeps that reasoning checkable:
/// if a future brand is added without moving `LASHLANG_SNAPSHOT_VERSION`, this is
/// the test that says why it must.
#[test]
fn a_minted_error_brand_ships_by_name_and_round_trips_at_the_current_version() {
    let mut heap = Heap::default();
    let cause = heap
        .allocate_record(
            [(
                "code".to_string(),
                Value::String("ResourceOperationFailed".into()),
            )]
            .into_iter()
            .collect(),
        )
        .expect("cause record");
    let error = heap
        .allocate_error(
            ErrorKind::EffectError,
            "boom".to_string(),
            Some(cause),
            None,
        )
        .expect("EffectError");
    let mut roots = Record::new();
    roots.insert("rejection".to_string(), error);
    let snapshot = Snapshot {
        globals: Record::new(),
        runtime_globals: roots,
        heap,
        reference_semantics: true,
    };

    let bytes = snapshot.to_canonical_bytes().expect("encode snapshot");
    assert!(
        bytes
            .windows("EffectError".len())
            .any(|window| window == b"EffectError"),
        "the brand travels as its own name, so an older reader cannot decode it"
    );
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("decode snapshot");
    assert_eq!(
        restored
            .to_canonical_bytes()
            .expect("re-encode the snapshot"),
        bytes,
        "the brand survives the decode as itself, byte for byte"
    );
}

#[test]
fn canonical_decode_rejects_descending_heap_ids() {
    let wire = canonical_heap_with(
        vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Ref {
                value: HeapId::from_counter(1),
            },
        }],
        vec![
            CanonicalHeapEntry {
                id: HeapId::from_counter(2),
                object: CanonicalHeapObject::List { items: Vec::new() },
            },
            CanonicalHeapEntry {
                id: HeapId::from_counter(1),
                object: CanonicalHeapObject::List { items: Vec::new() },
            },
        ],
        3,
        2,
        2 * super::super::heap::HeapObject::List(Vec::new()).logical_bytes(),
    );

    let error = Snapshot::from_canonical_bytes(&named_bytes(&wire))
        .expect_err("descending IDs must be rejected");
    assert!(error.to_string().contains("strictly ordered by ID"));
}

#[test]
fn canonical_decode_rejects_dangling_root_and_nested_references() {
    let dangling_root = canonical_heap_with(
        vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Ref {
                value: HeapId::from_counter(99),
            },
        }],
        Vec::new(),
        1,
        0,
        0,
    );
    let error = Snapshot::from_canonical_bytes(&named_bytes(&dangling_root))
        .expect_err("dangling root must be rejected");
    assert!(error.to_string().contains("dangling heap reference 99"));

    let member_object =
        super::super::heap::HeapObject::List(vec![Value::Ref(HeapId::from_counter(99))]);
    let dangling_member = canonical_heap_with(
        Vec::new(),
        vec![CanonicalHeapEntry {
            id: HeapId::from_counter(1),
            object: CanonicalHeapObject::List {
                items: vec![CanonicalValue::Ref {
                    value: HeapId::from_counter(99),
                }],
            },
        }],
        2,
        1,
        member_object.logical_bytes(),
    );
    let error = Snapshot::from_canonical_bytes(&named_bytes(&dangling_member))
        .expect_err("dangling member ref must be rejected");
    assert!(error.to_string().contains("dangling heap reference 99"));

    // An inline compound inside a heap object is rejected outright, so a
    // reference can never hide below the member level in an accepted wire.
    let nested_object = super::super::heap::HeapObject::List(vec![Value::List(
        vec![Value::Ref(HeapId::from_counter(99))].into(),
    )]);
    let inline_compound_member = canonical_heap_with(
        Vec::new(),
        vec![CanonicalHeapEntry {
            id: HeapId::from_counter(1),
            object: CanonicalHeapObject::List {
                items: vec![CanonicalValue::List {
                    items: vec![CanonicalValue::Ref {
                        value: HeapId::from_counter(99),
                    }],
                }],
            },
        }],
        2,
        1,
        nested_object.logical_bytes(),
    );
    let error = Snapshot::from_canonical_bytes(&named_bytes(&inline_compound_member))
        .expect_err("inline compound members must be rejected");
    assert!(
        error
            .to_string()
            .contains("heap object members must be scalars or heap references")
    );
}

#[test]
fn canonical_decode_rejects_counter_accounting_schedule_and_root_order() {
    let empty_object_bytes = super::super::heap::HeapObject::List(Vec::new()).logical_bytes();
    let object = CanonicalHeapEntry {
        id: HeapId::from_counter(1),
        object: CanonicalHeapObject::List { items: Vec::new() },
    };
    let counter = canonical_heap_with(
        Vec::new(),
        vec![object.clone()],
        1000,
        1,
        empty_object_bytes,
    );
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&counter))
            .expect_err("counter mismatch")
            .to_string()
            .contains("allocation counter plus one")
    );

    let accounting = canonical_heap_with(Vec::new(), vec![object.clone()], 2, 1, 0);
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&accounting))
            .expect_err("accounting mismatch")
            .to_string()
            .contains("logical byte counter")
    );

    let mut schedule = canonical_heap_with(Vec::new(), vec![object], 2, 1, empty_object_bytes);
    schedule.heap.as_mut().expect("heap").size_schedule_version += 1;
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&schedule))
            .expect_err("schedule mismatch")
            .to_string()
            .contains("size schedule version")
    );

    let roots = vec![
        CanonicalBinding {
            name: "z".to_string(),
            value: CanonicalValue::Null {},
        },
        CanonicalBinding {
            name: "a".to_string(),
            value: CanonicalValue::Null {},
        },
    ];
    let root_order = canonical_heap_with(roots, Vec::new(), 1, 0, 0);
    assert!(matches!(
        Snapshot::from_canonical_bytes(&named_bytes(&root_order)),
        Err(SnapshotDecodeError::NonCanonicalEncoding { location, .. })
            if location == "heap.roots"
    ));
}

#[test]
fn canonical_decode_rejects_shared_roots_cycles_and_unreachable_objects() {
    let id = HeapId::from_counter(1);
    let empty_bytes = super::super::heap::HeapObject::List(Vec::new()).logical_bytes();
    let shared = canonical_heap_with(
        vec![
            CanonicalBinding {
                name: "a".to_string(),
                value: CanonicalValue::Ref { value: id },
            },
            CanonicalBinding {
                name: "b".to_string(),
                value: CanonicalValue::Ref { value: id },
            },
        ],
        vec![CanonicalHeapEntry {
            id,
            object: CanonicalHeapObject::List { items: Vec::new() },
        }],
        2,
        1,
        empty_bytes,
    );
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&shared))
            .expect_err("shared roots must be rejected")
            .to_string()
            .contains("must have one owner")
    );

    let cyclic_object = super::super::heap::HeapObject::List(vec![Value::Ref(id)]);
    let cycle = canonical_heap_with(
        vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Ref { value: id },
        }],
        vec![CanonicalHeapEntry {
            id,
            object: CanonicalHeapObject::List {
                items: vec![CanonicalValue::Ref { value: id }],
            },
        }],
        2,
        1,
        cyclic_object.logical_bytes(),
    );
    // A rooted self-cycle is refused as a second owner: the root holds the
    // object and so does the object itself.
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&cycle))
            .expect_err("cycles must be rejected")
            .to_string()
            .contains("must have one owner")
    );

    // A cycle no root names has one owner per object and still must not
    // decode: nothing outside the cycle holds it up.
    let second = HeapId::from_counter(2);
    let ring_first = super::super::heap::HeapObject::List(vec![Value::Ref(second)]);
    let ring_second = super::super::heap::HeapObject::List(vec![Value::Ref(id)]);
    let ring = canonical_heap_with(
        Vec::new(),
        vec![
            CanonicalHeapEntry {
                id,
                object: CanonicalHeapObject::List {
                    items: vec![CanonicalValue::Ref { value: second }],
                },
            },
            CanonicalHeapEntry {
                id: second,
                object: CanonicalHeapObject::List {
                    items: vec![CanonicalValue::Ref { value: id }],
                },
            },
        ],
        3,
        2,
        ring_first.logical_bytes() + ring_second.logical_bytes(),
    );
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&ring))
            .expect_err("an unrooted cycle must be rejected")
            .to_string()
            .contains("acyclic")
    );

    // A repeated reference inside one root is a DAG, not a tree, and is
    // refused even though only one root names it.
    let diamond_child = super::super::heap::HeapObject::List(Vec::new());
    let diamond_root =
        super::super::heap::HeapObject::List(vec![Value::Ref(second), Value::Ref(second)]);
    let diamond = canonical_heap_with(
        vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Ref { value: id },
        }],
        vec![
            CanonicalHeapEntry {
                id,
                object: CanonicalHeapObject::List {
                    items: vec![
                        CanonicalValue::Ref { value: second },
                        CanonicalValue::Ref { value: second },
                    ],
                },
            },
            CanonicalHeapEntry {
                id: second,
                object: CanonicalHeapObject::List { items: Vec::new() },
            },
        ],
        3,
        2,
        diamond_root.logical_bytes() + diamond_child.logical_bytes(),
    );
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&diamond))
            .expect_err("a within-root diamond must be rejected")
            .to_string()
            .contains("must have one owner")
    );

    let unreachable = canonical_heap_with(
        Vec::new(),
        vec![CanonicalHeapEntry {
            id,
            object: CanonicalHeapObject::List { items: Vec::new() },
        }],
        2,
        1,
        empty_bytes,
    );
    assert!(
        Snapshot::from_canonical_bytes(&named_bytes(&unreachable))
            .expect_err("unreachable objects must be rejected")
            .to_string()
            .contains("unreachable objects")
    );
}

/// The heap form's value depth is bounded like the tree form's.
///
/// A chain of objects is a flat wire — every object holds one scalar and one
/// reference — so the MessagePack structure guard sees nothing deep. What is
/// deep is the value a root materializes into, and reading it is what would
/// overflow. The bound is enforced against the object chain, before anything
/// materializes.
#[test]
fn canonical_decode_rejects_a_heap_chain_deeper_than_the_value_limit() {
    fn chain_snapshot(depth: usize) -> Vec<u8> {
        let mut objects = Vec::new();
        let mut bytes = 0;
        for index in 0..depth {
            let id = HeapId::from_counter((index + 1) as u64);
            let object = if index + 1 == depth {
                super::super::heap::HeapObject::List(vec![Value::Number(0.0)])
            } else {
                super::super::heap::HeapObject::List(vec![Value::Ref(HeapId::from_counter(
                    (index + 2) as u64,
                ))])
            };
            bytes += object.logical_bytes();
            objects.push(CanonicalHeapEntry {
                id,
                object: if index + 1 == depth {
                    CanonicalHeapObject::List {
                        items: vec![CanonicalValue::Number { value: 0.0 }],
                    }
                } else {
                    CanonicalHeapObject::List {
                        items: vec![CanonicalValue::Ref {
                            value: HeapId::from_counter((index + 2) as u64),
                        }],
                    }
                },
            });
        }
        named_bytes(&canonical_heap_with(
            vec![CanonicalBinding {
                name: "root".to_string(),
                value: CanonicalValue::Ref {
                    value: HeapId::from_counter(1),
                },
            }],
            objects,
            depth as u64 + 1,
            depth as u64,
            bytes,
        ))
    }

    Snapshot::from_canonical_bytes(&chain_snapshot(MAX_SNAPSHOT_VALUE_DEPTH))
        .expect("a chain at the limit decodes");
    let error = Snapshot::from_canonical_bytes(&chain_snapshot(MAX_SNAPSHOT_VALUE_DEPTH + 1))
        .expect_err("a chain past the limit must be rejected");
    assert_eq!(
        error,
        SnapshotDecodeError::ValueDepthLimitExceeded {
            limit: MAX_SNAPSHOT_VALUE_DEPTH
        }
    );
}

#[test]
fn exotic_heap_snapshot_round_trip_preserves_order_aliases_and_durable_fields() {
    let mut heap = Heap::default();
    let shared = heap
        .allocate_list(vec![Value::String("shared".into())])
        .expect("shared object");
    let regexp = heap
        .allocate_regexp("a+".to_string(), "gim".to_string())
        .expect("RegExp");
    let Value::Ref(regexp_id) = regexp else {
        unreachable!()
    };
    heap.set_regexp_last_index(regexp_id, 7)
        .expect("set lastIndex");
    let regexp_slot = heap.id_to_slot[&regexp_id];
    let HeapObject::RegExp(regexp_object) = &mut heap.slots[regexp_slot]
        .as_mut()
        .expect("RegExp slot")
        .object
    else {
        unreachable!()
    };
    regexp_object.compiled_program = Some(Box::new(super::super::heap::RegExpProgramCache {
        program: regress::Regex::new("a+").expect("compiled test regexp"),
    }));
    let match_groups = heap
        .allocate_record(
            [("word".to_string(), Value::String("aaa".into()))]
                .into_iter()
                .collect(),
        )
        .expect("match groups");
    let regexp_match = heap
        .allocate_regexp_match(
            vec![Value::String("aaa".into())],
            Value::Number(4.0),
            Value::String("xxxxaaa".into()),
            match_groups,
        )
        .expect("RegExp match");

    let map = heap
        .allocate_map(vec![
            (Value::String("first".into()), shared.clone()),
            (Value::String("second".into()), Value::Number(f64::NAN)),
        ])
        .expect("Map");
    let set = heap
        .allocate_set(vec![shared.clone(), Value::Number(-0.0)])
        .expect("Set");
    let date = heap.allocate_date(f64::NAN).expect("Date");
    let error = heap
        .allocate_error(
            ErrorKind::TypeError,
            "bad".to_string(),
            Some(shared.clone()),
            None,
        )
        .expect("Error");
    let mut roots = Record::new();
    roots.insert("map".to_string(), map.clone());
    roots.insert("map_alias".to_string(), map);
    roots.insert("set".to_string(), set);
    roots.insert("regexp".to_string(), regexp);
    roots.insert("regexp_match".to_string(), regexp_match);
    roots.insert("date".to_string(), date);
    roots.insert("error".to_string(), error);
    let snapshot = Snapshot {
        globals: Record::new(),
        runtime_globals: roots,
        heap,
        reference_semantics: true,
    };

    let bytes = snapshot.to_canonical_bytes().expect("encode snapshot");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("decode snapshot");
    assert_eq!(
        restored.runtime_globals.get("map"),
        restored.runtime_globals.get("map_alias"),
        "two roots to one Map must still alias"
    );
    let Value::Ref(map_id) = restored.runtime_globals["map"] else {
        unreachable!()
    };
    let entries = restored
        .heap
        .map_entries(map_id)
        .expect("Map entries")
        .expect("Map kind");
    assert_eq!(entries[0].0, Value::String("first".into()));
    assert_eq!(entries[1].0, Value::String("second".into()));
    let Value::Ref(restored_regexp) = restored.runtime_globals["regexp"] else {
        unreachable!()
    };
    let regexp_slot = restored.heap.id_to_slot[&restored_regexp];
    let HeapObject::RegExp(regexp) = &restored.heap.slots[regexp_slot]
        .as_ref()
        .expect("restored RegExp slot")
        .object
    else {
        unreachable!()
    };
    assert_eq!(regexp.last_index, 7);
    assert!(
        regexp.compiled_program.is_none(),
        "compiled matcher cache must never be serialized"
    );
    let Value::Ref(restored_match) = restored.runtime_globals["regexp_match"] else {
        unreachable!()
    };
    let match_slot = restored.heap.id_to_slot[&restored_match];
    let HeapObject::RegExpMatch(regexp_match) = &restored.heap.slots[match_slot]
        .as_ref()
        .expect("restored RegExp match slot")
        .object
    else {
        unreachable!()
    };
    assert_eq!(regexp_match.items, vec![Value::String("aaa".into())]);
    assert_eq!(regexp_match.index, Value::Number(4.0));
    assert_eq!(regexp_match.input, Value::String("xxxxaaa".into()));
    assert_eq!(restored.to_canonical_bytes().expect("re-encode"), bytes);
}

#[test]
fn snapshot_decode_rejects_regexp_last_index_above_maximum_safe_length() {
    let id = HeapId::from_counter(1);
    let object = HeapObject::RegExp(RegExpObject {
        pattern: "a+".to_string(),
        flags: "g".to_string(),
        last_index: crate::runtime::heap::MAX_JAVASCRIPT_LENGTH + 1,
        compiled_program: None,
    });
    let mut wire = canonical_heap_with(
        vec![CanonicalBinding {
            name: "regexp".to_string(),
            value: CanonicalValue::Ref { value: id },
        }],
        vec![CanonicalHeapEntry {
            id,
            object: CanonicalHeapObject::RegExp {
                pattern: "a+".to_string(),
                flags: "g".to_string(),
                last_index: crate::runtime::heap::MAX_JAVASCRIPT_LENGTH + 1,
            },
        }],
        2,
        1,
        object.logical_bytes(),
    );
    wire.heap.as_mut().expect("heap").reference_semantics = true;
    let error = Snapshot::from_canonical_bytes(&named_bytes(&wire))
        .expect_err("out-of-range lastIndex must not decode");
    assert!(error.to_string().contains("maximum safe length"), "{error}");
}

#[test]
fn lashlang_forest_validation_rejects_every_typescript_exotic_kind() {
    for object in [
        HeapObject::RegExp(RegExpObject {
            pattern: String::new(),
            flags: String::new(),
            last_index: 0,
            compiled_program: None,
        }),
        HeapObject::RegExpMatch(crate::runtime::RegExpMatchObject {
            items: Vec::new(),
            index: Value::Number(0.0),
            input: Value::String(String::new().into()),
            groups: Value::Null,
        }),
        HeapObject::Map(MapObject {
            entries: Vec::new(),
        }),
        HeapObject::Set(SetObject { values: Vec::new() }),
        HeapObject::Date(DateObject { milliseconds: 0.0 }),
        HeapObject::Error(ErrorObject {
            kind: ErrorKind::Error,
            message: String::new(),
            cause: None,
            errors: None,
        }),
    ] {
        let mut heap = Heap::default();
        let root = heap.allocate(object).expect("exotic object");
        let mut roots = PersistedRoots::default();
        roots.durable("root", &root);
        assert!(heap.validate_persisted_graph(&roots).is_ok());
        let reason = heap
            .validate_persisted_forest(&roots)
            .expect_err("Lashlang forest must reject TypeScript exotic kinds");
        assert!(reason.contains("Lashlang forest"), "{reason}");
    }
}

/// A host with no abilities: the closure fixture below never performs one.
struct CrossProgramHost;

impl crate::runtime::ExecutionHost for CrossProgramHost {
    async fn perform(
        &self,
        _op: crate::runtime::AbilityOp,
    ) -> Result<crate::runtime::AbilityResult, crate::runtime::ExecutionHostError> {
        Err(crate::runtime::ExecutionHostError::new("no abilities"))
    }
}

/// A real captured closure, snapshotted and restored, must not fail validation
/// of a *different* program.
///
/// The sibling test above pins the intended rejection of malformed closure
/// metadata against the program that produced it. This is the other half:
/// closure metadata that is well-formed for program A carries no claim about
/// program B, and an RLM session compiles a fresh program per cell. See
/// FIG-1562.
///
/// Was red on `main`: `State::snapshot` collects the heap but keeps closures
/// its roots reach, so the restored state rejected the next program with
/// `UnknownFunction { index: 0 }`. The closure no longer survives the
/// execution that allocated it, so the snapshot carries none to reject with.
#[test]
fn a_restored_real_closure_does_not_reject_a_different_program() {
    let closure_program = compile_program_internal(&Program::block(vec![
        Expr::Assign {
            target: AssignTarget::variable("captured".into()),
            expr: Box::new(Expr::Number(1.0)),
        },
        Expr::Assign {
            target: AssignTarget::variable("f".into()),
            expr: Box::new(Expr::Function(Box::new(FunctionExpr {
                name: None,
                params: Vec::new(),
                captures: vec!["captured".into()],
                body: Box::new(Expr::Variable("captured".into())),
            }))),
        },
    ]));
    let mut state = State::new();
    futures::executor::block_on(crate::runtime::entry_points::execute(
        &closure_program,
        &mut state,
        &CrossProgramHost,
    ))
    .expect("the closure fixture executes");

    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("a real closure snapshot encodes");
    let mut restored = State::from_snapshot(
        Snapshot::from_canonical_bytes(&bytes).expect("a real closure snapshot decodes"),
    );

    // A different cell: no functions of its own.
    let next_program = compile_program_internal(&Program::block(vec![Expr::Number(42.0)]));
    restored
        .validate_program(&next_program)
        .expect("a restored closure must not reject the next cell's program");
}
