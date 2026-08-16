use super::*;

#[test]
fn swept_storage_gets_a_fresh_monotonic_identity() {
    let mut heap = Heap::default();
    let Value::Ref(first) = heap
        .allocate(HeapObject::List(Vec::new()))
        .expect("first allocation")
    else {
        unreachable!()
    };
    heap.collect(std::iter::empty());
    let Value::Ref(second) = heap
        .allocate(HeapObject::List(Vec::new()))
        .expect("second allocation")
    else {
        unreachable!()
    };
    assert!(second > first);
    assert!(matches!(
        heap.get(first),
        Err(RuntimeError::DanglingHeapReference { .. })
    ));
}

#[test]
fn isolation_preserves_cycles_with_fresh_ids() {
    let mut heap = Heap::default();
    let Value::Ref(original) = heap
        .allocate(HeapObject::List(Vec::new()))
        .expect("original")
    else {
        unreachable!()
    };
    heap.replace_object(original, HeapObject::List(vec![Value::Ref(original)]))
        .expect("cycle");
    let Value::Ref(copy) = heap
        .isolate_value(&Value::Ref(original))
        .expect("deep copy")
    else {
        unreachable!()
    };
    assert_ne!(copy, original);
    assert_eq!(
        heap.get(copy),
        Ok(&HeapObject::List(vec![Value::Ref(copy)]))
    );
    assert!(
        heap.structural_eq(&Value::Ref(original), &Value::Ref(copy))
            .expect("compare cycles")
    );
}

#[test]
fn sparse_object_bookkeeping_stays_bounded_by_live_objects() {
    let mut heap = Heap::default();
    for _ in 0..5_000 {
        heap.allocate(HeapObject::List(Vec::new()))
            .expect("allocate transient object");
        heap.collect(std::iter::empty());
    }

    assert_eq!(heap.slots.len(), 1, "vacant storage slot should be reused");
    assert!(heap.id_to_slot.is_empty(), "no dead ID bookkeeping remains");
    assert!(heap.materialized.is_empty());
    assert!(heap.boundary_refs.is_empty());
}

#[test]
fn indexed_add_charges_before_record_growth_and_updates_incrementally() {
    let key = Value::String("a-long-new-key".into());
    let added_bytes = RECORD_FIELD_BYTES
        + "a-long-new-key".len() as u64
        + value_logical_bytes(&Value::Number(1.0));
    let base_bytes = HeapObject::Record(Box::default()).logical_bytes();

    let mut exact = Heap::with_limit(base_bytes + added_bytes);
    let target = exact
        .allocate(HeapObject::Record(Box::default()))
        .expect("allocate exact-limit record");
    exact
        .add_assign_index_number(&target, &key, 1.0)
        .expect("exact limit must succeed");
    assert_eq!(exact.live_logical_bytes(), base_bytes + added_bytes);

    let mut over = Heap::with_limit(base_bytes + added_bytes - 1);
    let target = over
        .allocate(HeapObject::Record(Box::default()))
        .expect("allocate one-byte-over record");
    assert!(matches!(
        over.add_assign_index_number(&target, &key, 1.0),
        Err(RuntimeError::MemoryLimitExceeded { .. })
    ));
    assert_eq!(over.live_logical_bytes(), base_bytes);
    assert_eq!(
        over.export(&target)
            .expect("post-error record remains valid"),
        Value::Record(std::sync::Arc::new(Record::new()))
    );
}

#[test]
fn child_mutation_invalidates_materialized_ancestor_cache() {
    let mut heap = Heap::default();
    let child = heap
        .allocate(HeapObject::List(vec![Value::Number(1.0)]))
        .expect("allocate child");
    let parent = heap
        .allocate(HeapObject::List(vec![child.clone()]))
        .expect("allocate parent");

    assert_eq!(
        heap.export(&parent).expect("materialize parent"),
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
    heap.push_list(&child, Value::Number(2.0))
        .expect("mutate child");
    assert_eq!(
        heap.export(&parent).expect("rematerialize parent"),
        Value::List(
            vec![Value::List(
                vec![Value::Number(1.0), Value::Number(2.0)].into()
            )]
            .into()
        )
    );
}

#[test]
fn map_and_set_use_same_value_zero_without_reordering_updates() {
    let mut heap = Heap::default();
    let Value::Ref(map) = heap.allocate_map(Vec::new()).expect("Map") else {
        unreachable!()
    };
    heap.map_set(map, Value::Number(f64::NAN), Value::String("first".into()))
        .expect("insert NaN");
    heap.map_set(map, Value::Number(-0.0), Value::String("zero".into()))
        .expect("insert negative zero");
    heap.map_set(
        map,
        Value::Number(f64::NAN),
        Value::String("updated".into()),
    )
    .expect("update NaN");
    heap.map_set(map, Value::Number(0.0), Value::String("same zero".into()))
        .expect("update zero");
    let entries = heap.map_entries(map).expect("read Map").expect("Map kind");
    assert_eq!(entries.len(), 2);
    assert!(matches!(entries[0].0, Value::Number(value) if value.is_nan()));
    assert_eq!(entries[0].1, Value::String("updated".into()));
    assert!(matches!(entries[1].0, Value::Number(value) if value.to_bits() == 0.0_f64.to_bits()));
    assert_eq!(entries[1].1, Value::String("same zero".into()));

    let Value::Ref(set) = heap.allocate_set(Vec::new()).expect("Set") else {
        unreachable!()
    };
    for value in [f64::NAN, f64::NAN, -0.0, 0.0] {
        heap.set_add(set, Value::Number(value))
            .expect("add Set value");
    }
    let values = heap.set_values(set).expect("read Set").expect("Set kind");
    assert_eq!(values.len(), 2);
    assert!(matches!(values[0], Value::Number(value) if value.is_nan()));
    assert!(matches!(values[1], Value::Number(value) if value.to_bits() == 0.0_f64.to_bits()));
}

#[test]
fn exotic_member_apis_import_inline_compounds_before_storage() {
    let mut heap = Heap::default();
    let inline_list = || Value::List(vec![Value::Number(1.0)].into());
    let inline_record = || {
        Value::Record(std::sync::Arc::new({
            let mut record = Record::new();
            record.insert("nested".to_string(), Value::Bool(true));
            record
        }))
    };

    let Value::Ref(map) = heap
        .allocate_map(vec![(inline_list(), inline_record())])
        .expect("Map imports constructor members")
    else {
        unreachable!()
    };
    let entries = heap.map_entries(map).expect("read Map").expect("Map");
    assert!(matches!(entries[0], (Value::Ref(_), Value::Ref(_))));

    heap.map_set(map, inline_record(), inline_list())
        .expect("Map.set imports members");
    let entries = heap.map_entries(map).expect("read Map").expect("Map");
    assert!(
        entries
            .iter()
            .all(|(key, value)| matches!(key, Value::Ref(_)) && matches!(value, Value::Ref(_)))
    );

    let Value::Ref(set) = heap
        .allocate_set(vec![inline_list()])
        .expect("Set imports constructor values")
    else {
        unreachable!()
    };
    heap.set_add(set, inline_record())
        .expect("Set.add imports values");
    assert!(
        heap.set_values(set)
            .expect("read Set")
            .expect("Set")
            .iter()
            .all(|value| matches!(value, Value::Ref(_)))
    );
}

#[test]
fn map_object_keys_compare_by_heap_identity() {
    let mut heap = Heap::default();
    let first = heap.allocate_list(Vec::new()).expect("first key");
    let second = heap.allocate_list(Vec::new()).expect("second key");
    let Value::Ref(map) = heap.allocate_map(Vec::new()).expect("Map") else {
        unreachable!()
    };
    heap.map_set(map, first.clone(), Value::Number(1.0))
        .expect("first object key");
    heap.map_set(map, second.clone(), Value::Number(2.0))
        .expect("second object key");
    assert_eq!(
        heap.map_get(map, &first).expect("lookup"),
        Some(Value::Number(1.0))
    );
    assert_eq!(
        heap.map_get(map, &second).expect("lookup"),
        Some(Value::Number(2.0))
    );
    assert_eq!(
        heap.map_entries(map).expect("entries").expect("Map").len(),
        2
    );
}

#[test]
fn exotic_kinds_have_deterministic_logical_byte_charges() {
    let regexp = HeapObject::RegExp(RegExpObject {
        pattern: "ab".to_string(),
        flags: "gi".to_string(),
        last_index: 0,
        compiled_program: None,
    });
    let map = HeapObject::Map(MapObject {
        entries: vec![(Value::String("k".into()), Value::Number(1.0))],
    });
    let set = HeapObject::Set(SetObject {
        values: vec![Value::String("v".into())],
    });
    let date = HeapObject::Date(DateObject { milliseconds: 1.0 });
    let error = HeapObject::Error(ErrorObject {
        kind: ErrorKind::TypeError,
        message: "bad".to_string(),
        cause: Some(Value::Number(1.0)),
        errors: None,
    });
    assert_eq!(
        regexp.logical_bytes(),
        OBJECT_HEADER_BYTES + 2 + 2 + 3 * VALUE_SLOT_BYTES + 8
    );
    assert_eq!(
        map.logical_bytes(),
        OBJECT_HEADER_BYTES
            + COLLECTION_ENTRY_BYTES
            + value_logical_bytes(&Value::String("k".into()))
            + value_logical_bytes(&Value::Number(1.0))
    );
    assert_eq!(
        set.logical_bytes(),
        OBJECT_HEADER_BYTES
            + COLLECTION_ENTRY_BYTES
            + value_logical_bytes(&Value::String("v".into()))
    );
    assert_eq!(
        date.logical_bytes(),
        OBJECT_HEADER_BYTES + VALUE_SLOT_BYTES + 8
    );
    assert_eq!(
        error.logical_bytes(),
        OBJECT_HEADER_BYTES + 3 + VALUE_SLOT_BYTES + value_logical_bytes(&Value::Number(1.0))
    );
}

#[test]
fn exotic_host_boundary_errors_are_not_reported_as_function_values() {
    let mut heap = Heap::default();
    let values = [
        heap.allocate_regexp("a".to_string(), "g".to_string())
            .expect("RegExp"),
        heap.allocate_map(Vec::new()).expect("Map"),
        heap.allocate_set(Vec::new()).expect("Set"),
        heap.allocate_date(0.0).expect("Date"),
        heap.allocate_error(ErrorKind::Error, String::new(), None, None)
            .expect("Error"),
    ];
    for value in values {
        assert!(matches!(
            heap.export_for_instruction(&value),
            Err(RuntimeError::JavaScriptExoticAtHostBoundary { .. })
        ));
    }
}

/// The budget's per-slot charge is the real slot size, not a guess that
/// happened to be four times too small.
#[test]
fn value_slot_bytes_covers_the_real_value_slot() {
    assert!(
        std::mem::size_of::<Value>() as u64 <= VALUE_SLOT_BYTES,
        "VALUE_SLOT_BYTES ({VALUE_SLOT_BYTES}) must charge at least what a Value slot really \
         costs ({}); a variant grew the enum, so raise the charge with it",
        std::mem::size_of::<Value>(),
    );
}

/// The array pre-charge and the charge the committed list actually carries are
/// the same arithmetic. They are written twice — once ahead of building the
/// `Vec`, once from the built object — and a drift between them is either a
/// pre-charge that lets an over-budget array through or one that refuses an
/// array the heap would have accepted.
#[test]
fn the_list_pre_charge_matches_what_the_committed_list_is_charged() {
    let mut heap = Heap::default();
    for len in [0_usize, 1, 7, 64] {
        let committed = HeapObject::List(vec![Value::Undefined; len]).logical_bytes();
        let limit = heap.live_logical_bytes().saturating_add(committed);
        heap.set_limit(limit);
        heap.ensure_list_allocation_len(len)
            .unwrap_or_else(|error| panic!("len {len} must pre-charge within {limit}: {error}"));
        heap.set_limit(limit.saturating_sub(1));
        assert!(
            heap.ensure_list_allocation_len(len).is_err(),
            "len {len} must not pre-charge under the exact committed charge"
        );
    }
}
