//! Transactional identity laws exercised by the existing source-admission law.
use super::*;

/// Closed virtual calls still consume identities. A rejected or abandoned
/// prefix consumes none, and cold reopening never recycles committed IDs.
pub(super) fn completed_prefix() {
    entered_boundary();
    use crate::paged::{EditEpoch, SemanticHeap};
    use lash_kernel_doc::Value;
    let (mut heap, pages) = fresh();
    let access = heap.pages().clone();
    let ledger_store = heap.logical_ledger();
    let mut ledger = ledger_store.borrow_mut();
    let range = ledger.reserve_invocations(TaskId::MAIN, 4).unwrap();
    let first = range.invocation(0).unwrap().id();
    assert_eq!(range.len(), 4);
    assert!(range.invocation(4).is_none());
    for offset in 0..4 {
        assert_eq!(range.invocation(offset).unwrap().id(), first + offset);
    }
    drop(range);
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first);
    assert!(ledger.reserve_invocations(TaskId::MAIN, u64::MAX).is_none());

    let range = || ledger.reserve_invocations(TaskId::MAIN, 4).unwrap();
    let prepare = || ledger.prepare([], &access).unwrap();
    let abandoned = prepare().complete_invocations(range(), 4, &access).unwrap();
    drop(abandoned);
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first);
    let rejected = ledger
        .prepare(
            [Change::Roots(vec![(owner(0), Some(Holding::scalar(16)))])],
            &access,
        )
        .unwrap()
        .complete_invocations(range(), 4, &access)
        .unwrap();
    assert!(rejected.admit(15).is_err());
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first);
    assert!(prepare().complete_invocations(range(), 5, &access).is_err());
    assert!(
        Ledger::fresh()
            .prepare([], &access)
            .unwrap()
            .complete_invocations(range(), 1, &access)
            .is_err(),
        "same numeric identities in another ledger grant no authority"
    );
    let mut arena = crate::arena::Arena::new();
    for (offset, completed) in [(0, 1), (0, 0), (2, 1)] {
        let reserved = range();
        let activation = reserved.invocation(offset).unwrap();
        let retained = ledger
            .prepare_ownership(
                &mut arena,
                &access,
                &[],
                &[(
                    activation.owner(Slot::Binding(0)),
                    RootValue::Value(&Value::Null),
                )],
            )
            .unwrap();
        assert!(
            retained
                .complete_invocations(reserved, completed, &access)
                .is_err(),
            "neither completed nor unused reserved calls can retain a headerless binding"
        );
    }

    let stale_range = range();
    let stale = prepare()
        .complete_invocations(range(), 4, &access)
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("empty exit fits"));
    ledger.handoff().unwrap();
    assert!(
        ledger
            .prepare([], &access)
            .unwrap()
            .complete_invocations(stale_range, 4, &access)
            .is_err()
    );
    assert!(ledger.commit(stale).is_err());
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first);

    let mut left_arena = crate::arena::Arena::new();
    let mut right_arena = crate::arena::Arena::new();
    let left = ledger
        .prepare_source_boundary(left_arena.registration().unwrap(), &heap, &[], &[])
        .unwrap()
        .complete_invocations(
            ledger.reserve_invocations(TaskId::MAIN, 4).unwrap(),
            2,
            &access,
        )
        .unwrap();
    let right = ledger
        .prepare_source_boundary(right_arena.registration().unwrap(), &heap, &[], &[])
        .unwrap()
        .complete_invocations(
            ledger.reserve_invocations(TaskId::MAIN, 4).unwrap(),
            4,
            &access,
        )
        .unwrap();
    left.admit(u64::MAX)
        .unwrap_or_else(|_| panic!("empty exit fits"))
        .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
        .unwrap();
    assert!(
        right
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("empty exit fits"))
            .publish(&mut ledger, |_, _| panic!(
                "stale boundary cannot mutate storage"
            ))
            .is_err()
    );
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first + 2);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 0);
    for id in first..first + 2 {
        assert!(ledger.invocation(TaskId::MAIN, id, &access).is_err());
    }
    drop(ledger);
    heap.seal(budget()).unwrap().unwrap();
    let reopened = crate::paged::heap_impl::PagedHeap::open_header(
        heap.sealed_root().unwrap(),
        Box::new(Source(pages)),
        EditEpoch(3),
    )
    .unwrap();
    let reopened_store = reopened.logical_ledger();
    let mut reopened_ledger = reopened_store.borrow_mut();
    assert_eq!(
        reopened_ledger.prospective_invocation(TaskId::MAIN).id(),
        first + 2
    );
    assert_eq!(reopened_ledger.balance().unwrap().total().unwrap(), 0);
    let accepted = reopened_ledger
        .prepare([], reopened.pages())
        .unwrap()
        .complete_invocations(
            reopened_ledger
                .reserve_invocations(TaskId::MAIN, 1)
                .unwrap(),
            1,
            reopened.pages(),
        )
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("empty exit fits"));
    reopened_ledger.commit(accepted).unwrap();
    assert_eq!(
        reopened_ledger.prospective_invocation(TaskId::MAIN).id(),
        first + 3
    );
}

/// K-MACH-008: frame entry and caller roots consume one actual invocation only
/// after the prepared parameter graph and its source peak win admission.
fn entered_boundary() {
    use lash_kernel_doc::{Value, heap::PageValue};
    let (heap, _) = fresh();
    let mut arena = crate::arena::Arena::new();
    let mut ledger = Ledger::fresh();
    let (caller, enter) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(enter.admit(u64::MAX).ok().unwrap()).unwrap();
    let first = caller.owner(Slot::Operand(0));
    let second = caller.owner(Slot::Operand(1));
    let initial = ledger
        .prepare_source_ownership(
            &mut arena,
            &heap,
            &[],
            &[
                (first, RootValue::Value(&Value::Int(7))),
                (second, RootValue::Value(&Value::Int(9))),
            ],
        )
        .unwrap();
    ledger
        .commit(initial.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let invocation = ledger.prospective_invocation(TaskId::MAIN);
    let cell = arena.next_provisional();
    let page = PageValue::Ref(Identity::Object(cell));
    let entities = [VirtualEntity {
        entity: cell,
        body: VirtualBody::Variable(PageValue::Int(9.into())),
    }];
    let inputs = [first, second];
    let outputs = [
        (
            caller.owner(Slot::Continuation(0)),
            RootValue::Existing(first),
        ),
        (invocation.owner(Slot::Binding(0)), RootValue::Page(&page)),
    ];
    for foreign in [false, true] {
        let mut foreign_ledger = Ledger::fresh();
        let (_, entered) = foreign_ledger
            .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
            .unwrap();
        foreign_ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let other = foreign_ledger.prospective_invocation(TaskId::MAIN);
        assert_eq!(other.id(), invocation.id());
        let prepared = ledger.prepare_frame_boundary(
            arena.registration().unwrap(),
            &heap,
            if foreign { &other } else { &invocation },
            &inputs,
            &outputs,
            &entities,
        );
        if foreign {
            assert!(
                prepared.is_err(),
                "another ledger cannot lend an equal numeric invocation"
            );
            drop(prepared);
        } else {
            drop(prepared.unwrap());
        }
        assert_eq!(ledger.prospective_invocation(TaskId::MAIN), invocation);
        assert_eq!(arena.next_provisional(), cell);
    }
    assert!(
        ledger
            .prepare_frame_boundary(
                arena.registration().unwrap(),
                &heap,
                &invocation,
                &inputs,
                &[(
                    invocation.owner(Slot::Operand(0)),
                    RootValue::Existing(first)
                )],
                &[]
            )
            .is_err()
    );
    assert!(
        ledger
            .prepare_frame_boundary(
                arena.registration().unwrap(),
                &heap,
                &invocation,
                &inputs,
                &[(invocation.frame(), RootValue::Frame)],
                &[]
            )
            .is_err()
    );
    let before = ledger.balance().unwrap();
    let prepared = ledger
        .prepare_frame_boundary(
            arena.registration().unwrap(),
            &heap,
            &invocation,
            &inputs,
            &outputs,
            &entities,
        )
        .unwrap();
    let peak = prepared.peak();
    assert!(
        prepared.admit(peak - 1).is_err(),
        "frame header and cell graph both belong to the source peak"
    );
    assert_eq!(ledger.balance().unwrap(), before);
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN), invocation);
    assert_eq!(arena.next_provisional(), cell);
    let stale = ledger
        .prepare_frame_boundary(
            arena.registration().unwrap(),
            &heap,
            &invocation,
            &inputs,
            &outputs,
            &entities,
        )
        .unwrap()
        .admit(u64::MAX)
        .ok()
        .unwrap();
    ledger.handoff().unwrap();
    assert!(
        stale
            .publish(&mut ledger, |_, _| panic!(
                "stale frame entry cannot publish storage"
            ))
            .is_err()
    );
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN), invocation);
    assert_eq!(arena.next_provisional(), cell);
    let prepared = ledger
        .prepare_frame_boundary(
            arena.registration().unwrap(),
            &heap,
            &invocation,
            &inputs,
            &outputs,
            &entities,
        )
        .unwrap();
    let mut applied = false;
    prepared
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |_, edits| {
            assert!(edits.is_empty());
            applied = true;
        })
        .unwrap();
    assert!(applied);
    assert_eq!(
        ledger
            .invocation(TaskId::MAIN, invocation.id(), heap.pages())
            .unwrap(),
        invocation
    );
    assert_eq!(
        ledger.prospective_invocation(TaskId::MAIN).id(),
        invocation.id() + 1
    );
    assert!(ledger.holding(first, heap.pages()).unwrap().is_none());
    assert!(ledger.holding(second, heap.pages()).unwrap().is_none());
    assert_eq!(
        ledger
            .holding(caller.owner(Slot::Continuation(0)), heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    assert_eq!(
        ledger
            .holding(invocation.owner(Slot::Binding(0)), heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        16
    );
    assert!(ledger.entity_holding(cell, heap.pages()).unwrap().is_some());
    assert_ne!(arena.next_provisional(), cell);
    assert!(
        ledger
            .prepare_frame_boundary(
                arena.registration().unwrap(),
                &heap,
                &invocation,
                &[],
                &[],
                &[]
            )
            .is_err(),
        "an entered invocation cannot enter twice"
    );
}
