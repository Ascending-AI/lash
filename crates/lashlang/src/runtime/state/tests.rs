use super::*;
use crate::ast::{AssignTarget, Expr, FunctionExpr, Program};
use crate::runtime::HEAP_SIZE_SCHEDULE_VERSION;
use crate::runtime::ProjectedValue;
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
                js_name: None,
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
                // The `name`/`length` own-property slots ride the wire, so a
                // restored closure answers `f.name`/`f.length` as the live one
                // did — including after a `delete` cleared a slot to `None`.
                name: Some(Value::String("f".into())),
                length: Some(Value::Number(2.0)),
            })
            .expect("allocate malformed snapshot closure");
        let mut runtime_globals = Record::new();
        runtime_globals.insert("f".to_string(), closure);
        let snapshot = Snapshot {
            expired_functions: BTreeSet::new(),
            mode: StateMode::HeapBacked(Box::new(HeapBackedState {
                runtime_globals,
                projected: Record::new(),
                heap,
            })),
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
            name: None,
            length: None,
        })
        .expect("allocate unknown snapshot closure");
    let mut runtime_globals = Record::new();
    runtime_globals.insert("f".to_string(), closure);
    let bytes = Snapshot {
        expired_functions: BTreeSet::new(),
        mode: StateMode::HeapBacked(Box::new(HeapBackedState {
            runtime_globals,
            projected: Record::new(),
            heap,
        })),
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

/// The globals are a name table, so their order is normalized; a NaN's payload
/// is normalized too. A record's field order is its property order, which a
/// program can observe, so it is kept exactly — two records that differ only
/// in field order are different values on the wire and each decodes back in
/// its own order (FIG-3606).
#[test]
fn canonical_encoding_sorts_globals_normalizes_nan_and_keeps_property_order() {
    let left_nan = f64::from_bits(0x7ff0_0000_0000_0001);
    let right_nan = f64::from_bits(0xfff8_0000_0000_0042);

    let record = |fields: &[(&str, Value)]| {
        let mut record = Record::new();
        for (name, value) in fields {
            record.insert((*name).to_string(), value.clone());
        }
        Value::Record(Arc::new(record))
    };
    let text = Value::String("same\0\u{fffd}".into());
    let globals = |first: (&str, Value), second: (&str, Value)| {
        let mut globals = Record::new();
        globals.insert(first.0.to_string(), first.1);
        globals.insert(second.0.to_string(), second.1);
        globals
    };

    let z_first = record(&[("z", Value::Number(left_nan)), ("a", text.clone())]);
    let left = Snapshot::new(globals(("z-last", Value::Bool(true)), ("session", z_first)))
        .to_canonical_bytes()
        .expect("left encode");
    let z_first_again = record(&[("z", Value::Number(right_nan)), ("a", text.clone())]);
    let right = Snapshot::new(globals(
        ("session", z_first_again),
        ("z-last", Value::Bool(true)),
    ))
    .to_canonical_bytes()
    .expect("right encode");
    assert_eq!(left, right, "global order and NaN payload are normalized");

    let a_first = record(&[("a", text), ("z", Value::Number(left_nan))]);
    let reordered = Snapshot::new(globals(("session", a_first), ("z-last", Value::Bool(true))))
        .to_canonical_bytes()
        .expect("reordered encode");
    assert_ne!(left, reordered, "property order is part of the value");

    for (bytes, expected) in [(&left, ["z", "a"]), (&reordered, ["a", "z"])] {
        let decoded = Snapshot::from_canonical_bytes(bytes).expect("decode");
        let Some(Value::Record(session)) = decoded.globals().get("session") else {
            panic!("the session record decodes");
        };
        assert_eq!(session.keys().collect::<Vec<_>>(), expected);
    }
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
        expired_functions: Vec::new(),
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
            expired_functions: Vec::new(),
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
fn heapless_snapshot_encode_refuses_a_heap_reference() {
    let snapshot = Snapshot::new(
        [("dangling".to_string(), Value::Ref(HeapId::from_counter(7)))]
            .into_iter()
            .collect(),
    );

    assert_eq!(
        snapshot.to_canonical_bytes(),
        Err(ContinuationError::HeaplessSnapshotContainsReference {
            location: "globals.dangling".to_string(),
        })
    );
}

#[test]
fn heapless_snapshot_decode_refuses_a_heap_reference() {
    let wire = CanonicalSnapshot {
        expired_functions: Vec::new(),
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "dangling".to_string(),
            value: CanonicalValue::Ref {
                value: HeapId::from_counter(7),
            },
        }]),
        heap: None,
    };

    assert_eq!(
        Snapshot::from_canonical_bytes(&named_bytes(&wire)),
        Err(SnapshotDecodeError::HeaplessSnapshotContainsReference {
            location: "globals.dangling".to_string(),
        })
    );
}

#[test]
fn snapshot_try_from_refuses_a_heap_reference_without_the_raw_wire_validator() {
    let wire = CanonicalSnapshot {
        expired_functions: Vec::new(),
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "wrapper".to_string(),
            value: CanonicalValue::Tuple {
                items: vec![CanonicalValue::Ref {
                    value: HeapId::from_counter(11),
                }],
            },
        }]),
        heap: None,
    };

    assert_eq!(
        Snapshot::try_from(wire),
        Err(SnapshotDecodeError::HeaplessSnapshotContainsReference {
            location: "globals.wrapper[0]".to_string(),
        })
    );
}

#[test]
fn heapless_snapshot_fixed_point_cannot_launder_a_heap_reference() {
    let wire = CanonicalSnapshot {
        expired_functions: Vec::new(),
        version: LASHLANG_SNAPSHOT_VERSION,
        globals: Some(vec![CanonicalBinding {
            name: "wrapper".to_string(),
            value: CanonicalValue::Tuple {
                items: vec![CanonicalValue::Ref {
                    value: HeapId::from_counter(11),
                }],
            },
        }]),
        heap: None,
    };
    let bytes = named_bytes(&wire);
    let decoded_wire: CanonicalSnapshot =
        rmp_serde::from_slice(&bytes).expect("crafted canonical wire");
    assert_eq!(
        rmp_serde::to_vec_named(&decoded_wire).expect("re-encode crafted wire"),
        bytes,
        "the hostile bytes themselves are a serde fixed point"
    );

    assert_eq!(
        Snapshot::from_canonical_bytes(&bytes),
        Err(SnapshotDecodeError::HeaplessSnapshotContainsReference {
            location: "globals.wrapper.items[0]".to_string(),
        })
    );
}

#[test]
fn canonical_decode_rejects_a_depth_bomb_before_deserializing() {
    let mut value = CanonicalValue::Null {};
    for _ in 0..120 {
        value = CanonicalValue::List { items: vec![value] };
    }
    let bomb = CanonicalSnapshot {
        expired_functions: Vec::new(),
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
            0xaa, 0x1d, 0x8f, 0x15, 0xae, 0x61, 0x4e, 0x71, 0x20, 0x1a, 0x5e, 0x8f, 0xfe, 0x0f,
            0x85, 0xb4, 0xc7, 0x3c, 0xb4, 0x92, 0x43, 0x47, 0x73, 0x98, 0x17, 0x3b, 0x3b, 0xa4,
            0x5d, 0x12, 0x6f, 0xfe,
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
        expired_functions: Vec::new(),
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
            expired_functions: Vec::new(),
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
    assert_eq!(hex, "82a776657273696f6e0ba7676c6f62616c7390");
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
        expired_functions: Vec::new(),
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
        .allocate_error(ErrorKind::EffectError, Some("boom".to_string()), None, None)
        .expect("EffectError");
    let mut roots = Record::new();
    roots.insert("rejection".to_string(), error);
    let snapshot = Snapshot {
        expired_functions: BTreeSet::new(),
        mode: StateMode::HeapBacked(Box::new(HeapBackedState {
            runtime_globals: roots,
            projected: Record::new(),
            heap,
        })),
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
            Some("boom".to_string()),
            Some(cause),
            None,
        )
        .expect("EffectError");
    let mut roots = Record::new();
    roots.insert("rejection".to_string(), error);
    let snapshot = Snapshot {
        expired_functions: BTreeSet::new(),
        mode: StateMode::HeapBacked(Box::new(HeapBackedState {
            runtime_globals: roots,
            projected: Record::new(),
            heap,
        })),
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

/// FIG-3657: an error's own `message` presence is heap state, so the wire
/// carries `Option<String>` and an absent message stays absent while an
/// explicitly empty one stays empty. `Object.hasOwn(e, "message")` reads the
/// same slot, so these assertions are the round-trip's ownness contract.
#[test]
fn error_message_presence_round_trips_through_the_snapshot_wire() {
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
    let mut roots = Record::new();
    roots.insert("absent".to_string(), absent);
    roots.insert("empty".to_string(), empty);
    roots.insert("message".to_string(), message);
    roots.insert("caused".to_string(), caused);
    let snapshot = Snapshot {
        expired_functions: BTreeSet::new(),
        mode: StateMode::HeapBacked(Box::new(HeapBackedState {
            runtime_globals: roots,
            projected: Record::new(),
            heap,
        })),
    };

    let bytes = snapshot.to_canonical_bytes().expect("encode snapshot");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("decode snapshot");
    assert_eq!(
        restored.to_canonical_bytes().expect("re-encode"),
        bytes,
        "the snapshot re-encodes byte for byte"
    );

    let StateMode::HeapBacked(backed) = &restored.mode else {
        panic!("a rooted error heap restores heap-backed");
    };
    let restored_error = |name: &str| -> &ErrorObject {
        let Some(Value::Ref(id)) = backed.runtime_globals.get(name) else {
            panic!("{name} should restore as a heap reference");
        };
        let HeapObject::Error(error) = backed.heap.get(*id).expect("restored object") else {
            panic!("{name} should restore as an error");
        };
        error
    };
    assert_eq!(restored_error("absent").message, None);
    assert_eq!(restored_error("empty").message.as_deref(), Some(""));
    assert_eq!(restored_error("message").message.as_deref(), Some("m"));
    let caused = restored_error("caused");
    assert_eq!(caused.message.as_deref(), Some("m"));
    assert_eq!(caused.cause, Some(Value::String("why".into())));
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
    let HeapObject::RegExp(regexp_object) = &mut heap
        .entries
        .get_mut(&regexp_id)
        .expect("RegExp entry")
        .object
    else {
        unreachable!()
    };
    regexp_object.compiled_program = Some(Box::new(super::super::heap::RegExpProgramCache {
        program: lash_regress::Regex::new("a+").expect("compiled test regexp"),
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
            Some("bad".to_string()),
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
        expired_functions: BTreeSet::new(),
        mode: StateMode::HeapBacked(Box::new(HeapBackedState {
            runtime_globals: roots,
            projected: Record::new(),
            heap,
        })),
    };

    let bytes = snapshot.to_canonical_bytes().expect("encode snapshot");
    let restored = Snapshot::from_canonical_bytes(&bytes).expect("decode snapshot");
    let StateMode::HeapBacked(backed) = &restored.mode else {
        panic!("a rooted snapshot restores heap-backed")
    };
    let runtime_globals = &backed.runtime_globals;
    let heap = &backed.heap;
    assert_eq!(
        runtime_globals.get("map"),
        runtime_globals.get("map_alias"),
        "two roots to one Map must still alias"
    );
    let Value::Ref(map_id) = runtime_globals["map"] else {
        unreachable!()
    };
    let entries = heap
        .map_entries(map_id)
        .expect("Map entries")
        .expect("Map kind");
    assert_eq!(entries[0].0, Value::String("first".into()));
    assert_eq!(entries[1].0, Value::String("second".into()));
    let Value::Ref(restored_regexp) = runtime_globals["regexp"] else {
        unreachable!()
    };
    let HeapObject::RegExp(regexp) = &heap
        .entries
        .get(&restored_regexp)
        .expect("restored RegExp entry")
        .object
    else {
        unreachable!()
    };
    assert_eq!(regexp.last_index, 7);
    assert!(
        regexp.compiled_program.is_none(),
        "compiled matcher cache must never be serialized"
    );
    let Value::Ref(restored_match) = runtime_globals["regexp_match"] else {
        unreachable!()
    };
    let HeapObject::RegExpMatch(regexp_match) = &heap
        .entries
        .get(&restored_match)
        .expect("restored RegExp match entry")
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
            message: None,
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
                js_name: None,
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

/// The pending-tool handle record a VM mints for an in-flight call.
///
/// `access::value_contains_tool_handle` recognizes it by the two fields
/// `lash_sansio::handle` owns, so this is the shape the host view refuses to
/// carry across an execution boundary.
fn pending_tool_handle_record() -> Record {
    let mut record = Record::new();
    record.insert(
        lash_sansio::handle::HANDLE_FIELD.to_string(),
        Value::String(lash_sansio::handle::HANDLE_KIND.into()),
    );
    record.insert(
        "id".to_string(),
        Value::String(
            lash_sansio::handle::HandleId::tool(0x9e37, 1)
                .as_str()
                .into(),
        ),
    );
    record
}

fn pending_tool_handle_object() -> HeapObject {
    HeapObject::Record(Box::new(pending_tool_handle_record()))
}

/// Every heap object kind that stays bound in the runtime roots while the host
/// view omits it: the JavaScript exotics that have no detached host shape, and
/// a pending-tool handle, which has one and must not travel in it.
///
/// Closures are deliberately absent: `install_runtime` drops a closure-rooted
/// name from the runtime roots too, so the two records still agree about it.
fn names_the_host_view_omits() -> Vec<(&'static str, HeapObject)> {
    vec![
        (
            "Map",
            HeapObject::Map(MapObject {
                entries: Vec::new(),
            }),
        ),
        ("Set", HeapObject::Set(SetObject { values: Vec::new() })),
        ("Date", HeapObject::Date(DateObject { milliseconds: 1.0 })),
        ("pending tool handle", pending_tool_handle_object()),
    ]
}

fn state_rooting(name: &str, object: HeapObject) -> (State, Value) {
    let mut heap = Heap::default();
    let value = heap.allocate(object).expect("allocate the rooted object");
    let mut runtime_globals = Record::new();
    runtime_globals.insert(name.to_string(), value.clone());
    let mut state = State::new();
    state
        .install_runtime(runtime_globals, heap)
        .expect("install a runtime binding the host view omits");
    (state, value)
}

/// The roots only a heap-backed mode carries — tests reach them through the
/// mode rather than a field.
fn heap_backed_roots(state: &State) -> &Record {
    let StateMode::HeapBacked(backed) = &state.mode else {
        panic!("expected a heap-backed state")
    };
    &backed.runtime_globals
}

/// The precondition every test below rests on: the host view is a lossy
/// projection, so a live binding can be absent from it.
fn assert_binding_is_owned_but_unprojected(state: &State, label: &str, name: &str, value: &Value) {
    assert_eq!(
        heap_backed_roots(state).get(name),
        Some(value),
        "{label}: the runtime roots must own the binding"
    );
    assert!(
        state.globals().get(name).is_none(),
        "{label}: the host view must omit the binding"
    );
}

#[test]
fn a_default_leaves_a_binding_the_host_view_omits_alone() {
    for (label, object) in names_the_host_view_omits() {
        let (mut state, value) = state_rooting("kept", object);
        assert_binding_is_owned_but_unprojected(&state, label, "kept", &value);

        let bound = state
            .set_default("kept", Value::Number(1.0))
            .expect("a default over a rooted binding stays within the heap bound");

        assert!(
            !bound,
            "{label}: a default must not bind a name the runtime roots already hold"
        );
        assert_eq!(
            heap_backed_roots(&state).get("kept"),
            Some(&value),
            "{label}: the live binding must survive the default untouched"
        );
    }
}

#[test]
fn removing_a_binding_the_host_view_omits_reports_it_removed() {
    for (label, object) in names_the_host_view_omits() {
        let (mut state, value) = state_rooting("kept", object);
        assert_binding_is_owned_but_unprojected(&state, label, "kept", &value);

        assert!(
            state.remove_global("kept"),
            "{label}: removing a live binding must report it removed"
        );
        assert!(
            heap_backed_roots(&state).get("kept").is_none(),
            "{label}: the runtime roots must no longer hold the binding"
        );
    }
}

#[test]
fn rebinding_a_name_the_host_view_omits_reports_the_previous_binding() {
    for (label, object) in names_the_host_view_omits() {
        let (mut state, value) = state_rooting("kept", object);
        assert_binding_is_owned_but_unprojected(&state, label, "kept", &value);

        let replaced = state
            .insert_global("kept", Value::Number(1.0))
            .expect("rebinding a rooted name stays within the heap bound");

        assert!(
            replaced,
            "{label}: rebinding must report that a binding was already there"
        );
        assert_eq!(
            state.globals().get("kept"),
            Some(&Value::Number(1.0)),
            "{label}: the new binding is host-visible"
        );
    }
}

#[test]
fn a_state_whose_host_view_omits_a_binding_round_trips_through_the_wire() {
    for (label, object) in names_the_host_view_omits() {
        let (state, value) = state_rooting("kept", object);
        assert_binding_is_owned_but_unprojected(&state, label, "kept", &value);

        let snapshot = state.snapshot();
        let bytes = snapshot
            .to_canonical_bytes()
            .expect("a rooted binding the host view omits encodes");
        let decoded = Snapshot::from_canonical_bytes(&bytes)
            .expect("a rooted binding the host view omits decodes");

        assert_eq!(
            decoded.globals(),
            snapshot.globals(),
            "{label}: the decoder must derive the host view by the live rule"
        );
        assert_eq!(decoded, snapshot, "{label}: decode(encode(s)) must equal s");
    }
}

/// A host may write a name whose value the view cannot carry. The view stays a
/// projection of the roots rather than growing an entry of its own, so the
/// state a snapshot restores is still the state that was captured.
#[test]
fn a_host_write_the_view_cannot_carry_leaves_the_view_a_projection() {
    let (mut state, _) = state_rooting("anchor", HeapObject::List(Vec::new()));
    let replaced = state
        .insert_global(
            "pending",
            Value::Record(Arc::new(pending_tool_handle_record())),
        )
        .expect("writing a handle-bearing binding stays within the heap bound");

    assert!(!replaced, "the name was unbound before the write");
    assert!(
        heap_backed_roots(&state).get("pending").is_some(),
        "the runtime roots own the binding the host wrote"
    );
    assert!(
        state.globals().get("pending").is_none(),
        "the host view omits it by the one projection rule"
    );

    let snapshot = state.snapshot();
    let bytes = snapshot
        .to_canonical_bytes()
        .expect("a host-written handle-bearing binding encodes");
    let decoded = Snapshot::from_canonical_bytes(&bytes)
        .expect("a host-written handle-bearing binding decodes");
    assert_eq!(decoded, snapshot, "decode(encode(s)) must equal s");
}

/// The mode is a value, so it must survive the wire as a value: a plain
/// snapshot decodes plain and a heap-backed snapshot decodes heap-backed.
#[test]
fn the_state_mode_survives_a_snapshot_round_trip_in_both_directions() {
    let plain = State::from_snapshot(Snapshot::new(
        [("x".to_string(), Value::Number(1.0))]
            .into_iter()
            .collect(),
    ));
    assert!(matches!(plain.mode, StateMode::Plain(_)));
    let plain_snapshot = plain.snapshot();
    let plain_bytes = plain_snapshot
        .to_canonical_bytes()
        .expect("a plain snapshot encodes");
    let decoded = Snapshot::from_canonical_bytes(&plain_bytes).expect("a plain snapshot decodes");
    assert!(
        matches!(decoded.mode, StateMode::Plain(_)),
        "a globals wire must not come back heap-backed"
    );
    assert_eq!(decoded, plain_snapshot);
    assert_eq!(State::from_snapshot(decoded), plain);

    let (heap_backed, _) = state_rooting(
        "kept",
        HeapObject::Map(MapObject {
            entries: Vec::new(),
        }),
    );
    assert!(matches!(heap_backed.mode, StateMode::HeapBacked(_)));
    let heap_snapshot = heap_backed.snapshot();
    let heap_bytes = heap_snapshot
        .to_canonical_bytes()
        .expect("a heap-backed snapshot encodes");
    let decoded =
        Snapshot::from_canonical_bytes(&heap_bytes).expect("a heap-backed snapshot decodes");
    assert!(
        matches!(decoded.mode, StateMode::HeapBacked(_)),
        "a heap wire must not come back plain"
    );
    assert_eq!(decoded, heap_snapshot);
    assert_eq!(State::from_snapshot(decoded), heap_backed);
}

/// A host write makes the heap the owner at once, exactly as a cell's binding
/// does, so the first execution does not re-home seeded values under new ids.
/// A removal alone leaves a plain state plain.
#[test]
fn a_host_write_promotes_a_plain_state_to_the_heap() {
    let mut state = State::new();
    assert!(!state.remove_global("absent"));
    assert!(matches!(state.mode, StateMode::Plain(_)));
    state
        .insert_global(
            "seeded",
            Value::List(vec![Value::String("kept".into())].into()),
        )
        .expect("seed a compound");
    let StateMode::HeapBacked(backed) = &state.mode else {
        panic!("a host write promotes the state")
    };
    assert!(matches!(backed.runtime_globals["seeded"], Value::Ref(_)));
    assert_eq!(
        state.globals()["seeded"],
        Value::List(vec![Value::String("kept".into())].into())
    );
}

/// Taking a heap-backed state's runtime leaves a plain state holding the
/// host view: the projection outlives the roots it was projected from, and a
/// second take hands out that record with a fresh heap.
#[test]
fn taking_the_runtime_leaves_the_host_view_as_a_plain_state() {
    let (mut state, _) = state_rooting(
        "kept",
        HeapObject::Map(MapObject {
            entries: Vec::new(),
        }),
    );
    state
        .insert_global("visible", Value::Number(7.0))
        .expect("a host-visible write stays within the heap bound");

    let (roots, heap) = state.take_runtime();
    assert!(heap.has_runtime_state(), "the taken heap is the live one");
    assert!(roots.get("kept").is_some());
    assert!(roots.get("visible").is_some());

    let StateMode::Plain(view) = &state.mode else {
        panic!("the state left behind must be plain")
    };
    assert_eq!(view.get("visible"), Some(&Value::Number(7.0)));
    assert!(
        view.get("kept").is_none(),
        "the view still omits what the host cannot see"
    );

    let (globals, heap) = state.take_runtime();
    assert_eq!(globals.get("visible"), Some(&Value::Number(7.0)));
    assert!(
        !heap.has_runtime_state(),
        "a second take hands out a fresh heap, not the live one"
    );
}

/// Rebinding a reload's projection placeholders writes inside the objects that
/// hold them: two bindings that shared an object before still share it, and
/// both see the live projection (FIG-3628).
#[test]
fn rebinding_projections_keeps_every_binding_on_the_same_object() {
    let placeholder = ProjectedValue::unavailable_after_restore_with_projection_ref(
        "report",
        "object",
        Some(serde_json::json!({"kind": "memory", "key": "k"})),
    );
    let mut heap = Heap::default();
    let mut holder = Record::new();
    holder.insert("doc".to_string(), Value::Projected(placeholder));
    holder.insert("n".to_string(), Value::Number(1.0));
    let holder = heap.allocate_record(holder).expect("holder");
    let mut roots = Record::new();
    roots.insert("alias".to_string(), holder.clone());
    roots.insert("holder".to_string(), holder.clone());
    let mut state = State::new();
    state
        .install_runtime(roots, heap)
        .expect("install the shared holder");

    let found = state.unavailable_projections();
    assert_eq!(
        found
            .iter()
            .map(|(name, projected)| (name.as_str(), projected.name()))
            .collect::<Vec<_>>(),
        [("alias", "report"), ("holder", "report")],
        "both bindings depend on the one placeholder"
    );

    let live = ProjectedValue::scalar("report", Value::String("live".into()));
    state
        .rebind_projections(|placeholder| (placeholder.name() == "report").then(|| live.clone()))
        .expect("rebind in place");
    let roots = heap_backed_roots(&state);
    assert_eq!(roots["alias"], holder, "`alias` still names the holder");
    assert_eq!(roots["holder"], holder, "`holder` still names the holder");
    assert!(state.unavailable_projections().is_empty());
    let Some(Value::Record(view)) = state.globals().get("holder") else {
        panic!("the holder is in the host view")
    };
    assert_eq!(
        view["doc"],
        Value::Projected(live),
        "the view is re-derived from the rebound object"
    );
}
