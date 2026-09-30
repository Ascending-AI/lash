use super::*;

#[test]
fn definition_id_survives_every_container_across_cells() {
    let ids = (1..=7)
        .map(|byte| lash_sansio::ProcessDefinitionId::from_sha256_digest([byte; 32]))
        .collect::<Vec<_>>();
    let mut heap = Heap::with_limit(u64::MAX);
    let tags = heap
        .import_values(
            ids.iter()
                .map(|id| crate::from_json(id.to_tagged_json()))
                .collect(),
            ids.len(),
        )
        .expect("definition records enter the heap");
    let tagged = |index: usize| tags[index].clone();
    let map_key = heap
        .allocate(HeapObject::Map(MapObject {
            entries: vec![(tagged(1), Value::Null)],
        }))
        .expect("map key");
    let map_value = heap
        .allocate(HeapObject::Map(MapObject {
            entries: vec![(Value::String("key".into()), tagged(2))],
        }))
        .expect("map value");
    let set = heap
        .allocate(HeapObject::Set(SetObject {
            values: vec![tagged(3)],
        }))
        .expect("set");
    let error = heap
        .allocate(HeapObject::Error(ErrorObject {
            kind: ErrorKind::Error,
            message: Some("cause".into()),
            cause: Some(tagged(4)),
            errors: None,
        }))
        .expect("error");
    let errors = heap
        .allocate(HeapObject::List(vec![tagged(5)]))
        .expect("aggregate errors list");
    let aggregate = heap
        .allocate(HeapObject::Error(ErrorObject {
            kind: ErrorKind::AggregateError,
            message: None,
            cause: None,
            errors: Some(errors),
        }))
        .expect("aggregate error");
    let mut nested = Record::new();
    nested.insert("id".into(), tagged(6));
    let nested = heap
        .import_values(
            vec![Value::Tuple(vec![Value::Record(Arc::new(nested))].into())],
            1,
        )
        .expect("nested heap records")
        .remove(0);
    let mut roots = Record::new();
    for (name, value) in [
        ("plain", tagged(0)),
        ("map_key", map_key),
        ("map_value", map_value),
        ("set", set),
        ("error", error),
        ("aggregate", aggregate),
        ("nested", nested),
        (
            "bare_digest",
            Value::String(
                lash_sansio::ProcessDefinitionId::from_sha256_digest([99; 32])
                    .to_string()
                    .into(),
            ),
        ),
    ] {
        roots.insert(name.into(), value);
    }
    let mut state = State::new();
    state
        .install_runtime(roots, heap)
        .expect("install heap roots");
    let expected = ids.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(state.referenced_definition_ids(), expected);
    assert!(state.globals().get("map_key").is_none());
    for _cell in 0..2 {
        let bytes = state.snapshot().to_canonical_bytes().expect("capture cell");
        let decoded = crate::VmInstance::pristine()
            .open_snapshot(&bytes)
            .expect("worker restores cell");
        state = State::from_snapshot(decoded);
        assert_eq!(state.referenced_definition_ids(), expected);
    }
    let malformed = crate::from_json(
        serde_json::json!({"$lash_definition_id": "lash.definition:sha256:ABC", "extra": true}),
    );
    assert!(crate::referenced_definition_ids(&malformed).is_empty());
}
