//! Independent body snapshots must never overwrite another member's write.
use super::*;
use lash_kernel_doc::{self as doc, TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Dependencies, Ledger, Member, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn final_bodies() {
    shared_batch_child();
    let run = Embedder::new("main { return null }");
    let heap = run.machine.source_law_heap();
    for kind in 0..4 {
        let mut arena = Arena::new();
        let child = arena.create_list(&[Value::Int(9)]);
        let child_handle = child.handle().unwrap();
        arena.list_set(child_handle, 1, child).unwrap();
        let key = arena.text("original");
        let other = arena.text("other");
        let value = if kind == 3 {
            Value::ObjectRef(arena.new_cell(Value::Int(1)))
        } else {
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let value = nursery.allocate(match kind {
                0 => doc::NewObject::List(vec![Value::Int(1), child, child]),
                1 => doc::NewObject::Map(vec![
                    (key, Value::Int(1)),
                    (other, child),
                    (Value::Int(1), child),
                ]),
                _ => doc::NewObject::Record(vec![(key, Value::Int(1)), (other, child)]),
            });
            arena.install(nursery.into_new()).unwrap();
            value
        };
        let handle = value.handle().unwrap();
        let (first, second) = match kind {
            0 => (Member::List(0), Member::List(1)),
            1 => (Member::Table(key), Member::Table(other)),
            2 => (
                Member::Field(Arc::from("original")),
                Member::Field(Arc::from("other")),
            ),
            _ => (Member::Variable, Member::Variable),
        };
        let mut ledger = Ledger::fresh();
        let (invocation, entered) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[
                    (Slot::Binding(0), RootValue::Value(&value)),
                    (Slot::Binding(1), RootValue::Value(&value)),
                    (Slot::Binding(2), RootValue::Value(&child)),
                ],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let before = ledger.balance().unwrap();
        let version = arena.content_version(handle).unwrap();
        let large = arena.text("a longer unequal price");
        let foreign = arena.new_cell(Value::Int(1));
        let allocations = arena.allocations();
        let mut prepared = arena.prepare_member_body(handle).unwrap().unwrap();
        // Unequal immediate prices, followed by two aliases of the same body.
        for value in [Value::Int(123456), large, Value::Int(7)] {
            assert!(prepared.stage(&arena, &first, value).unwrap());
            assert!(prepared.read(&arena, &first).unwrap().unwrap().same(&value));
        }
        assert!(prepared.stage(&arena, &second, Value::Int(11)).unwrap());
        if kind == 1 {
            let float_key = Member::Table(Value::Float(doc::Float::new(1.0)));
            assert!(prepared.stage(&arena, &float_key, Value::Int(19)).unwrap());
            assert!(
                prepared
                    .stage(&arena, &Member::Table(Value::Int(1)), Value::Int(23))
                    .unwrap()
            );
            assert!(
                prepared
                    .read(&arena, &float_key)
                    .unwrap()
                    .unwrap()
                    .same(&Value::Int(23))
            );
        } else if kind == 2 {
            assert!(
                prepared
                    .stage(&arena, &Member::Table(key), Value::Int(7))
                    .unwrap()
            );
        } else if kind == 0 {
            assert!(
                prepared
                    .stage(&arena, &Member::List(3), Value::Int(17))
                    .unwrap()
            );
            assert!(
                prepared
                    .stage(&arena, &Member::List(3), Value::Int(19))
                    .unwrap()
            );
        }
        assert!(arena.content_is(&version));
        assert_eq!(arena.allocations(), allocations);
        assert_eq!(ledger.balance().unwrap(), before);
        let dependencies = Dependencies::capture(&ledger, &arena, &[handle]).unwrap();
        let prepared = prepared.seal(&mut arena).unwrap().unwrap();
        assert!(
            prepared
                .read(&arena, &second)
                .unwrap()
                .unwrap()
                .same(&Value::Int(11))
        );
        let final_edits = prepared.edits().to_vec();
        assert!(prepared.matches_edits(&final_edits));
        let mut wrong_cell = final_edits.clone();
        for edit in &mut wrong_cell {
            edit.handle = foreign;
        }
        assert!(
            !prepared.matches_edits(&wrong_cell),
            "equal-price foreign target cannot install a private shadow"
        );
        let mut wrong_value = final_edits.clone();
        wrong_value[0].next = Some(Value::Int(111));
        assert!(
            !prepared.matches_edits(&wrong_value),
            "same-price substituted final value is not the staged body"
        );
        assert_eq!(
            final_edits.len(),
            match kind {
                0 | 1 => 3,
                2 => 2,
                _ => 1,
            }
        );
        let boundary = ledger
            .prepare_source_member_bodies_boundary(
                arena.registration().unwrap(),
                heap,
                &[&final_edits],
                &[],
                &[],
            )
            .unwrap()
            .bind_dependencies(dependencies)
            .unwrap();
        let final_balance = boundary.balance().unwrap();
        // Refusal and abandonment leave every original member and version.
        assert!(boundary.admit(0).is_err());
        assert!(arena.content_is(&version));
        assert_eq!(ledger.balance().unwrap(), before);
        let boundary = ledger
            .prepare_source_member_bodies_boundary(
                arena.registration().unwrap(),
                heap,
                &[&final_edits],
                &[],
                &[],
            )
            .unwrap();
        drop(boundary);
        assert!(arena.content_is(&version));
        assert!(
            ledger
                .prepare_source_member_bodies_boundary(
                    arena.registration().unwrap(),
                    heap,
                    &[&final_edits, &final_edits],
                    &[],
                    &[],
                )
                .is_err(),
            "two final snapshots of one body are invalid"
        );
        let duplicate = [final_edits[0].clone(), final_edits[0].clone()];
        assert!(
            ledger
                .prepare_source_member_bodies_boundary(
                    arena.registration().unwrap(),
                    heap,
                    &[&duplicate],
                    &[],
                    &[],
                )
                .is_err(),
            "one final member cannot occur twice"
        );
        prepared
            .prepare_boundary(&ledger, arena.registration().unwrap(), heap, &[], &[])
            .unwrap()
            .unwrap()
            .admit(u64::MAX)
            .ok()
            .unwrap()
            .publish(&mut ledger, |arena, edits| {
                prepared.publish(arena, edits);
            })
            .unwrap();
        assert_eq!(ledger.balance().unwrap(), final_balance);
        assert!(!arena.content_is(&version));
        match kind {
            0 => {
                assert!(arena.item(handle, 0).unwrap().unwrap().same(&Value::Int(7)));
                assert!(
                    arena
                        .item(handle, 1)
                        .unwrap()
                        .unwrap()
                        .same(&Value::Int(11))
                );
                assert!(arena.item(handle, 2).unwrap().unwrap().same(&child));
                assert!(
                    arena
                        .item(handle, 3)
                        .unwrap()
                        .unwrap()
                        .same(&Value::Int(19))
                );
            }
            1 | 2 => {
                assert!(
                    arena
                        .table_get_text(handle, &doc::arena::ShortText::new("original").unwrap())
                        .unwrap()
                        .unwrap()
                        .same(&Value::Int(7))
                );
                assert!(
                    arena
                        .table_get_text(handle, &doc::arena::ShortText::new("other").unwrap())
                        .unwrap()
                        .unwrap()
                        .same(&Value::Int(11))
                );
            }
            _ => assert!(arena.cell(handle).unwrap().same(&Value::Int(11))),
        }
        if kind == 1 || kind == 2 {
            let entries = arena.entries(handle).unwrap();
            assert!(entries[0].same(&key));
            assert!(entries[2].same(&other));
            if kind == 1 {
                assert!(
                    entries[4].same(&Value::Int(1)),
                    "normal-equal overwrite preserves the original key spelling"
                );
                assert!(entries[5].same(&Value::Int(23)));
            }
            let (sequence, first_key) = arena.table_after(handle, None).unwrap().unwrap();
            assert_eq!(sequence, 0);
            assert!(first_key.same(&key));
            assert_eq!(
                arena
                    .table_after(handle, Some(sequence))
                    .unwrap()
                    .unwrap()
                    .0,
                1
            );
        }
        assert!(arena.item(child_handle, 1).unwrap().unwrap().same(&child));
        for slot in [Slot::Binding(0), Slot::Binding(1)] {
            assert!(
                ledger
                    .holding(invocation.owner(slot), heap.pages())
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            ledger
                .entity_holding(arena.ledger_key(child_handle).unwrap(), heap.pages())
                .unwrap()
                .is_some()
        );
        if kind == 3 {
            let mut stale = arena.prepare_member_body(handle).unwrap().unwrap();
            assert!(
                stale
                    .stage(&arena, &Member::Variable, Value::Int(12))
                    .unwrap()
            );
            let stale = stale.seal(&mut arena).unwrap().unwrap();
            arena.cell_set(handle, Value::Int(13)).unwrap();
            assert!(
                stale
                    .prepare_boundary(&ledger, arena.registration().unwrap(), heap, &[], &[])
                    .unwrap()
                    .is_none(),
                "stale original body declines before logical preparation"
            );
            assert_eq!(ledger.balance().unwrap(), final_balance);
            assert!(arena.cell(handle).unwrap().same(&Value::Int(13)));
        }
    }
}

/// Two final bodies transfer a shared child without dropping its surviving
/// member or installing independent stale snapshots of either target.
fn shared_batch_child() {
    let run = Embedder::new("main { return null }");
    let heap = run.machine.source_law_heap();
    let mut arena = Arena::new();
    let removed = arena.create_list(&[Value::Int(1)]);
    let retained = arena.create_list(&[Value::Int(2)]);
    let first = arena.new_cell(removed);
    let second = arena.new_cell(retained);
    let first_value = Value::ObjectRef(first);
    let second_value = Value::ObjectRef(second);
    let mut ledger = Ledger::fresh();
    let (_, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (Slot::Binding(0), RootValue::Value(&first_value)),
                (Slot::Binding(1), RootValue::Value(&second_value)),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let removed_id = arena.ledger_key(removed.handle().unwrap()).unwrap();
    let retained_id = arena.ledger_key(retained.handle().unwrap()).unwrap();
    let mut a = arena.prepare_member_body(first).unwrap().unwrap();
    let mut b = arena.prepare_member_body(second).unwrap().unwrap();
    assert!(a.stage(&arena, &Member::Variable, retained).unwrap());
    assert!(b.stage(&arena, &Member::Variable, Value::Int(7)).unwrap());
    let a = a.seal(&mut arena).unwrap().unwrap();
    let b = b.seal(&mut arena).unwrap().unwrap();
    let boundary = ledger
        .prepare_source_member_bodies_boundary(
            arena.registration().unwrap(),
            heap,
            &[a.edits(), b.edits()],
            &[],
            &[],
        )
        .unwrap();
    boundary
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |arena, mut edits| {
            let b_edits = edits.split_off(a.edits().len());
            a.publish(arena, edits);
            b.publish(arena, b_edits);
        })
        .unwrap();
    assert!(arena.cell(first).unwrap().same(&retained));
    assert!(arena.cell(second).unwrap().same(&Value::Int(7)));
    assert!(
        ledger
            .entity_holding(removed_id, heap.pages())
            .unwrap()
            .is_none()
    );
    assert!(
        ledger
            .entity_holding(retained_id, heap.pages())
            .unwrap()
            .is_some()
    );
}
