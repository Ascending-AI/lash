use super::heap::{Source, fresh};
use crate::paged::SealBudget;
use crate::paged::census_impl::TraceLimits;
use crate::paged::logical::*;
use crate::paged::tree::Access;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity};
use std::rc::Rc;

fn id(n: u64) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: n,
    }
}
fn owner(n: u32) -> Owner {
    Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(n),
    }
}
fn frame() -> Owner {
    Owner::Frame {
        task: TaskId::MAIN,
        invocation: 0,
    }
}
fn value(bytes: u64, entities: &[u64], access: &Access<'_>) -> Holding {
    Holding::with_edges(
        bytes,
        entities.iter().map(|n| Identity::Object(id(*n))),
        access,
    )
    .unwrap()
}
fn admit(ledger: &mut Ledger, changes: Vec<Change>, bound: u64, access: &Access<'_>) {
    let prepared = ledger.prepare(changes, access).unwrap();
    let accepted = match prepared.admit(bound) {
        Ok(a) => a,
        Err(p) => panic!("source peak {} exceeds {bound}", p.peak()),
    };
    ledger.commit(accepted).unwrap();
}
fn budget() -> SealBudget {
    SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }
}

/// LogicalHeldV1 source transfers conserve occurrences and reclaim the last
/// acyclic body across cold restore. A speculative admission publishes nothing.
#[test]
fn source_transfers_release_bodies_once_and_preserve_nested_aliases() {
    let (heap, pages) = fresh();
    let access = heap.pages().clone();
    let mut ledger = Ledger::fresh();
    let mut arena = crate::arena::Arena::new();
    // A failed prefix cannot reserve an identity or cache an edge to the
    // identity subsequently assigned to another source allocation.
    let discarded = arena.create_list(&[]);
    let tuple = arena.tuple(vec![discarded], 256).unwrap();
    {
        let mut registration = arena.registration().unwrap();
        Holding::from_native(registration.arena(), &access, &tuple).unwrap();
        assert_eq!(registration.arena().next_ordinal(), 1);
    }
    assert_eq!(arena.next_ordinal(), 0);
    let child = arena.create_list(&[lash_kernel_doc::Value::Int(7)]);
    let child_handle = child.handle().unwrap();
    assert_eq!(arena.identify(child_handle).unwrap(), id(0));
    {
        let mut registration = arena.registration().unwrap();
        let holding = Holding::from_native(registration.arena(), &access, &tuple).unwrap();
        let mut edges = Vec::new();
        holding
            .visit_edges(&access, &mut |target, _| {
                edges.push(target);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            edges,
            [Identity::Object(id(1))],
            "discarded tuple caches cannot alias a later allocation"
        );
    }
    assert_eq!(arena.next_ordinal(), 1);

    let parent = arena.create_list(&[child, child]);
    let parent_handle = parent.handle().unwrap();
    assert_eq!(arena.identify(parent_handle).unwrap(), id(1));
    let seals = pages.borrow().seals;
    let prepared = ledger
        .prepare_ownership(&mut arena, &access, &[], &[(frame(), RootValue::Frame)])
        .unwrap()
        .then_ownership(
            &mut arena,
            &access,
            &[],
            &[
                (owner(0), RootValue::Value(&parent)),
                (owner(1), RootValue::Value(&child)),
            ],
        )
        .unwrap();
    assert_eq!(
        pages.borrow().seals,
        seals,
        "holding acquisition does not project payloads"
    );
    assert_eq!(prepared.peak(), 209); // 49 + 64 + frame 64 + two references 32.
    let prepared = match prepared.admit(208) {
        Err(p) => p,
        Ok(_) => panic!("understated source room accepted"),
    };
    assert_eq!(ledger.balance().unwrap(), Balance::default());
    let accepted = match prepared.admit(209) {
        Ok(a) => a,
        Err(_) => panic!("exact peak refused"),
    };
    ledger.commit(accepted).unwrap();
    assert_eq!(
        ledger.balance().unwrap(),
        Balance {
            entity_local_bytes: 113,
            root_and_control_bytes: 96,
            entities: 2
        }
    );

    assert!(
        matches!(
            ledger.prepare_ownership(
                &mut arena,
                &access,
                &[],
                &[(owner(0), RootValue::Value(&child)),]
            ),
            Err(HeapFault::Malformed(_))
        ),
        "an overwrite must consume its old owner"
    );
    assert_eq!(
        ledger.release_cost(&[owner(0)], &access).unwrap(),
        80,
        "the other alias retains the child's body"
    );
    assert_eq!(
        ledger.release_cost(&[owner(0), owner(1)], &access).unwrap(),
        145,
        "a group releases each body once"
    );
    let stamp = ledger.stamp();
    for result in [
        RootValue::Value(&child),
        RootValue::Page(&lash_kernel_doc::heap::PageValue::List(id(0))),
    ] {
        assert_eq!(
            ledger
                .release_cost_preserving(
                    &mut arena,
                    &heap,
                    &[owner(0), owner(1)],
                    &[(owner(4), result)]
                )
                .unwrap(),
            96,
            "an outgoing result keeps the child's body while both input occurrences leave"
        );
    }
    let wrapper = arena.create_list(&[child]);
    {
        let mut registration = arena.registration().unwrap();
        assert_eq!(
            ledger
                .release_cost_preserving(
                    registration.arena(),
                    &heap,
                    &[owner(0), owner(1)],
                    &[(owner(4), RootValue::Value(&wrapper))]
                )
                .unwrap(),
            96,
            "a new result body protects the old entities in its members"
        );
    }
    assert!(
        ledger.is_current(&stamp),
        "pure source release evaluation publishes no owner or graph"
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 209);
    // The tuple has two 16-byte aliases, plus its 16-byte outer occurrence.
    let tuple = arena.tuple(vec![child, child], 256).unwrap();
    let tuple_holding = Holding::from_native(&mut arena, heap.pages(), &tuple).unwrap();
    let stored = lash_kernel_doc::heap::PageValue::Tuple(
        crate::paged::stored::pages::tuple(
            &access,
            vec![
                lash_kernel_doc::heap::PageValue::List(id(0)),
                lash_kernel_doc::heap::PageValue::List(id(0)),
            ],
        )
        .unwrap(),
    );
    let cold_holding = Holding::from_page(heap.pages(), &stored).unwrap();
    assert_eq!(tuple_holding.bytes(), cold_holding.bytes());
    admit(
        &mut ledger,
        vec![Change::Roots(vec![
            (owner(0), None),
            (owner(1), None),
            (owner(2), Some(tuple_holding)),
        ])],
        209,
        &access,
    );
    assert_eq!(
        ledger.balance().unwrap(),
        Balance {
            entity_local_bytes: 49,
            root_and_control_bytes: 112,
            entities: 1
        }
    );
    admit(
        &mut ledger,
        vec![
            Change::Move {
                from: owner(2),
                to: owner(3),
            },
            Change::Copy {
                from: owner(3),
                to: owner(2),
            },
            Change::Roots(vec![(owner(3), None)]),
            Change::Roots(vec![(owner(2), Some(cold_holding))]),
        ],
        209,
        &access,
    );
    assert!(ledger.holding(owner(3), &access).unwrap().is_none());
    let root = ledger.seal(&access, budget()).unwrap().unwrap();
    access.clear();
    let reads = pages.borrow().reads;
    let mut cold = Ledger::open(root).unwrap();
    assert_eq!(
        pages.borrow().reads,
        reads,
        "opening balances does not walk the heap"
    );
    let cold_access = Access::new(Box::new(Source(Rc::clone(&pages))));
    admit(
        &mut cold,
        vec![Change::Roots(vec![(owner(2), None)])],
        161,
        &cold_access,
    );
    assert_eq!(
        cold.balance().unwrap(),
        Balance {
            entity_local_bytes: 0,
            root_and_control_bytes: 64,
            entities: 0
        }
    );
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(owner(2), None)])],
        161,
        &access,
    );
    assert_eq!(ledger.balance().unwrap(), cold.balance().unwrap());
    // Entering a closure adopts reach to its cells, rather than creating
    // another value occurrence for every physical capture slot.
    let capture = Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Capture(0),
    };
    let cell = arena.new_cell(lash_kernel_doc::Value::Int(7));
    assert_eq!(arena.identify(cell).unwrap(), id(2));
    let variable = lash_kernel_doc::Value::ObjectRef(cell);
    let prepared = ledger
        .prepare_ownership(
            &mut arena,
            &access,
            &[],
            &[(owner(0), RootValue::Value(&variable))],
        )
        .unwrap();
    ledger
        .commit(
            prepared
                .admit(129)
                .unwrap_or_else(|_| panic!("variable did not fit")),
        )
        .unwrap();
    let prepared = ledger
        .prepare_ownership(
            &mut arena,
            &access,
            &[owner(0)],
            &[(capture, RootValue::Capture(cell))],
        )
        .unwrap();
    ledger
        .commit(
            prepared
                .admit(129)
                .unwrap_or_else(|_| panic!("capture did not fit")),
        )
        .unwrap();
    assert!(
        matches!(
            ledger.prepare_ownership(
                &mut arena,
                &access,
                &[],
                &[(
                    Owner::Value {
                        task: TaskId::MAIN,
                        invocation: 0,
                        slot: Slot::Capture(1)
                    },
                    RootValue::Capture(child_handle)
                ),]
            ),
            Err(HeapFault::Malformed(_))
        ),
        "a list cannot claim a borrowed variable owner"
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 113);
    for holding in [
        value(16, &[2], &access),
        Holding::scalar(0),
        value(0, &[2, 2], &access),
        Holding::with_edges(0, [Identity::Task(TaskId(7))], &access).unwrap(),
    ] {
        assert!(matches!(
            ledger.prepare([Change::Roots(vec![(capture, Some(holding))])], &access),
            Err(HeapFault::Malformed(_))
        ));
        assert_eq!(ledger.balance().unwrap().total().unwrap(), 113);
    }
    assert!(matches!(
        ledger.prepare(
            [Change::Roots(vec![(
                owner(0),
                Some(value(0, &[2], &access))
            )])],
            &access
        ),
        Err(HeapFault::Malformed(_))
    ));
    let root = ledger.seal(&access, budget()).unwrap().unwrap();
    let mut cold = Ledger::open(root).unwrap();
    assert_eq!(cold.balance().unwrap(), ledger.balance().unwrap());
    admit(
        &mut cold,
        vec![Change::Roots(vec![(capture, None)])],
        113,
        &access,
    );
    assert_eq!(cold.balance().unwrap().total().unwrap(), 64);
    let computed = arena.create_list(&[
        lash_kernel_doc::Value::Int(1),
        lash_kernel_doc::Value::Int(2),
    ]);
    {
        let registration = arena.registration().unwrap();
        let prepared = cold
            .prepare_publication(
                registration,
                &access,
                &[],
                &[(owner(4), RootValue::Value(&computed))],
            )
            .unwrap();
        assert_eq!(prepared.balance().unwrap().entity_local_bytes, 66);
        let prepared = prepared
            .admit(145)
            .err()
            .expect("65 bytes of entity headroom is insufficient");
        let accepted = prepared
            .admit(146)
            .unwrap_or_else(|_| panic!("66-byte entity must fit its exact event"));
        accepted
            .publish(&mut cold, |_, edit| assert!(edit.is_empty()))
            .unwrap();
    }
    assert_eq!(cold.balance().unwrap().total().unwrap(), 146);
    admit(
        &mut cold,
        vec![Change::Roots(vec![(owner(4), None)])],
        146,
        &access,
    );
    let first = lash_kernel_doc::Value::Int(1);
    let second = lash_kernel_doc::Value::Int(2);
    let prepared = cold
        .prepare_ownership(
            &mut arena,
            &access,
            &[],
            &[
                (owner(5), RootValue::Value(&first)),
                (owner(6), RootValue::Value(&second)),
            ],
        )
        .unwrap();
    cold.commit(
        prepared
            .admit(98)
            .unwrap_or_else(|_| panic!("evaluated operands must fit")),
    )
    .unwrap();
    let constructed = arena.create_list(&[first, second]);
    let registration = arena.registration().unwrap();
    let prepared = cold
        .prepare_publication(
            registration,
            &access,
            &[owner(5), owner(6)],
            &[(owner(7), RootValue::Value(&constructed))],
        )
        .unwrap();
    assert_eq!(
        prepared.peak(),
        146,
        "moving occurrences into members is not a second copy"
    );
    prepared
        .admit(146)
        .unwrap_or_else(|_| panic!("only the header and result reference grow"))
        .publish(&mut cold, |_, edit| assert!(edit.is_empty()))
        .unwrap();
    let third = lash_kernel_doc::Value::Int(3);
    let prepared = cold
        .prepare_ownership(
            &mut arena,
            &access,
            &[],
            &[(owner(8), RootValue::Value(&third))],
        )
        .unwrap();
    cold.commit(
        prepared
            .admit(163)
            .unwrap_or_else(|_| panic!("the RHS must fit")),
    )
    .unwrap();
    let registration = arena.registration().unwrap();
    let member = cold
        .prepare_member_publication(
            registration,
            &access,
            constructed.handle().unwrap(),
            Member::List(2),
            Some(third),
            &[owner(8)],
            &[],
        )
        .unwrap();
    assert_eq!(
        member.peak(),
        163,
        "an RHS moves into a member at one frontier"
    );
    let member = member
        .admit(163)
        .unwrap_or_else(|_| panic!("member transfer adds no occurrence"));
    member
        .publish(&mut cold, |arena, edit| {
            let MemberEdit {
                handle,
                member,
                next,
            } = edit.into_iter().next().unwrap();
            let Member::List(index) = member else {
                panic!("list member");
            };
            arena.list_set(handle, index, next.unwrap()).unwrap();
        })
        .unwrap();
    assert!(cold.holding(owner(8), &access).unwrap().is_none());
    assert_eq!(cold.balance().unwrap().entity_local_bytes, 83);
    // Reusing a physical frame or slot cannot reuse a logical activation.
    // The next ID is journaled, and survives a cold open without an owner walk.
    let mut frames = Ledger::fresh();
    fn enter<'a>(
        ledger: &Ledger,
        arena: &'a mut crate::arena::Arena,
        access: &Access<'_>,
        value: &lash_kernel_doc::Value,
    ) -> (Invocation, PreparedPublication<'a>) {
        ledger
            .prepare_enter_publication(
                TaskId::MAIN,
                arena.registration().unwrap(),
                access,
                &[],
                &[(Slot::Binding(0), RootValue::Value(value))],
            )
            .unwrap()
    }
    let (first, attempt) = enter(&frames, &mut arena, &access, &constructed);
    assert!(attempt.admit(162).is_err());
    let (same, attempt) = enter(&frames, &mut arena, &access, &constructed);
    assert_eq!(first, same, "abandoned entry consumes no activation");
    attempt
        .admit(163)
        .unwrap_or_else(|_| panic!("entry fits"))
        .publish(&mut frames, |_, edit| assert!(edit.is_empty()))
        .unwrap();
    let exit = frames
        .prepare_leave_publication(
            &first,
            arena.registration().unwrap(),
            &access,
            &[(
                Owner::Ended(TaskId::MAIN),
                RootValue::Existing(first.owner(Slot::Binding(0))),
            )],
        )
        .unwrap();
    exit.admit(163)
        .unwrap_or_else(|_| panic!("return transfers its result"))
        .publish(&mut frames, |_, edit| assert!(edit.is_empty()))
        .unwrap();
    assert_eq!(frames.balance().unwrap().total().unwrap(), 99);
    let sealed = frames.seal(&access, budget()).unwrap().unwrap();
    let mut frames = Ledger::open(sealed).unwrap();
    let (second, attempt) = enter(&frames, &mut arena, &access, &constructed);
    assert_ne!(
        first.id(),
        second.id(),
        "cold frame reuse must retain the activation cursor"
    );
    attempt
        .admit(179)
        .unwrap_or_else(|_| panic!("second alias has one body"))
        .publish(&mut frames, |_, edit| assert!(edit.is_empty()))
        .unwrap();
    assert!(
        frames
            .invocation(TaskId::MAIN, first.id(), &access)
            .is_err()
    );
    assert_eq!(
        frames.invocation_owners(&second, &access).unwrap(),
        [second.frame(), second.owner(Slot::Binding(0))]
    );
    // The live heap owns the ledger. A cached physical certificate cannot
    // hide a newer source-root publication when sealing the same heap again.
    {
        use crate::paged::{EditEpoch, SemanticHeap};
        let (mut live, pages) = fresh();
        let mut arena = crate::arena::Arena::new();
        let lifecycle = Lifecycle::new(live.logical_ledger());
        let invocation = lifecycle
            .enter(
                TaskId::MAIN,
                &mut arena,
                &live,
                &[],
                &[(
                    Slot::Binding(0),
                    SourceValue::Value(lash_kernel_doc::Value::Int(7)),
                )],
            )
            .unwrap();
        live.seal(budget()).unwrap().unwrap();
        let original = live.sealed_root().unwrap();
        lifecycle.replace(
            invocation.owner(Slot::Binding(0)),
            Some(SourceValue::Value(lash_kernel_doc::Value::Int(0))),
        );
        lifecycle.flush(&mut arena, &live).unwrap();
        assert!(
            live.sealed_root().is_none(),
            "a source publication invalidates the old heap header"
        );
        live.seal(budget()).unwrap().unwrap();
        assert_ne!(original, live.sealed_root().unwrap());
        let reads = pages.borrow().payload_reads;
        let restored = crate::paged::heap_impl::PagedHeap::open_header(
            live.sealed_root().unwrap(),
            Box::new(Source(pages.clone())),
            EditEpoch(2),
        )
        .unwrap();
        assert_eq!(
            restored
                .logical_ledger()
                .borrow()
                .balance()
                .unwrap()
                .total()
                .unwrap(),
            80
        );
        assert_eq!(
            pages.borrow().payload_reads,
            reads,
            "ledger restore reads no scalar payloads"
        );
        assert!(
            restored
                .logical_ledger()
                .borrow()
                .invocation(TaskId::MAIN, invocation.id(), restored.pages())
                .is_ok()
        );
        // Canonical arena writes report member occurrences even when no root
        // changes. A later root overwrite retains the self-cycle until trace.
        arena.watch_logical_members();
        let cycle = arena.create_list(&[]);
        lifecycle.replace(
            invocation.owner(Slot::Binding(0)),
            Some(SourceValue::Value(cycle)),
        );
        lifecycle.flush(&mut arena, &live).unwrap();
        arena.list_set(cycle.handle().unwrap(), 0, cycle).unwrap();
        lifecycle.flush(&mut arena, &live).unwrap();
        lifecycle.replace(
            invocation.owner(Slot::Binding(0)),
            Some(SourceValue::Value(lash_kernel_doc::Value::Null)),
        );
        lifecycle.flush(&mut arena, &live).unwrap();
        assert_eq!(
            live.logical_ledger()
                .borrow()
                .balance()
                .unwrap()
                .entity_local_bytes,
            48,
            "runtime member events retain the cyclic component"
        );
        live.logical_ledger()
            .borrow_mut()
            .normalize(
                live.pages(),
                crate::paged::census_impl::TraceLimits::default(),
            )
            .unwrap();
        assert_eq!(
            live.logical_ledger()
                .borrow()
                .balance()
                .unwrap()
                .entity_local_bytes,
            0
        );
        // Quiet execution may discard the external occurrence before its
        // adapter flushes. The canonical mutation still owns a cycle identity.
        let elided = arena.create_list(&[]);
        arena.list_set(elided.handle().unwrap(), 0, elided).unwrap();
        lifecycle.flush(&mut arena, &live).unwrap();
        assert_eq!(
            live.logical_ledger()
                .borrow()
                .balance()
                .unwrap()
                .entity_local_bytes,
            48,
            "elided roots cannot erase cyclic membership"
        );
        live.logical_ledger()
            .borrow_mut()
            .normalize(
                live.pages(),
                crate::paged::census_impl::TraceLimits::default(),
            )
            .unwrap();
        assert_eq!(
            live.logical_ledger()
                .borrow()
                .balance()
                .unwrap()
                .entity_local_bytes,
            0
        );
    }

    // Distinct object and task identities preserve multiplicity through a
    // speculative member edit, a retained snapshot and a cold restore.
    for restore in [false, true] {
        use lash_kernel_doc::Value;
        let (heap, pages) = fresh();
        let mut access = heap.pages().clone();
        let mut arena = crate::arena::Arena::new();
        let mut ledger = Ledger::fresh();
        let first = arena.create_list(&[Value::Int(7)]);
        let second = arena.create_list(&[Value::Int(0)]);
        let nested = arena.tuple(vec![first, second], 256).unwrap();
        let parent = arena.create_list(&[nested, first, Value::Task(TaskId(7))]);
        let parent_id = arena.identify(parent.handle().unwrap()).unwrap();
        let first_id = arena.identify(first.handle().unwrap()).unwrap();
        let second_id = arena.identify(second.handle().unwrap()).unwrap();
        let prepared = ledger
            .prepare_ownership(
                &mut arena,
                &access,
                &[],
                &[
                    (owner(0), RootValue::Value(&parent)),
                    (owner(1), RootValue::Value(&second)),
                ],
            )
            .unwrap();
        // Parent: 32 + tuple(16 + 16 + 16) + alias 16 + task 16.
        // Child bodies: 49 and 48; two root occurrences: 32.
        assert_eq!(prepared.peak(), 241);
        ledger
            .commit(
                prepared
                    .admit(241)
                    .unwrap_or_else(|_| panic!("exact mixed body fits")),
            )
            .unwrap();
        let snapshot = ledger.entity_holding(parent_id, &access).unwrap().unwrap();
        if restore {
            let root = ledger.seal(&access, budget()).unwrap().unwrap();
            ledger = Ledger::open(root).unwrap();
            access = Access::new(Box::new(Source(pages)));
        }
        let prepared = ledger
            .prepare_member_publication(
                arena.registration().unwrap(),
                &access,
                parent.handle().unwrap(),
                Member::List(0),
                Some(Value::Int(0)),
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(prepared.balance().unwrap().total().unwrap(), 209);
        assert_eq!(
            ledger.release_cost(&[owner(0)], &access).unwrap(),
            177,
            "speculation preserves every old edge and the retained child control"
        );
        prepared
            .admit(241)
            .unwrap_or_else(|_| panic!("member shrink fits"))
            .publish(&mut ledger, |arena, edit| {
                for edit in edit {
                    let Member::List(index) = edit.member else {
                        panic!("list member")
                    };
                    arena
                        .list_set(edit.handle, index, edit.next.unwrap())
                        .unwrap();
                }
            })
            .unwrap();
        let mut edges = Vec::new();
        snapshot
            .visit_edges(&access, &mut |target, count| {
                edges.push((target, count));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            edges,
            [
                (Identity::Object(first_id), 2),
                (Identity::Object(second_id), 1),
                (Identity::Task(TaskId(7)), 1),
            ],
            "a retained version keeps the original identity multiplicities"
        );
        let root = ledger.seal(&access, budget()).unwrap().unwrap();
        let mut cold = Ledger::open(root).unwrap();
        admit(
            &mut cold,
            vec![Change::Roots(vec![(owner(0), None)])],
            209,
            &access,
        );
        assert_eq!(
            cold.balance().unwrap().total().unwrap(),
            64,
            "the parent's last owner reclaims its first child while the second stays rooted"
        );
        admit(
            &mut cold,
            vec![Change::Roots(vec![(owner(1), None)])],
            64,
            &access,
        );
        assert_eq!(cold.balance().unwrap(), Balance::default());
    }
}

/// K-BND-003 / K-MACH-008: source-retained cycles and unreferenced ended
/// results cannot become saved floors. Normalize before an exact refusal.
#[test]
fn pressure_reclaims_cycles_and_retires_only_unreached_results_after_restore() {
    for keep_result in [false, true] {
        for restore in [false, true] {
            let (heap, pages) = fresh();
            let mut access = heap.pages().clone();
            let mut ledger = Ledger::fresh();
            let mut arena = crate::arena::Arena::new();
            let mirror = arena.create_list(&[]);
            let mirror_handle = mirror.handle().unwrap();
            assert_eq!(arena.identify(mirror_handle).unwrap(), id(0));
            let empty_body = Holding::native_body(&mut arena, heap.pages(), mirror_handle).unwrap();
            admit(
                &mut ledger,
                vec![
                    Change::Allocate {
                        entity: id(0),
                        body: empty_body,
                    },
                    Change::Roots(vec![
                        (frame(), Some(Holding::frame())),
                        (
                            owner(0),
                            Some(Holding::from_native(&mut arena, heap.pages(), &mirror).unwrap()),
                        ),
                    ]),
                ],
                112,
                &access,
            );
            let registration = arena.registration().unwrap();
            let mutation = ledger
                .prepare_member_publication(
                    registration,
                    &access,
                    mirror_handle,
                    Member::List(0),
                    Some(mirror),
                    &[],
                    &[],
                )
                .unwrap();
            assert_eq!(mutation.balance().unwrap().entity_local_bytes, 48);
            let mutation = mutation
                .admit(128)
                .unwrap_or_else(|_| panic!("self edge must fit"));
            mutation
                .publish(&mut ledger, |arena, edit| {
                    let MemberEdit {
                        handle,
                        member,
                        next,
                    } = edit.into_iter().next().unwrap();
                    let Member::List(index) = member else {
                        panic!("list edit");
                    };
                    arena.list_set(handle, index, next.unwrap()).unwrap();
                })
                .unwrap();
            let result = arena.create_list(&[lash_kernel_doc::Value::Int(7)]);
            let result_handle = result.handle().unwrap();
            assert_eq!(arena.identify(result_handle).unwrap(), id(1));
            let result_body =
                Holding::native_body(&mut arena, heap.pages(), result_handle).unwrap();
            let mut roots = vec![(
                Owner::Ended(TaskId(7)),
                Some(Holding::from_native(&mut arena, heap.pages(), &result).unwrap()),
            )];
            if keep_result {
                roots.push((
                    Owner::Incoming(TaskId::MAIN),
                    Some(Holding::with_edges(16, [Identity::Task(TaskId(7))], &access).unwrap()),
                ));
            }
            admit(
                &mut ledger,
                vec![
                    Change::Allocate {
                        entity: id(1),
                        body: result_body,
                    },
                    Change::Roots(roots),
                    Change::Roots(vec![(owner(0), None)]),
                ],
                256,
                &access,
            );
            let retained = if keep_result { 193 } else { 177 };
            assert_eq!(ledger.balance().unwrap().total().unwrap(), retained);
            if restore {
                let root = ledger.seal(&access, budget()).unwrap().unwrap();
                ledger = Ledger::open(root).unwrap();
                access = Access::new(Box::new(Source(Rc::clone(&pages))));
                assert_eq!(ledger.balance().unwrap().total().unwrap(), retained);
            }
            let limit = if keep_result { 162 } else { 81 };
            let request = || vec![Change::Roots(vec![(owner(2), Some(Holding::scalar(17)))])];
            let pending = ledger.prepare(request(), &access).unwrap();
            assert!(pending.admit(limit).is_err());
            ledger.normalize(&access, TraceLimits::default()).unwrap();
            assert_eq!(
                ledger.balance().unwrap(),
                if keep_result {
                    Balance {
                        entity_local_bytes: 49,
                        root_and_control_bytes: 96,
                        entities: 1,
                    }
                } else {
                    Balance {
                        entity_local_bytes: 0,
                        root_and_control_bytes: 64,
                        entities: 0,
                    }
                }
            );
            // Completion-formula mirrors are host retention. They remain
            // readable after logical normalization retires their source cycle.
            arena.clean(mirror_handle).unwrap();
            arena.clean(result_handle).unwrap();
            let mut physical_roots = |visit: &mut crate::arena::Visit<'_>| visit(&mirror);
            arena.collect_cycles(&mut physical_roots);
            arena.release(&mut physical_roots);
            assert!(matches!(arena.item(mirror_handle, 0).unwrap(),
                Some(lash_kernel_doc::Value::List(handle)) if handle == mirror_handle));
            admit(&mut ledger, request(), limit, &access);
            assert_eq!(ledger.balance().unwrap().total().unwrap(), limit);
            if keep_result {
                admit(
                    &mut ledger,
                    vec![Change::Roots(vec![(Owner::Incoming(TaskId::MAIN), None)])],
                    limit,
                    &access,
                );
                ledger.normalize(&access, TraceLimits::default()).unwrap();
                assert_eq!(
                    ledger.balance().unwrap(),
                    Balance {
                        entity_local_bytes: 0,
                        root_and_control_bytes: 81,
                        entities: 0
                    }
                );
            }
        }
    }
}

/// Logical admission is tied to one unchanged source state. Host trace or
/// page failures cannot publish a partial root, and scratch cannot cross seal.
#[test]
fn failed_or_stale_source_admission_never_publishes_a_holding() {
    checked_program_and_warm_balance();
    let (heap, _) = fresh();
    let access = heap.pages().clone();
    let mut ledger = Ledger::fresh();
    assert!(
        matches!(
            ledger.prepare([Change::Roots(vec![(owner(3), None)])], &access),
            Err(HeapFault::Malformed(_))
        ),
        "releasing an absent source owner must reject the journal"
    );
    admit(
        &mut ledger,
        vec![
            Change::Allocate {
                entity: id(0),
                body: Holding::scalar(49),
            },
            Change::Roots(vec![
                (frame(), Some(Holding::frame())),
                (owner(9), Some(value(16, &[0], &access))),
            ]),
        ],
        129,
        &access,
    );
    let facts = ledger.stamp();
    let pending = ledger
        .prepare(
            [Change::Roots(vec![(owner(0), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap();
    let accepted = match pending.admit(146) {
        Ok(a) => a,
        Err(_) => panic!("17-byte occurrence did not fit"),
    };
    assert!(matches!(
        ledger.normalize(
            &access,
            TraceLimits {
                work: 0,
                ..Default::default()
            }
        ),
        Err(HeapFault::Exhausted { .. })
    ));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 129);
    assert!(
        ledger.is_current(&facts),
        "an aborted normalization changes no facts"
    );
    ledger.commit(accepted).unwrap();
    assert!(!ledger.is_current(&facts));
    assert!(matches!(
        ledger.prepare_at(&facts, [], &access),
        Err(HeapFault::Malformed(_))
    ));
    let stale = ledger
        .prepare(
            [Change::Roots(vec![(owner(1), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap();
    let stale = match stale.admit(163) {
        Ok(a) => a,
        Err(_) => panic!("second occurrence did not fit"),
    };
    ledger.normalize(&access, TraceLimits::default()).unwrap();
    assert!(matches!(ledger.commit(stale), Err(HeapFault::Malformed(_))));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 146);
    assert!(ledger.holding(owner(1), &access).unwrap().is_none());
    let before = ledger.balance().unwrap();
    let across_handoff = ledger
        .prepare_ownership(
            &mut crate::arena::Arena::new(),
            &access,
            &[],
            &[(owner(1), RootValue::Existing(owner(0)))],
        )
        .unwrap()
        .admit(163)
        .unwrap_or_else(|_| panic!("copy did not fit"));
    ledger.handoff().unwrap();
    assert!(matches!(
        ledger.commit(across_handoff),
        Err(HeapFault::Malformed(_))
    ));
    assert_eq!(
        ledger.balance().unwrap(),
        before,
        "handoff changes authority, not holdings"
    );
    let scratch = Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Scratch(2),
    };
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(scratch, Some(Holding::scalar(7)))])],
        153,
        &access,
    );
    assert!(matches!(
        ledger.seal(&access, budget()),
        Err(HeapFault::Malformed(_))
    ));
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(scratch, None)])],
        153,
        &access,
    );
    let across_seal = ledger
        .prepare(
            [Change::Roots(vec![(owner(1), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap()
        .admit(163)
        .unwrap_or_else(|_| panic!("copy did not fit"));
    assert!(ledger.seal(&access, budget()).unwrap().is_some());
    let published = std::cell::Cell::new(false);
    assert!(
        matches!(
            ledger.publish(across_seal, || published.set(true)),
            Err(HeapFault::Malformed(_))
        ),
        "seal is a hard handoff, even when all logical holdings stay equal"
    );
    assert!(
        !published.get(),
        "a stale journal cannot publish semantic changes"
    );
    assert_eq!(ledger.balance().unwrap(), before);
    let fresh = ledger
        .prepare(
            [Change::Roots(vec![(owner(1), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap()
        .admit(163)
        .unwrap_or_else(|_| panic!("copy did not fit"));
    ledger.publish(fresh, || published.set(true)).unwrap();
    assert!(published.get());
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 163);
    let mut arena = crate::arena::Arena::from_ordinal(1);
    let fresh_list = arena.create_list(&[lash_kernel_doc::Value::Int(9)]);
    let attempt = ledger
        .prepare_publication(
            arena.registration().unwrap(),
            &access,
            &[],
            &[(owner(2), RootValue::Value(&fresh_list))],
        )
        .unwrap();
    let attempt = match attempt.admit(163) {
        Err(rejected) => rejected,
        Ok(_) => panic!("a fresh source body cannot fit without its local price"),
    };
    drop(attempt);
    assert_eq!(
        arena.next_ordinal(),
        1,
        "abandoned publication returns identity grants"
    );
    let attempt = ledger
        .prepare_publication(
            arena.registration().unwrap(),
            &access,
            &[],
            &[(owner(2), RootValue::Value(&fresh_list))],
        )
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("unbounded publication fits"));
    ledger.handoff().unwrap();
    published.set(false);
    assert!(matches!(
        attempt.publish(&mut ledger, |_, _| published.set(true)),
        Err(HeapFault::Malformed(_))
    ));
    assert!(
        !published.get(),
        "stale publication cannot edit semantic storage"
    );
    assert_eq!(
        arena.next_ordinal(),
        1,
        "stale publication returns identity grants"
    );
    assert!(ledger.holding(owner(2), &access).unwrap().is_none());
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 163);
}

fn checked_program_and_warm_balance() {
    crate::arena::Arena::check_prepared_closure_edges();
    // A native batch rejected by a later table key must install no earlier body.
    {
        use lash_kernel_doc::{NewObject, Nursery, heap::LogicalBytes};
        let mut arena = crate::arena::Arena::new();
        let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
        let first = nursery.allocate(NewObject::List(vec![Value::Int(7)]));
        nursery.allocate(NewObject::Map(vec![(Value::Absent, first)]));
        let new = nursery.into_new();
        let before = (arena.held(), arena.allocations(), arena.next_ordinal());
        assert!(arena.install(new).is_err());
        assert!(
            arena.body(first.handle().unwrap()).is_err(),
            "a rejected batch installed its first body"
        );
        assert_eq!(
            (arena.held(), arena.allocations(), arena.next_ordinal()),
            before
        );
        let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
        let retry = nursery.allocate(NewObject::List(vec![]));
        assert!(retry.same(&first), "rejection returns the reserved slots");
        let mut staged = arena.prepare_install(nursery.into_new()).unwrap();
        let identity = EntityId {
            family: FamilyId::PRIMARY,
            ordinal: 0,
        };
        staged
            .bind_entity(retry.handle().unwrap(), identity)
            .unwrap();
        drop(staged);
        assert_eq!(
            (arena.held(), arena.allocations(), arena.next_ordinal()),
            before
        );
        let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
        let list = nursery.allocate(NewObject::List(vec![]));
        assert!(list.same(&retry), "abandonment returns the reserved slots");
        let text = nursery.text("a canonical long key with more than twenty three bytes");
        let map = nursery.allocate(NewObject::Map(vec![
            (text, Value::Int(1)),
            (text, Value::Int(2)),
        ]));
        let mut new = nursery.into_new();
        new[0].1 = NewObject::List(vec![text, list]);
        arena.prepare_install(new).unwrap().publish();
        assert!(
            arena
                .table_get(map.handle().unwrap(), &text)
                .unwrap()
                .unwrap()
                .same(&Value::Int(2))
        );
        let members = arena.sequence(list.handle().unwrap()).unwrap();
        assert_eq!(members.len(), 2);
        assert!(members[0].same(&text) && members[1].same(&list));
        assert_eq!(arena.allocations(), 2);
        let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
        nursery.allocate(NewObject::List(vec![list]));
        let staged = arena.prepare_install(nursery.into_new()).unwrap();
        let (mut registration, batch) = staged.into_parts();
        assert!(batch.is_current(registration.arena()));
        registration
            .arena()
            .list_set(list.handle().unwrap(), 0, Value::Null)
            .unwrap();
        assert!(
            !batch.is_current(registration.arena()),
            "changed member facts expire prepared storage"
        );
        drop(registration);
        let mut next = arena.registration().unwrap();
        assert!(
            !batch.is_current(next.arena()),
            "another registration cannot reuse abandoned preparation"
        );
    }
    use crate::arena::ProxyKind;
    use crate::paged::{EditEpoch, ObjectInit, SemanticHeap};
    use lash_kernel_doc::{Value, heap::PageValue};
    let (mut heap, pages) = fresh();
    let entity = heap.allocate_object(ObjectInit::List(vec![])).unwrap();
    let mut arena = crate::arena::Arena::from_ordinal(heap.next_ordinal());
    arena.watch_logical_members();
    let lifecycle = Lifecycle::new(heap.logical_ledger());
    let root = Owner::Incoming(TaskId::MAIN);
    lifecycle.replace(root, Some(SourceValue::Page(PageValue::List(entity))));
    lifecycle.flush(&mut arena, &heap).unwrap();
    let balance = heap.logical_balance().unwrap();
    assert_eq!(balance.total().unwrap(), 48);
    let handle = arena.proxy(entity, ProxyKind::List);
    lifecycle.handoff(&mut arena, &heap).unwrap();
    let fact = heap
        .logical_ledger()
        .borrow()
        .authenticate_warm(&arena, handle, heap.pages())
        .unwrap();
    arena.open_list(handle, 0).unwrap();
    assert_eq!(
        fact.finish(&heap.logical_ledger().borrow(), &arena)
            .unwrap(),
        balance,
        "warming authenticates an existing fact without minting a physical claim"
    );
    heap.record_capacity(1 << 20);
    let sealed = heap.seal(budget()).unwrap().unwrap();
    assert_eq!(
        sealed.root.logical.balance().unwrap(),
        balance,
        "seal publishes the source balance"
    );
    let opened = crate::paged::heap_impl::PagedHeap::open_header(
        heap.sealed_root().unwrap(),
        Box::new(Source(pages)),
        EditEpoch(3),
    )
    .unwrap();
    assert_eq!(
        opened.logical_balance().unwrap(),
        balance,
        "open restores ledger summaries rather than physical capacity"
    );
    assert_eq!(opened.certificate().capacity, 1 << 20);

    let (heap, _) = fresh();
    let mut arena = crate::arena::Arena::new();
    let mut ledger = Ledger::fresh();
    let id = EntityId {
        family: FamilyId::PRIMARY,
        ordinal: 0,
    };
    let make = || {
        Program::new(vec![
            JournalEvent {
                entities: vec![VirtualEntity {
                    entity: id,
                    body: VirtualBody::List(vec![]),
                }],
                outputs: vec![(owner(0), PublicationValue::Page(PageValue::List(id)))],
                ..Default::default()
            },
            JournalEvent {
                entities: vec![VirtualEntity {
                    entity: id,
                    body: VirtualBody::List(vec![PageValue::List(id)]),
                }],
                ..Default::default()
            },
            JournalEvent {
                inputs: vec![owner(0)],
                outputs: vec![(owner(0), PublicationValue::Native(Value::Null))],
                ..Default::default()
            },
        ])
    };
    let evaluated = make().evaluate(&ledger, &arena).unwrap();
    let prepared = evaluated
        .prepare(&ledger, arena.registration().unwrap(), &heap, make())
        .unwrap();
    assert_eq!(
        prepared.balance().unwrap().entity_local_bytes,
        48,
        "an elided source cycle remains itemized"
    );
    assert_eq!(prepared.peak(), 64);
    let materializations = prepared
        .admit(64)
        .unwrap()
        .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
        .unwrap();
    assert_eq!(materializations.entities().len(), 1);
    materializations
        .publish(&mut ledger, |recipes| assert_eq!(recipes[0].entity, id))
        .unwrap();
    ledger
        .normalize(heap.pages(), TraceLimits::default())
        .unwrap();
    assert_eq!(ledger.balance().unwrap().entity_local_bytes, 0);
    // Equal-size actual scalar substitutions are not the evaluated recipe.
    let make = |value| {
        Program::new(vec![JournalEvent {
            inputs: vec![owner(0)],
            outputs: vec![(owner(0), PublicationValue::Native(value))],
            ..Default::default()
        }])
    };
    let evaluated = make(Value::Int(1)).evaluate(&ledger, &arena).unwrap();
    assert!(
        evaluated
            .prepare(
                &ledger,
                arena.registration().unwrap(),
                &heap,
                make(Value::Int(2))
            )
            .is_err()
    );
    let entity = EntityId {
        family: FamilyId::PRIMARY,
        ordinal: arena.next_ordinal(),
    };
    let make = || {
        Program::new(vec![JournalEvent {
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::List(vec![PageValue::Int(1.into()), PageValue::Int(2.into())]),
            }],
            outputs: vec![(owner(1), PublicationValue::Page(PageValue::List(entity)))],
            ..Default::default()
        }])
    };
    let expected = make().evaluate(&ledger, &arena).unwrap();
    let mut nursery = lash_kernel_doc::Nursery::new(
        arena.objects(),
        lash_kernel_doc::heap::LogicalBytes(u64::MAX),
    );
    let refused_value = nursery.allocate(lash_kernel_doc::NewObject::List(vec![
        Value::Int(1),
        Value::Int(2),
    ]));
    let mut storage = arena.prepare_install(nursery.into_new()).unwrap();
    storage
        .bind_entity(refused_value.handle().unwrap(), entity)
        .unwrap();
    let (registration, _batch) = storage.into_parts();
    let prepared = expected
        .prepare(&ledger, registration, &heap, make())
        .unwrap();
    assert_eq!(prepared.balance().unwrap().entity_local_bytes, 66);
    assert!(
        prepared.admit(97).is_none(),
        "65 entity bytes after all containing obligations cannot fit 66"
    );
    assert_eq!(
        arena.next_ordinal(),
        entity.ordinal,
        "a declined prefix consumes no identity"
    );
    assert!(
        arena.body(refused_value.handle().unwrap()).is_err(),
        "refused admission installs no prepared body"
    );
    assert_eq!(arena.allocations(), 0);
    // One publication remains. Storage installed in that publication must not
    // need a second fallible handoff after the guest body becomes visible.
    ledger.leave_one_publication_for_law();
    let expected = make().evaluate(&ledger, &arena).unwrap();
    let mut nursery = lash_kernel_doc::Nursery::new(
        arena.objects(),
        lash_kernel_doc::heap::LogicalBytes(u64::MAX),
    );
    let value = nursery.allocate(lash_kernel_doc::NewObject::List(vec![
        Value::Int(1),
        Value::Int(2),
    ]));
    assert!(
        value.same(&refused_value),
        "refused admission returns the prepared slot"
    );
    let new = nursery.into_new();
    let mut storage = arena.prepare_install(new).unwrap();
    storage
        .bind_entity(value.handle().unwrap(), entity)
        .unwrap();
    let (mut registration, batch) = storage.into_parts();
    assert!(registration.arena().body(value.handle().unwrap()).is_err());
    let prepared = expected
        .prepare(&ledger, registration, &heap, make())
        .unwrap();
    let materializations = prepared
        .admit(98)
        .unwrap()
        .publish_materialized(&mut ledger, |arena, edits, entities| {
            assert!(edits.is_empty());
            assert_eq!(entities.len(), 1);
            assert_eq!(entities[0].entity, entity);
            batch.publish(arena);
        })
        .unwrap();
    assert_eq!(arena.logical_handle(entity), value.handle());
    assert_eq!(arena.logical_local(&value).unwrap(), 16);
    assert_eq!(arena.sequence(value.handle().unwrap()).unwrap().len(), 2);
    assert_eq!(ledger.balance().unwrap().entity_local_bytes, 66);
    assert!(materializations.entities().is_empty());

    // A shared binding installs its cell only in the admitted journal. Its
    // member keeps the computed occurrence price, without pooling the RHS.
    let (heap, _) = fresh();
    let mut arena = crate::arena::Arena::new();
    let mut ledger = Ledger::fresh();
    let entity = EntityId {
        family: FamilyId::PRIMARY,
        ordinal: arena.next_ordinal(),
    };
    let make = || {
        Program::new(vec![JournalEvent {
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::NativeVariable(Value::Int(256)),
            }],
            outputs: vec![(
                owner(0),
                PublicationValue::Page(PageValue::Ref(Identity::Object(entity))),
            )],
            ..Default::default()
        }])
    };
    let expected = make().evaluate(&ledger, &arena).unwrap();
    let (refused, mut storage) = arena.prepare_cell(Value::Int(256)).unwrap();
    storage.bind_entity(refused, entity).unwrap();
    let (mut registration, _batch) = storage.into_parts();
    assert!(registration.arena().body(refused).is_err());
    let prepared = expected
        .prepare(&ledger, registration, &heap, make())
        .unwrap();
    assert_eq!(prepared.peak(), 66);
    assert!(prepared.admit(65).is_none());
    assert!(arena.body(refused).is_err());
    assert_eq!(arena.next_ordinal(), entity.ordinal);
    assert_eq!(arena.allocations(), 0);
    let expected = make().evaluate(&ledger, &arena).unwrap();
    let (cell, mut storage) = arena.prepare_cell(Value::Int(256)).unwrap();
    assert_eq!(cell, refused, "abandonment returns the fresh cell's slot");
    storage.bind_entity(cell, entity).unwrap();
    let (registration, batch) = storage.into_parts();
    let prepared = expected
        .prepare(&ledger, registration, &heap, make())
        .unwrap();
    prepared
        .admit(66)
        .unwrap()
        .publish_materialized(&mut ledger, |arena, edits, entities| {
            assert!(edits.is_empty());
            assert_eq!(entities.len(), 1);
            batch.publish(arena);
        })
        .unwrap();
    assert!(arena.cell(cell).unwrap().same(&Value::Int(256)));
    assert_eq!(arena.logical_handle(entity), Some(cell));
    assert_eq!(ledger.balance().unwrap().entity_local_bytes, 50);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 66);

    // A closure captures a cell edge, not another guest value occurrence.
    // The existing cell remains shared by its binding and the new closure.
    let closure = EntityId {
        family: FamilyId::PRIMARY,
        ordinal: arena.next_ordinal(),
    };
    let make = || {
        Program::new(vec![JournalEvent {
            entities: vec![VirtualEntity {
                entity: closure,
                body: VirtualBody::Closure(vec![entity]),
            }],
            outputs: vec![(
                owner(1),
                PublicationValue::Page(PageValue::Closure(closure)),
            )],
            ..Default::default()
        }])
    };
    let expected = make().evaluate(&ledger, &arena).unwrap();
    let prepared = expected
        .prepare(&ledger, arena.registration().unwrap(), &heap, make())
        .unwrap();
    assert_eq!(
        prepared.peak(),
        122,
        "a captured cell edge costs 8, not a 16-byte holding"
    );
    assert_eq!(prepared.balance().unwrap().entity_local_bytes, 90);
    assert!(prepared.admit(121).is_none());
    let (_, members) = VirtualBody::Closure(vec![entity])
        .native_members(&heap, &arena, &Default::default())
        .unwrap();
    assert_eq!(
        members[0].bytes, 8,
        "the native recipe uses the same closure-edge price"
    );
}
