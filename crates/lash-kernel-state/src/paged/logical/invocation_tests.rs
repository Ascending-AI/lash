//! Transactional identity laws exercised by the existing source-admission law.
use super::*;

/// Closed virtual calls still consume identities. A rejected or abandoned
/// prefix consumes none, and cold reopening never recycles committed IDs.
pub(super) fn completed_prefix() {
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
