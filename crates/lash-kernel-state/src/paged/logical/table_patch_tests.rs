use super::*;
use crate::arena::Arena;
use lash_kernel_doc::{Float, NewObject, Value};

/// R3, K-KEY-003 and K-ITER-003: preparing one insertion must examine only
/// that entry, while refusal, stale admission and acceptance preserve the
/// original spellings, aliases and insertion sequences.
#[test]
fn table_patch_preparation_is_bounded_and_preserves_atomic_members() {
    let (heap, _) = fresh();
    let mut arena = Arena::new();
    let names: Vec<_> = (0..1000).map(|at| format!("k{at}")).collect();
    let mut entries: Vec<_> = names
        .iter()
        .map(|name| (arena.text(name), Value::Int(1)))
        .collect();
    entries.push((Value::Int(1), Value::Int(1)));
    let map = arena.create(NewObject::Map(entries)).unwrap();
    let handle = map.handle().unwrap();
    let alias = map;
    let mut ledger = Ledger::fresh();
    ledger
        .transfer_source(
            &mut arena,
            &heap,
            &[],
            &[
                (owner(0), RootValue::Value(&map)),
                (owner(1), RootValue::Value(&alias)),
            ],
        )
        .unwrap();
    let before = 64 + names.iter().map(|name| 33 + name.len() as u64).sum::<u64>() + 34;
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    assert_eq!(arena.entries(handle).unwrap().len(), 2002);
    let key = arena.text("later");
    crate::perf_witness::enable_work();
    let patch = arena
        .prepare_table_member(handle, key, Some(Value::Int(1)))
        .unwrap()
        .unwrap();
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert!(
        work.flat_size_members <= 4,
        "one entry cannot reprice the prior 1001 entries: {} member visits",
        work.flat_size_members
    );
    drop(patch);
    assert_eq!(arena.entries(handle).unwrap().len(), 2002);
    let peak = before + 38;
    for stale in [false, true] {
        let patch = arena
            .prepare_table_member(handle, key, Some(Value::Int(1)))
            .unwrap()
            .unwrap();
        let edit = patch.edit();
        let boundary = ledger
            .prepare_member_publication(
                arena.registration().unwrap(),
                heap.pages(),
                handle,
                edit.member,
                edit.next,
                &[],
                &[],
            )
            .unwrap();
        if stale {
            let accepted = boundary
                .admit(peak)
                .unwrap_or_else(|_| panic!("exact peak fits"));
            ledger.handoff().unwrap();
            let vm = std::cell::Cell::new(7);
            assert!(
                accepted
                    .publish(&mut ledger, |arena, edits| {
                        vm.set(99);
                        patch.publish(arena, edits.into_iter().next().unwrap());
                    })
                    .is_err()
            );
            assert_eq!(vm.get(), 7);
        } else {
            assert!(boundary.admit(peak - 1).is_err());
            drop(patch);
        }
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
        assert!(arena.table_get(handle, &key).unwrap().is_none());
        assert_eq!(
            arena.table_after(handle, Some(999)).unwrap().unwrap().0,
            1000
        );
    }
    let patch = arena
        .prepare_table_member(handle, key, Some(Value::Int(1)))
        .unwrap()
        .unwrap();
    let edit = patch.edit();
    let boundary = ledger
        .prepare_member_publication(
            arena.registration().unwrap(),
            heap.pages(),
            handle,
            edit.member,
            edit.next,
            &[],
            &[],
        )
        .unwrap();
    boundary
        .admit(peak)
        .unwrap_or_else(|_| panic!("exact peak fits"))
        .publish(&mut ledger, |arena, edits| {
            patch.publish(arena, edits.into_iter().next().unwrap())
        })
        .unwrap();
    assert!(
        arena
            .table_get(alias.handle().unwrap(), &key)
            .unwrap()
            .unwrap()
            .same(&Value::Int(1))
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), peak);
    assert_eq!(
        arena.table_after(handle, Some(1000)).unwrap().unwrap().0,
        1001
    );
    let alternate = Value::Float(Float::new(1.0));
    let patch = arena
        .prepare_table_member(handle, alternate, Some(Value::Int(2)))
        .unwrap()
        .unwrap();
    let edit = patch.edit();
    let boundary = ledger
        .prepare_member_publication(
            arena.registration().unwrap(),
            heap.pages(),
            handle,
            edit.member,
            edit.next,
            &[],
            &[],
        )
        .unwrap();
    boundary
        .admit(peak)
        .unwrap_or_else(|_| panic!("equal-size overwrite fits"))
        .publish(&mut ledger, |arena, edits| {
            patch.publish(arena, edits.into_iter().next().unwrap())
        })
        .unwrap();
    let (sequence, spelling) = arena.table_after(handle, Some(999)).unwrap().unwrap();
    assert_eq!(sequence, 1000);
    assert!(spelling.same(&Value::Int(1)));
    assert!(
        arena
            .table_get(handle, &alternate)
            .unwrap()
            .unwrap()
            .same(&Value::Int(2))
    );
    publish_member(
        &mut arena,
        &mut ledger,
        &heap,
        handle,
        alternate,
        None,
        peak,
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), peak - 34);
    let (sequence, spelling) = arena.table_after(handle, Some(999)).unwrap().unwrap();
    assert_eq!(sequence, 1001);
    assert!(spelling.same(&key));
    publish_member(
        &mut arena,
        &mut ledger,
        &heap,
        handle,
        alternate,
        Some(Value::Int(1)),
        peak - 1,
    );
    let (sequence, spelling) = arena.table_after(handle, Some(1001)).unwrap().unwrap();
    assert_eq!(sequence, 1002);
    assert!(spelling.same(&alternate));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), peak - 1);
    publish_member(
        &mut arena,
        &mut ledger,
        &heap,
        handle,
        alternate,
        None,
        peak - 1,
    );
    publish_member(
        &mut arena,
        &mut ledger,
        &heap,
        handle,
        Value::Int(1),
        Some(Value::Int(1)),
        peak,
    );
    let (sequence, spelling) = arena.table_after(handle, Some(1001)).unwrap().unwrap();
    assert_eq!(sequence, 1003);
    assert!(spelling.same(&Value::Int(1)));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), peak);
    assert!(ledger.release(&[owner(0)], heap.pages()).unwrap());
    assert_eq!(ledger.balance().unwrap().total().unwrap(), peak - 16);
    assert!(ledger.release(&[owner(1)], heap.pages()).unwrap());
    assert_eq!(ledger.balance().unwrap(), Balance::default());
}

fn publish_member(
    arena: &mut Arena,
    ledger: &mut Ledger,
    heap: &crate::kernel_internals::PagedHeap,
    handle: lash_kernel_doc::arena::ArenaHandle,
    key: Value,
    next: Option<Value>,
    bound: u64,
) {
    let patch = arena
        .prepare_table_member(handle, key, next)
        .unwrap()
        .unwrap();
    let edit = patch.edit();
    let boundary = ledger
        .prepare_member_publication(
            arena.registration().unwrap(),
            heap.pages(),
            handle,
            edit.member,
            edit.next,
            &[],
            &[],
        )
        .unwrap();
    boundary
        .admit(bound)
        .unwrap_or_else(|_| panic!("source peak fits"))
        .publish(ledger, |arena, edits| {
            patch.publish(arena, edits.into_iter().next().unwrap())
        })
        .unwrap();
}
