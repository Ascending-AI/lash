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
