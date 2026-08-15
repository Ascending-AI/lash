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
    assert!(matches!(values[1], Value::Number(value) if value == 0.0));
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
}
