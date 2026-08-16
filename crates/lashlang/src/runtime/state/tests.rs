use super::*;
use crate::runtime::HEAP_SIZE_SCHEDULE_VERSION;

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
            0x89, 0xb2, 0x49, 0xb1, 0x7e, 0x9b, 0xf7, 0xa6, 0xba, 0x2b, 0xbe, 0xe5, 0xff, 0x32,
            0xdc, 0x1a, 0xf1, 0xaa, 0x92, 0xeb, 0x7a, 0x51, 0xc6, 0x6d, 0x0d, 0x8b, 0xbc, 0xff,
            0xd3, 0xba, 0x19, 0xb9,
        ]
    );
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
    assert_eq!(hex, "82a776657273696f6e02a7676c6f62616c7390");
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
