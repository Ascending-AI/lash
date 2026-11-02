/// Source allocation order survives reversed binding order, interior retirement,
/// immutable nesting and canonical duplicate-key normalization. This executes the
/// detached boundary API directly; common source acceptance is core-owned.
pub(super) fn prepared_graphs(machine: &crate::machine::KernelMachine) {
    use super as boundary;
    use lash_kernel_doc::{NewObject, Nursery, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, Owner, Slot},
    };
    let heap = machine.source_law_heap();
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let owner = |slot| Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(slot),
    };
    for attempt in 0..4 {
        let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
        let before = (arena.allocations(), arena.next_provisional());
        let mut nursery = Nursery::unaccounted(arena.objects());
        let first = nursery.allocate(NewObject::List(vec![Value::Int(7)]));
        let retired = nursery.allocate(NewObject::List(vec![]));
        let outer = nursery.allocate(NewObject::List(vec![first]));
        let tuple = nursery.allocate(NewObject::Tuple(vec![outer, Value::Int(17)]));
        let key = nursery.text("same");
        let map = nursery.allocate(NewObject::Map(vec![
            (key, Value::Int(1)),
            (key, Value::Int(2)),
        ]));
        let set = nursery.allocate(NewObject::Set(vec![
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
        ]));
        let record = nursery.allocate(NewObject::Record(vec![
            (key, Value::Int(1)),
            (key, Value::Int(3)),
        ]));
        // The last allocated mutable body is the first binding live-out.
        let outputs = [
            (owner(0), record),
            (owner(1), tuple),
            (owner(2), map),
            (owner(3), set),
        ];
        let new = nursery.into_new();
        let graph = boundary::prepare_collections(&mut arena, heap, new, &outputs, 128)
            .unwrap()
            .unwrap();
        assert_eq!(
            graph
                .entities
                .iter()
                .map(|entity| entity.entity.ordinal)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
        if attempt == 0 {
            drop(graph);
        } else {
            let (transaction, storage) = graph
                .into_boundary(&ledger, heap, &[], dependencies)
                .unwrap();
            assert_eq!(
                storage.members.len(),
                6,
                "one canonical body summary for every source allocation"
            );
            if attempt == 1 {
                assert!(transaction.admit(0).is_err());
            } else {
                let accepted = transaction
                    .admit(u64::MAX)
                    .unwrap_or_else(|_| panic!("unbounded construction fits"));
                if attempt == 2 {
                    ledger.handoff().unwrap();
                }
                let result = accepted.publish(&mut ledger, |arena, edits| {
                    assert!(edits.is_empty());
                    storage.batch.publish(arena);
                });
                if attempt == 2 {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
            }
        }
        if attempt < 3 {
            assert_eq!((arena.allocations(), arena.next_provisional()), before);
            for value in [first, retired, outer, tuple, map, set, record] {
                assert!(
                    arena.body(value.handle().unwrap()).is_err(),
                    "unaccepted storage is invisible"
                );
            }
            assert_eq!(ledger.balance().unwrap().total().unwrap(), 0);
        } else {
            assert_eq!(arena.next_provisional().ordinal, 6);
            for (value, ordinal) in [
                (first, 0),
                (retired, 1),
                (outer, 2),
                (map, 3),
                (set, 4),
                (record, 5),
            ] {
                assert_eq!(
                    arena
                        .facts(value.handle().unwrap())
                        .unwrap()
                        .ledger_entity()
                        .unwrap()
                        .ordinal,
                    ordinal
                );
            }
            assert!(arena.sequence(outer.handle().unwrap()).unwrap()[0].same(&first));
            assert!(arena.sequence(tuple.handle().unwrap()).unwrap()[0].same(&outer));
            let map_entries = arena.entries(map.handle().unwrap()).unwrap();
            assert_eq!(map_entries.len(), 2);
            assert!(map_entries[0].same(&key));
            assert!(map_entries[1].same(&Value::Int(2)));
            let record_entries = arena.entries(record.handle().unwrap()).unwrap();
            assert_eq!(record_entries.len(), 2);
            assert!(record_entries[1].same(&Value::Int(3)));
            assert_eq!(arena.entries(set.handle().unwrap()).unwrap().len(), 4);
            assert_eq!(
                ledger.balance().unwrap().entities,
                5,
                "retired interior storage consumes an ordinal but no source membership"
            );
        }
    }
}

pub(super) fn prepared_literals_and_captures(
    machine: &crate::machine::KernelMachine,
    identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
    code: lash_kernel_doc::arena::LocalCode,
) {
    use super as boundary;
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, Owner, RootValue, Slot},
    };
    use std::sync::Arc;
    let heap = machine.source_law_heap();
    let link = identity.callable.link;
    let function = identity.callable.function;
    let owner = |slot| Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(slot),
    };
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let cell = arena.new_cell(Value::Int(7));
    let declaring = Value::ObjectRef(cell);
    ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            heap,
            &[],
            &[(owner(0), RootValue::Value(&declaring))],
        )
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("capture fits"))
        .publish(&mut ledger, |_, _| {})
        .unwrap();
    let before = arena.next_provisional();
    let dependencies = Dependencies::capture(&ledger, &arena, &[cell]).unwrap();
    let mut graph = boundary::prepare_closure(
        &mut arena,
        heap,
        Arc::clone(&identity),
        vec![cell],
        code,
        owner(1),
    )
    .unwrap();
    let abandoned = graph.outputs[0].value;
    graph
        .registration
        .arena()
        .cell_set(cell, Value::Int(8))
        .unwrap();
    let (transaction, storage) = graph
        .into_boundary(&ledger, heap, &[owner(0)], dependencies)
        .unwrap();
    assert!(
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("capture fits"))
            .publish(&mut ledger, |arena, _| storage.batch.publish(arena))
            .is_err(),
        "a changed capture invalidates the entire construction"
    );
    assert_eq!(arena.next_provisional(), before);
    assert!(arena.body(abandoned.handle().unwrap()).is_err());
    let dependencies = Dependencies::capture(&ledger, &arena, &[cell]).unwrap();
    let graph =
        boundary::prepare_closure(&mut arena, heap, identity, vec![cell], code, owner(1)).unwrap();
    let closure = graph.outputs[0].value;
    let (transaction, storage) = graph
        .into_boundary(&ledger, heap, &[owner(0)], dependencies)
        .unwrap();
    transaction
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("capture fits"))
        .publish(&mut ledger, |arena, _| storage.batch.publish(arena))
        .unwrap();
    assert!(ledger.holding(owner(0), heap.pages()).unwrap().is_none());
    arena.release(&mut |visit| visit(&closure));
    assert!(
        arena.cell(cell).unwrap().same(&Value::Int(8)),
        "the closure keeps its capture after the declaring root leaves"
    );
    assert_eq!(arena.next_provisional().ordinal, before.ordinal + 1);
    for (slot, literal) in [
        doc::Literal::Int(doc::Integer::parse("18446744073709551616").unwrap()),
        doc::Literal::Text(
            "literal text longer than the small text representation must keep its exact contents"
                .into(),
        ),
        doc::Literal::Bytes(doc::Bytes::new(vec![1, 2, 3])),
        doc::Literal::Function(function),
    ]
    .into_iter()
    .enumerate()
    {
        let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
        let graph =
            boundary::prepare_literal(&mut arena, heap, &literal, link, owner(slot as u32 + 2))
                .unwrap();
        assert!(
            graph.entities.is_empty(),
            "an immutable literal consumes no entity ordinal"
        );
        let value = graph.outputs[0].value;
        let holding = graph.outputs[0].holding.bytes();
        let (transaction, storage) = graph
            .into_boundary(&ledger, heap, &[], dependencies)
            .unwrap();
        assert_eq!(storage.outputs.len(), 1);
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("literal fits"))
            .publish(&mut ledger, |arena, _| storage.batch.publish(arena))
            .unwrap();
        assert_eq!(
            ledger
                .holding(owner(slot as u32 + 2), heap.pages())
                .unwrap()
                .unwrap()
                .bytes(),
            holding
        );
        match literal {
            doc::Literal::Int(expected) => assert_eq!(
                arena.semantics().integer(&value).unwrap().unwrap(),
                expected
            ),
            doc::Literal::Text(expected) => {
                assert_eq!(arena.semantics().text(&value).unwrap(), expected.as_str())
            }
            doc::Literal::Bytes(expected) => assert_eq!(
                arena.semantics().bytes(value.handle().unwrap()).unwrap(),
                expected.as_slice()
            ),
            doc::Literal::Function(expected) => assert_eq!(
                arena.callable(value.handle().unwrap()).unwrap().function,
                expected
            ),
            _ => unreachable!(),
        }
    }
    assert_eq!(arena.next_provisional().ordinal, before.ordinal + 1);
}
