use super::*;
use crate::arena::Arena;
use lash_kernel_doc::{NewObject, Value, heap::PageValue};

fn exit(
    lease: &mut StateLease<'_>,
    inputs: impl Into<Box<[Owner]>>,
    outputs: impl Into<Box<[(Owner, PublicationValue)]>>,
    peak: u64,
    delta: i128,
) -> StateExit {
    region_fixtures::exit(
        lease.materialize_graph().unwrap(),
        inputs,
        outputs,
        peak,
        delta,
    )
}
fn accept(lease: &mut StateLease<'_>, op: LeaseOp<'_>) -> (i128, u64, Option<LeaseValue>) {
    let step = lease.stage(op).unwrap();
    let facts = (step.facts().delta(), step.facts().peak());
    let value = step.reserve().unwrap().accept();
    (facts.0, facts.1, value)
}
fn allocate(lease: &mut StateLease<'_>, allocation: LeaseAllocation<'_>) -> LeaseValue {
    accept(lease, LeaseOp::Allocate(allocation)).2.unwrap()
}

/// ISA-D: every shared-ledger writer obeys the affine lease. Invocation bounds
/// remain tied to the actual caller and cannot be reused by a sibling lease.
#[test]
fn region_lease_excludes_alias_writers_and_sibling_invocations() {
    shared_arena_lease();
    let (mut heap, _) = fresh();
    let access = heap.pages().clone();
    let mut arena = Arena::new();
    let shared = heap.logical_ledger();
    let caller = shared
        .borrow_mut()
        .enter_source(TaskId::MAIN, &mut arena, &heap, &[], &[])
        .unwrap();
    let accepted = shared
        .borrow()
        .prepare([], &access)
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("empty boundary fits"));
    let before = shared.borrow().balance().unwrap();
    let stamp = shared.borrow().stamp();
    {
        let _borrow = shared.borrow_mut();
        assert!(StateLease::acquire(&mut heap, &mut arena).is_err());
    }
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let saved = lease.reserve_invocations(&caller, 2).unwrap();
    let foreign_vm = std::cell::Cell::new(7);
    assert!(
        shared
            .borrow_mut()
            .publish(accepted, || foreign_vm.set(99))
            .is_err()
    );
    assert_eq!(foreign_vm.get(), 7);
    assert!(
        shared
            .borrow()
            .reserve_invocations(TaskId::MAIN, 2)
            .is_none()
    );
    assert!(shared.borrow_mut().handoff().is_err());
    assert!(
        shared
            .borrow_mut()
            .release(&[caller.frame()], &access)
            .is_err()
    );
    assert!(
        shared
            .borrow_mut()
            .transfer(caller.frame(), Owner::Incoming(TaskId::MAIN), &access)
            .is_err()
    );
    assert!(
        shared
            .borrow_mut()
            .normalize(&access, TraceLimits::default())
            .is_err()
    );
    assert_eq!(shared.borrow().balance().unwrap(), before);
    assert!(shared.borrow().is_current(&stamp));
    drop(lease);
    let mut sibling = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let reservation = sibling.reserve_invocations(&caller, 2).unwrap();
    let mut rejected = exit(&mut sibling, [], [], 64, 0);
    rejected.invocations = Some(saved);
    assert!(sibling.prepare(rejected, 64).is_err());
    assert_eq!(shared.borrow().next_invocation(), 2);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    lease.reserve_invocations(&caller, 2).unwrap();
    let mut rejected = exit(&mut lease, [], [], 64, 0);
    rejected.invocations = Some(reservation);
    assert!(lease.prepare(rejected, 64).is_err());
    let mut foreign = Ledger::fresh();
    let mut foreign_arena = Arena::new();
    let other = foreign
        .enter_source(TaskId::MAIN, &mut foreign_arena, &heap, &[], &[])
        .unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    assert!(lease.reserve_invocations(&other, 1).is_err());
    let range = lease
        .reserve_invocations(&caller, 4)
        .unwrap()
        .prefix(2)
        .unwrap();
    assert_eq!(range.invocation(0).unwrap().id(), 2);
    let mut packet = exit(&mut lease, [], [], 64, 0);
    packet.invocations = Some(range);
    let prepared = lease.prepare(packet, 64).unwrap();
    assert!(shared.try_borrow_mut().is_err());
    let published = std::cell::Cell::new(0);
    prepared
        .commit(|| {
            assert!(shared.try_borrow_mut().is_err());
            published.set(1);
        })
        .unwrap();
    assert_eq!(published.get(), 1);
    assert_eq!(shared.borrow().next_invocation(), 4);
    assert_eq!(shared.borrow().balance().unwrap(), before);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let range = lease
        .reserve_invocations(&caller, 5)
        .unwrap()
        .prefix(1)
        .unwrap();
    let active = range.invocation(0).unwrap();
    let mut packet = exit(
        &mut lease,
        [],
        [
            (active.frame(), PublicationValue::Frame),
            (
                active.owner(Slot::Binding(0)),
                PublicationValue::Native(Value::Int(7)),
            ),
        ],
        145,
        81,
    );
    packet.invocations = Some(range);
    lease.prepare(packet, 145).unwrap().commit(|| {}).unwrap();
    assert_eq!(
        shared
            .borrow()
            .invocation(TaskId::MAIN, 4, &access)
            .unwrap(),
        active
    );
    assert_eq!(shared.borrow().next_invocation(), 5);
    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), 145);
    let mut unused = StateLease::acquire(&mut heap, &mut arena).unwrap();
    unused.reserve_invocations(&caller, 3).unwrap();
    let packet = exit(&mut unused, [], [], 145, 0);
    unused.prepare(packet, 145).unwrap().commit(|| {}).unwrap();
    assert_eq!(shared.borrow().next_invocation(), 5);
    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), 145);
}

/// ISA-D: dropping either phase capability preserves the reached prefix;
/// stale dependencies, foreign packets and failed final admission preserve
/// every canonical body, ledger root, identity cursor and invocation grant.
#[test]
fn region_failed_prepare_and_changed_dependencies_are_atomic() {
    cold_placeholder_requires_boundary();
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let shared = heap.logical_ledger();
    let child = arena.create_list(&[Value::Int(7)]);
    shared
        .borrow_mut()
        .transfer_source(
            &mut arena,
            &heap,
            &[],
            &[(owner(0), RootValue::Value(&child))],
        )
        .unwrap();
    let handle = child.handle().unwrap();
    let old = Dependencies::capture(&shared.borrow(), &arena, &[handle]).unwrap();
    arena.list_set(handle, 0, Value::Int(8)).unwrap();
    let before = shared.borrow().balance().unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    assert!(lease.bind_dependencies(old).is_err());
    drop(lease);
    for (peak, delta, bound) in [(64, 17, 81), (82, 17, 81), (82, 18, 82)] {
        let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
        let packet = exit(
            &mut lease,
            [],
            [(owner(1), PublicationValue::Native(Value::Int(1)))],
            peak,
            delta,
        );
        assert!(lease.prepare(packet, bound).is_err());
        assert_eq!(shared.borrow().balance().unwrap(), before);
        assert!(
            shared
                .borrow()
                .holding(owner(1), heap.pages())
                .unwrap()
                .is_none()
        );
        assert!(arena.item(handle, 0).unwrap().unwrap().same(&Value::Int(8)));
    }
    let orphan = arena.create_list(&[]);
    let orphan_tuple = arena.tuple(vec![orphan], 256).unwrap();
    let unbound_cursor = arena.next_provisional();
    let mut unbound = StateLease::acquire(&mut heap, &mut arena).unwrap();
    assert!(
        unbound.import(&orphan_tuple).is_err(),
        "an immutable view cannot grant an unretained child's source identity"
    );
    drop(unbound);
    assert_eq!(arena.next_provisional(), unbound_cursor);
    let cursor = arena.next_provisional();
    let allocations = arena.allocations();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let local = lease.import(&child).unwrap();
    let object = lease.object(local).unwrap();
    let nine = lease.import(&Value::Int(9)).unwrap();
    drop(
        lease
            .stage(LeaseOp::Member {
                object,
                member: LeaseMember::List(0),
                next: Some(nine),
            })
            .unwrap(),
    );
    let observed = lease.read(object, &LeaseMember::List(0)).unwrap();
    assert!(lease.scalar(observed).unwrap().same(&Value::Int(8)));
    drop(
        lease
            .stage(LeaseOp::Member {
                object,
                member: LeaseMember::List(0),
                next: Some(nine),
            })
            .unwrap()
            .reserve()
            .unwrap(),
    );
    let observed = lease.read(object, &LeaseMember::List(0)).unwrap();
    assert!(lease.scalar(observed).unwrap().same(&Value::Int(8)));
    let values = [local, local];
    let ready = lease
        .stage(LeaseOp::Allocate(LeaseAllocation::list(&values)))
        .unwrap()
        .reserve()
        .unwrap();
    let abandoned = ready.result().unwrap();
    drop(ready);
    assert!(lease.object(abandoned).is_err());
    let result = allocate(&mut lease, LeaseAllocation::list(&values));
    assert!(
        lease.object(abandoned).is_err(),
        "a dropped ready result cannot alias a later accepted slot"
    );
    for _ in 0..64 {
        let transient = allocate(&mut lease, LeaseAllocation::list(&[]));
        accept(
            &mut lease,
            LeaseOp::Release {
                value: transient,
                count: 1,
            },
        );
        assert!(lease.object(transient).is_err());
    }
    let work = region_fixtures::storage_work(&lease);
    assert_eq!(
        (work.slab_slots, work.live_objects),
        (3, 2),
        "dead phases reuse a slab slot instead of retaining iteration history"
    );

    assert!(
        lease
            .arena()
            .item(handle, 0)
            .unwrap()
            .unwrap()
            .same(&Value::Int(8))
    );
    let graph = lease.materialize_graph().unwrap();
    let value = graph.value(result).unwrap();
    let output = graph.publication(result).unwrap();
    assert!(
        lease
            .stage(LeaseOp::Hold {
                value: local,
                count: 1
            })
            .is_err()
    );
    let packet = region_fixtures::exit(graph, [], [(owner(1), output.clone())], 128, 80);
    assert!(lease.prepare(packet, 128).is_err());
    assert_eq!(arena.next_provisional(), cursor);
    assert_eq!(arena.allocations(), allocations);
    assert!(arena.body(value.handle().unwrap()).is_err());
    assert_eq!(shared.borrow().balance().unwrap(), before);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let child = lease.import(&child).unwrap();
    let values = [child, child];
    let _current = allocate(&mut lease, LeaseAllocation::list(&values));
    let packet = exit(&mut lease, [], [(owner(1), output)], 145, 80);
    assert!(lease.prepare(packet, 145).is_err());
    assert_eq!(arena.allocations(), allocations);
    assert_eq!(arena.next_provisional(), cursor);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let packet = exit(&mut lease, [], [], before.total().unwrap(), 0);
    let original = std::mem::replace(&mut *shared.borrow_mut(), Ledger::fresh());
    assert!(lease.prepare(packet, u64::MAX).is_err());
    *shared.borrow_mut() = original;
    assert_eq!(shared.borrow().balance().unwrap(), before);
}

// A proxy known only by a ref does not reveal the actual body's native kind.
fn cold_placeholder_requires_boundary() {
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let shared = heap.logical_ledger();
    let access = heap.pages().clone();
    admit(
        &mut shared.borrow_mut(),
        vec![
            Change::Allocate {
                entity: id(7),
                body: Holding::scalar(32),
            },
            Change::Roots(vec![(owner(0), Some(value(16, &[7], &access)))]),
        ],
        48,
        &access,
    );
    let handle = arena.proxy(id(7), crate::arena::ProxyKind::Unknown);
    let stamp = shared.borrow().stamp();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let record = lease.import(&Value::Record(handle)).unwrap();
    let object = lease.object(record).unwrap();
    let member = LeaseMember::Field(std::sync::Arc::from("field"));
    assert!(matches!(
        lease.read(object, &member),
        Err(LeaseFault::BoundaryRequired)
    ));
    assert!(matches!(
        lease.stage(LeaseOp::Member {
            object,
            member,
            next: Some(record),
        }),
        Err(LeaseFault::BoundaryRequired)
    ));
    assert!(matches!(
        lease.stage(LeaseOp::HoldCapture {
            cell: object,
            count: 1
        }),
        Err(LeaseFault::BoundaryRequired)
    ));
    drop(lease);
    assert!(arena.is_cold(handle).unwrap());
    assert!(shared.borrow().is_current(&stamp));
    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), 48);
}

/// ISA-D: the lease graph and the independent itemized source program agree
/// on duplicate edges, cycles, dead allocations, cascading source release and
/// identity progression. Each exit reaches its prefix through real phases.
#[test]
fn region_graph_projection_commutes_with_itemized_source() {
    atomic_source_root_transfer();
    linear_chain_release_and_binding();
    imported_tuple_reads();
    call_and_measure_views();
    list_and_field_overlays();
    table_region_exit();
    for cycle in [false, true] {
        let (mut heap, _) = fresh();
        let mut arena = Arena::new();
        let shared = heap.logical_ledger();
        let base = arena.next_provisional();
        let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
        let seven = lease.import(&Value::Int(7)).unwrap();
        let b = allocate(
            &mut lease,
            LeaseAllocation::list(if cycle {
                &[]
            } else {
                std::slice::from_ref(&seven)
            }),
        );
        if cycle {
            let object = lease.object(b).unwrap();
            accept(
                &mut lease,
                LeaseOp::Member {
                    object,
                    member: LeaseMember::List(0),
                    next: Some(b),
                },
            );
        }
        let children = [b, b];
        let a = allocate(&mut lease, LeaseAllocation::list(&children));
        accept(&mut lease, LeaseOp::Release { value: b, count: 1 });
        let dead = allocate(&mut lease, LeaseAllocation::list(&[]));
        accept(
            &mut lease,
            LeaseOp::Release {
                value: dead,
                count: 1,
            },
        );
        assert!(lease.object(dead).is_err());
        let graph = lease.materialize_graph().unwrap();
        let av = graph.value(a).unwrap();
        let bv = graph.value(b).unwrap();
        let output = graph.publication(a).unwrap();
        let expected = if cycle { 128 } else { 129 };
        let peak = expected + 48;
        lease
            .prepare(
                region_fixtures::exit(graph, [], [(owner(0), output)], peak, expected.into()),
                peak,
            )
            .unwrap()
            .commit(|| {})
            .unwrap();
        assert!(
            arena
                .item(av.handle().unwrap(), 0)
                .unwrap()
                .unwrap()
                .same(&bv)
        );
        assert!(
            arena
                .item(av.handle().unwrap(), 1)
                .unwrap()
                .unwrap()
                .same(&bv)
        );
        assert_eq!(arena.next_provisional().ordinal, base.ordinal + 3);
        let ids = [
            EntityId {
                family: base.family,
                ordinal: base.ordinal + 1,
            },
            base,
            EntityId {
                family: base.family,
                ordinal: base.ordinal + 2,
            },
        ];
        let (oracle_heap, _) = fresh();
        let mut oracle_arena = Arena::new();
        let mut oracle = Ledger::fresh();
        let mut event = JournalEvent {
            entities: vec![
                VirtualEntity {
                    entity: ids[0],
                    body: VirtualBody::List(vec![PageValue::List(ids[1]), PageValue::List(ids[1])]),
                },
                VirtualEntity {
                    entity: ids[1],
                    body: VirtualBody::List(if cycle {
                        vec![PageValue::List(ids[1])]
                    } else {
                        vec![PageValue::Int(lash_kernel_doc::heap::IntRef::Inline(
                            7.into(),
                        ))]
                    }),
                },
                VirtualEntity {
                    entity: ids[2],
                    body: VirtualBody::List(vec![]),
                },
            ],
            outputs: vec![(owner(0), PublicationValue::Page(PageValue::List(ids[0])))],
            ..Default::default()
        };
        event.entities.swap(0, 1);
        let program = Program::new(vec![event]);
        program
            .clone()
            .evaluate(&oracle, &oracle_arena)
            .unwrap()
            .prepare(
                &oracle,
                oracle_arena.registration().unwrap(),
                &oracle_heap,
                program,
            )
            .unwrap()
            .admit(expected)
            .unwrap()
            .publish(&mut oracle, |_, _| {})
            .unwrap();
        assert_eq!(
            shared.borrow().balance().unwrap(),
            oracle.balance().unwrap()
        );
        for id in ids {
            let actual = shared.borrow().entity_holding(id, heap.pages()).unwrap();
            let expected = oracle.entity_holding(id, oracle_heap.pages()).unwrap();
            assert_eq!(
                actual.as_ref().map(Holding::bytes),
                expected.as_ref().map(Holding::bytes)
            );
            if let (Some(actual), Some(expected)) = (actual, expected) {
                let mut a = Vec::new();
                let mut b = Vec::new();
                actual
                    .visit_edges(heap.pages(), &mut |target, count| {
                        a.push((target, count));
                        Ok(())
                    })
                    .unwrap();
                expected
                    .visit_edges(oracle_heap.pages(), &mut |target, count| {
                        b.push((target, count));
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(a, b);
            }
        }
        // The first overwrite retains the second b edge, so its +1 byte
        // precedes b's last-owner release at the second overwrite. The old
        // final-edit bag concealed this 130-byte source prefix.
        for bound in [129, 130] {
            let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
            let a = lease.import(&av).unwrap();
            let object = lease.object(a).unwrap();
            for (at, value) in [(0, 1), (1, 2)] {
                let value = lease.import(&Value::Int(value)).unwrap();
                let (delta, peak, _) = accept(
                    &mut lease,
                    LeaseOp::Member {
                        object,
                        member: LeaseMember::List(at),
                        next: Some(value),
                    },
                );
                assert_eq!(delta, if !cycle && at == 1 { -48 } else { 1 });
                assert_eq!(peak, if !cycle && at == 1 { 0 } else { 1 });
            }
            let packet = exit(&mut lease, [], [], 130, if cycle { 2 } else { -47 });
            let result = lease.prepare(packet, bound);
            if bound == 129 {
                assert!(result.is_err());
                drop(result);
                assert_eq!(
                    shared.borrow().balance().unwrap().total().unwrap(),
                    expected
                );
            } else {
                result.unwrap().commit(|| {}).unwrap();
            }
        }
        assert!(
            arena
                .item(av.handle().unwrap(), 0)
                .unwrap()
                .unwrap()
                .same(&Value::Int(1))
        );
        assert!(
            arena
                .item(av.handle().unwrap(), 1)
                .unwrap()
                .unwrap()
                .same(&Value::Int(2))
        );
        assert_eq!(
            shared
                .borrow()
                .entity_holding(ids[1], heap.pages())
                .unwrap()
                .is_some(),
            cycle
        );
        assert_eq!(
            shared.borrow().balance().unwrap().total().unwrap(),
            if cycle { 130 } else { 82 }
        );
        let retained = shared.borrow().balance().unwrap().total().unwrap();
        let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
        let a = lease.import(&av).unwrap();
        accept(&mut lease, LeaseOp::Release { value: a, count: 1 });
        let packet = exit(&mut lease, [owner(0)], [], retained, -82);
        lease
            .prepare(packet, retained)
            .unwrap()
            .commit(|| {})
            .unwrap();
        assert_eq!(
            shared.borrow().balance().unwrap().total().unwrap(),
            if cycle { 48 } else { 0 }
        );
        shared
            .borrow_mut()
            .normalize(heap.pages(), TraceLimits::default())
            .unwrap();
        assert_eq!(shared.borrow().balance().unwrap(), Balance::default());
    }
}

// An imported tuple and a fresh tuple must use the same indexed read rules.
fn imported_tuple_reads() {
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let tuple = arena.tuple(vec![Value::Int(7)], 256).unwrap();
    let expected = arena.item(tuple.handle().unwrap(), 0).unwrap().unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let imported = lease.import(&tuple).unwrap();
    let seven = lease.import(&Value::Int(7)).unwrap();
    let local = allocate(&mut lease, LeaseAllocation::tuple(&[seven]));
    for tuple in [imported, local] {
        let object = lease.object(tuple).unwrap();
        let actual = lease.read(object, &LeaseMember::List(0)).unwrap();
        assert!(lease.scalar(actual).unwrap().same(&expected));
        let absent = lease.read(object, &LeaseMember::List(1)).unwrap();
        assert!(lease.scalar(absent).unwrap().same(&Value::Absent));
        assert!(matches!(
            lease.stage(LeaseOp::Member {
                object,
                member: LeaseMember::List(0),
                next: Some(seven),
            }),
            Err(LeaseFault::Native(lash_kernel_doc::NativeError::Decline))
        ));
    }
}

fn shared_arena_lease() {
    let (mut heap, _) = fresh();
    let arena = Rc::new(std::cell::RefCell::new(Arena::new()));
    let value = arena.borrow_mut().create_list(&[Value::Int(7)]);
    let handle = value.handle().unwrap();
    let ledger = heap.logical_ledger();
    ledger
        .borrow_mut()
        .transfer_source(
            &mut arena.borrow_mut(),
            &heap,
            &[],
            &[(owner(0), RootValue::Value(&value))],
        )
        .unwrap();
    let stamp = ledger.borrow().stamp();
    let lease = StateLease::acquire_shared(&mut heap, arena.as_ref()).unwrap();
    assert!(arena.try_borrow_mut().is_err());
    assert!(
        lease
            .arena()
            .item(handle, 0)
            .unwrap()
            .unwrap()
            .same(&Value::Int(7))
    );
    drop(lease);
    assert!(arena.try_borrow_mut().is_ok());
    assert!(ledger.borrow().is_current(&stamp));
    for bound in [64, 65] {
        let mut lease = StateLease::acquire_shared(&mut heap, arena.as_ref()).unwrap();
        lease.import(&value).unwrap();
        let packet = exit(
            &mut lease,
            [owner(0)],
            [(owner(1), PublicationValue::Existing(owner(0)))],
            65,
            0,
        );
        let result = lease.prepare(packet, bound);
        if bound == 64 {
            assert!(result.is_err());
            drop(result);
            assert!(
                ledger
                    .borrow()
                    .holding(owner(0), heap.pages())
                    .unwrap()
                    .is_some()
            );
            assert!(
                ledger
                    .borrow()
                    .holding(owner(1), heap.pages())
                    .unwrap()
                    .is_none()
            );
        } else {
            let vm = std::cell::Cell::new(7);
            result
                .unwrap()
                .commit(|| {
                    assert!(arena.try_borrow_mut().is_err());
                    assert!(ledger.try_borrow_mut().is_err());
                    vm.set(9);
                })
                .unwrap();
            assert_eq!(vm.get(), 9);
        }
    }
    assert!(
        arena
            .borrow()
            .item(handle, 0)
            .unwrap()
            .unwrap()
            .same(&Value::Int(7))
    );
    assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), 65);
    assert!(
        ledger
            .borrow()
            .holding(owner(0), heap.pages())
            .unwrap()
            .is_none()
    );
    assert!(
        ledger
            .borrow()
            .holding(owner(1), heap.pages())
            .unwrap()
            .is_some()
    );
}
fn table_region_exit() {
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let shared = heap.logical_ledger();
    let map = arena.create(NewObject::Map(vec![])).unwrap();
    let handle = map.handle().unwrap();
    shared
        .borrow_mut()
        .transfer_source(
            &mut arena,
            &heap,
            &[],
            &[(owner(0), RootValue::Value(&map))],
        )
        .unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let local = lease.import(&map).unwrap();
    let object = lease.object(local).unwrap();
    let alias = lease.import(&map).unwrap();
    let alias = lease.object(alias).unwrap();
    let one = lease.import(&Value::Int(1)).unwrap();
    let keys = ["x", "y", "z", "w"].map(|key| {
        lease
            .import(&Value::Text(
                lash_kernel_doc::arena::ShortText::new(key).unwrap(),
            ))
            .unwrap()
    });
    for key in &keys[..3] {
        accept(
            &mut lease,
            LeaseOp::Member {
                object,
                member: LeaseMember::Table(*key),
                next: Some(one),
            },
        );
        let observed = lease.read(alias, &LeaseMember::Table(*key)).unwrap();
        assert!(lease.scalar(observed).unwrap().same(&Value::Int(1)));
    }
    let packet = exit(&mut lease, [], [], 150, 102);
    lease.prepare(packet, 150).unwrap().commit(|| {}).unwrap();
    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), 150);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let value = lease.import(&map).unwrap();
    let object = lease.object(value).unwrap();
    let keys = ["x", "y", "w"].map(|key| {
        lease
            .import(&Value::Text(
                lash_kernel_doc::arena::ShortText::new(key).unwrap(),
            ))
            .unwrap()
    });
    let two = lease.import(&Value::Int(2)).unwrap();
    let one = lease.import(&Value::Int(1)).unwrap();
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Table(keys[1]),
            next: None,
        },
    );
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Table(keys[0]),
            next: Some(two),
        },
    );
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Table(keys[2]),
            next: Some(one),
        },
    );
    let packet = exit(&mut lease, [], [], 150, 0);
    lease.prepare(packet, 150).unwrap().commit(|| {}).unwrap();
    let mut after = None;
    let mut observed = Vec::new();
    while let Some((sequence, key)) = arena.table_after(handle, after).unwrap() {
        observed.push((
            sequence,
            key,
            arena.table_get(handle, &key).unwrap().unwrap(),
        ));
        after = Some(sequence);
    }
    assert_eq!(observed.len(), 3);
    for ((sequence, key, value), (expected_sequence, expected_key, expected_value)) in observed
        .into_iter()
        .zip([(0, "x", 2), (2, "z", 1), (3, "w", 1)])
    {
        assert_eq!(sequence, expected_sequence);
        assert!(key.same(&Value::Text(
            lash_kernel_doc::arena::ShortText::new(expected_key).unwrap()
        )));
        assert!(value.same(&Value::Int(expected_value)));
    }
    assert_eq!(shared.borrow().balance().unwrap().total().unwrap(), 150);
    // A temporary insertion still consumes its sequence and invalidates
    // iterators after its final member is deleted. Its clean head must be
    // projected at the next unchanged safe point.
    arena.clean(handle).unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let value = lease.import(&map).unwrap();
    let object = lease.object(value).unwrap();
    let key = lease
        .import(&Value::Text(
            lash_kernel_doc::arena::ShortText::new("temporary").unwrap(),
        ))
        .unwrap();
    let one = lease.import(&Value::Int(1)).unwrap();
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Table(key),
            next: Some(one),
        },
    );
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Table(key),
            next: None,
        },
    );
    assert_eq!(region_fixtures::storage_work(&lease).changed_members, 0);
    let packet = exit(&mut lease, [], [], 192, 0);
    lease.prepare(packet, 192).unwrap().commit(|| {}).unwrap();
    assert!(
        arena.facts(handle).unwrap().dirty,
        "a net-empty overlay must still publish its changed table head"
    );
    assert_eq!(arena.region_table_next(handle).unwrap(), 5);
}

fn call_and_measure_views() {
    use lash_kernel_doc::{Measure, ValueKind};
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let ledger = heap.logical_ledger();
    let link = lash_kernel_value::content_ids::LinkId::from_bytes([3; 32]);
    let identity = std::sync::Arc::new(lash_kernel_doc::arena::ClosureIdentity {
        capture_names: vec![lash_kernel_value::Name::new("captured")],
        callable: lash_kernel_value::names::CallableId {
            link,
            function: lash_kernel_value::names::FunctionId::from_bytes([7; 32]),
        },
        site: lash_kernel_doc::linked_site::LinkedSite {
            link,
            site: lash_kernel_value::sites::Site::new(lash_kernel_value::sites::Unit::Main, []),
        },
    });
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let seven = lease.import(&Value::Int(7)).unwrap();
    let cell = allocate(&mut lease, LeaseAllocation::cell(&seven));
    let object = lease.object(cell).unwrap();
    let captures = [cell];
    let closure = allocate(
        &mut lease,
        LeaseAllocation::closure(identity.clone(), &captures),
    );
    assert_eq!(lease.kind(closure).unwrap(), ValueKind::Closure);
    let capture = lease.callable(closure).unwrap().captures()[0];
    assert_eq!(lease.object(capture).unwrap(), object);
    assert_eq!(
        lease.callable(closure).unwrap().identity(),
        &identity.callable
    );
    assert_eq!(
        lease.callable(closure).unwrap().closure().unwrap().site,
        identity.site
    );
    accept(
        &mut lease,
        LeaseOp::Release {
            value: cell,
            count: 1,
        },
    );
    let eight = lease.import(&Value::Int(8)).unwrap();
    let version = lease.version(object).unwrap();
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Variable,
            next: Some(eight),
        },
    );
    assert_ne!(lease.version(object).unwrap(), version);
    let capture = lease.callable(closure).unwrap().captures()[0];
    let captured = lease.object(capture).unwrap();
    let value = lease.read(captured, &LeaseMember::Variable).unwrap();
    assert!(lease.scalar(value).unwrap().same(&Value::Int(8)));
    assert_eq!(lease.measure(closure, Measure::Size).unwrap(), 1);
    assert_eq!(lease.measure(closure, Measure::DeepSize).unwrap(), 1);
    assert_eq!(lease.measure(eight, Measure::Magnitude).unwrap(), 8);
    let capture_owner = Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Capture(0),
    };
    assert_eq!(
        accept(
            &mut lease,
            LeaseOp::HoldCapture {
                cell: object,
                count: 1
            }
        )
        .0,
        0
    );
    let graph = lease.materialize_graph().unwrap();
    let cv = graph.value(closure).unwrap();
    let cell_handle = graph.object(object).unwrap();
    let output = graph.publication(closure).unwrap();
    lease
        .prepare(
            region_fixtures::exit(
                graph,
                [],
                [
                    (owner(0), output),
                    (capture_owner, PublicationValue::Capture(cell_handle)),
                ],
                121,
                105,
            ),
            121,
        )
        .unwrap()
        .commit(|| {})
        .unwrap();
    assert!(arena.cell(cell_handle).unwrap().same(&Value::Int(8)));
    assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), 105);
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let closure = lease.import(&cv).unwrap();
    let capture = lease.callable(closure).unwrap().captures()[0];
    let object = lease.object(capture).unwrap();
    let nine = lease.import(&Value::Int(9)).unwrap();
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Variable,
            next: Some(nine),
        },
    );
    let capture = lease.callable(closure).unwrap().captures()[0];
    let object = lease.object(capture).unwrap();
    let value = lease.read(object, &LeaseMember::Variable).unwrap();
    assert!(lease.scalar(value).unwrap().same(&Value::Int(9)));
    assert!(
        lease
            .arena()
            .cell(cell_handle)
            .unwrap()
            .same(&Value::Int(8))
    );
    let packet = exit(&mut lease, [], [], 105, 0);
    lease.prepare(packet, 105).unwrap().commit(|| {}).unwrap();
    assert!(arena.cell(cell_handle).unwrap().same(&Value::Int(9)));
    assert_eq!(
        ledger
            .borrow()
            .holding(capture_owner, heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        0
    );
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let closure = lease.import(&cv).unwrap();
    let capture = lease.callable(closure).unwrap().captures()[0];
    let cell = lease.object(capture).unwrap();
    assert_eq!(
        accept(&mut lease, LeaseOp::ReleaseCapture { cell, count: 1 }).0,
        0
    );
    assert_eq!(
        accept(
            &mut lease,
            LeaseOp::Release {
                value: closure,
                count: 1
            }
        )
        .0,
        -105
    );
    let packet = exit(&mut lease, [owner(0), capture_owner], [], 105, -105);
    lease.prepare(packet, 105).unwrap().commit(|| {}).unwrap();
    assert_eq!(ledger.borrow().balance().unwrap(), Balance::default());
}
fn list_and_field_overlays() {
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let ledger = heap.logical_ledger();
    let list = arena.create_list(&[Value::Int(1), Value::Int(2), Value::Int(3)]);
    let handle = list.handle().unwrap();
    ledger
        .borrow_mut()
        .transfer_source(
            &mut arena,
            &heap,
            &[],
            &[(owner(0), RootValue::Value(&list))],
        )
        .unwrap();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let local = lease.import(&list).unwrap();
    let object = lease.object(local).unwrap();
    assert_eq!(
        accept(
            &mut lease,
            LeaseOp::Member {
                object,
                member: LeaseMember::List(0),
                next: None
            }
        )
        .0,
        -17
    );
    for (at, expected) in [(0, 2), (1, 3)] {
        let value = lease.read(object, &LeaseMember::List(at)).unwrap();
        assert!(lease.scalar(value).unwrap().same(&Value::Int(expected)));
    }
    let four = lease.import(&Value::Int(4)).unwrap();
    drop(
        lease
            .stage(LeaseOp::Member {
                object,
                member: LeaseMember::List(2),
                next: Some(four),
            })
            .unwrap()
            .reserve()
            .unwrap(),
    );
    let value = lease.read(object, &LeaseMember::List(2)).unwrap();
    assert!(matches!(lease.scalar(value).unwrap(), Value::Absent));
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::List(2),
            next: Some(four),
        },
    );
    let one = lease.import(&Value::Int(1)).unwrap();
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::List(0),
            next: Some(one),
        },
    );
    assert_eq!(
        lease
            .measure(local, lash_kernel_doc::Measure::DeepSize)
            .unwrap(),
        10
    );
    let packet = exit(&mut lease, [], [], 99, 0);
    lease.prepare(packet, 99).unwrap().commit(|| {}).unwrap();
    for (at, expected) in [(0, 1), (1, 3), (2, 4)] {
        assert!(
            arena
                .item(handle, at)
                .unwrap()
                .unwrap()
                .same(&Value::Int(expected))
        );
    }
    assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), 99);
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let ledger = heap.logical_ledger();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let record = allocate(&mut lease, LeaseAllocation::record(&[]));
    let object = lease.object(record).unwrap();
    let one = lease.import(&Value::Int(1)).unwrap();
    let field: std::sync::Arc<str> = std::sync::Arc::from("a_retained_field_longer_than_inline");
    accept(
        &mut lease,
        LeaseOp::Member {
            object,
            member: LeaseMember::Field(field.clone()),
            next: Some(one),
        },
    );
    let expected = 65 + field.len() as u64;
    let graph = lease.materialize_graph().unwrap();
    let value = graph.value(record).unwrap();
    let output = graph.publication(record).unwrap();
    lease
        .prepare(
            region_fixtures::exit(graph, [], [(owner(0), output)], expected, expected.into()),
            expected,
        )
        .unwrap()
        .commit(|| {})
        .unwrap();
    let key = arena.text(&field);
    assert!(
        arena
            .table_get(value.handle().unwrap(), &key)
            .unwrap()
            .unwrap()
            .same(&Value::Int(1))
    );
    assert_eq!(
        ledger.borrow().balance().unwrap().total().unwrap(),
        expected
    );
}

// ISA-D: last-owner propagation visits the affected component linearly, and
// importing that same source graph does not grow the Rust call stack.
fn linear_chain_release_and_binding() {
    use lash_kernel_doc::LogicalHeldV1 as Cost;
    const LENGTH: usize = 64;
    let retained = LENGTH as u64 * (Cost::OBJECT + Cost::REFERENCE);
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let mut root = allocate(&mut lease, LeaseAllocation::list(&[]));
    for _ in 1..LENGTH {
        let next = allocate(&mut lease, LeaseAllocation::list(&[root]));
        accept(
            &mut lease,
            LeaseOp::Release {
                value: root,
                count: 1,
            },
        );
        root = next;
    }
    // Dropping preparation must preserve the entire previously accepted graph.
    for accepted in [false, true] {
        let step = lease
            .stage(LeaseOp::Release {
                value: root,
                count: 1,
            })
            .unwrap();
        assert_eq!(step.facts().delta(), -i128::from(retained));
        assert_eq!(step.facts().peak(), 0);
        if accepted {
            step.reserve().unwrap().accept();
        } else {
            drop(step);
        }
        let work = region_fixtures::storage_work(&lease);
        assert!(
            work.release_visits <= 3 * LENGTH as u64,
            "last-owner propagation rescanned its component: {} visits for {LENGTH} bodies",
            work.release_visits
        );
        assert_eq!(work.live_objects, if accepted { 0 } else { LENGTH });
    }
    drop(lease);
    let ledger = heap.logical_ledger();
    let mut root = arena.create_list(&[]);
    for _ in 1..LENGTH {
        root = arena.create_list(&[root]);
    }
    ledger
        .borrow_mut()
        .transfer_source(
            &mut arena,
            &heap,
            &[],
            &[(owner(0), RootValue::Value(&root))],
        )
        .unwrap();
    assert_eq!(
        ledger.borrow().balance().unwrap().total().unwrap(),
        retained
    );
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let root = lease.import(&root).unwrap();
    let work = region_fixtures::storage_work(&lease);
    assert_eq!(
        work.binding_peak, 1,
        "entry binding recursively consumed the Rust stack"
    );
    assert_eq!(work.live_objects, LENGTH);
    let (delta, peak, _) = accept(
        &mut lease,
        LeaseOp::Release {
            value: root,
            count: 1,
        },
    );
    assert_eq!((delta, peak), (-i128::from(retained), 0));
    let packet = exit(&mut lease, [owner(0)], [], retained, delta);
    lease
        .prepare(packet, retained)
        .unwrap()
        .commit(|| {})
        .unwrap();
    assert_eq!(ledger.borrow().balance().unwrap(), Balance::default());
}

// ISA-D: one source transfer protects its new aliases before last-owner
// cascades. An abandoned phase leaves the accepted local graph unchanged.
fn atomic_source_root_transfer() {
    use lash_kernel_doc::LogicalHeldV1 as Cost;
    let (mut heap, _) = fresh();
    let mut arena = Arena::new();
    let mut lease = StateLease::acquire(&mut heap, &mut arena).unwrap();
    let leaf = allocate(&mut lease, LeaseAllocation::list(&[]));
    let parent = allocate(&mut lease, LeaseAllocation::list(&[leaf]));
    accept(
        &mut lease,
        LeaseOp::Release {
            value: leaf,
            count: 1,
        },
    );
    accept(
        &mut lease,
        LeaseOp::Hold {
            value: parent,
            count: 2,
        },
    );
    let parent_object = lease.object(parent).unwrap();
    let leaf_object = lease.object(leaf).unwrap();
    let holds = [(leaf, false)];
    let releases = [(parent, false); 3];
    let expected = -i128::from(Cost::OBJECT + 3 * Cost::REFERENCE);
    for accepted in [false, true] {
        let step = lease
            .stage(LeaseOp::Transfer {
                holds: &holds,
                releases: &releases,
            })
            .unwrap();
        assert_eq!(step.facts().delta(), expected);
        assert_eq!(step.facts().peak(), 0);
        assert_eq!(
            step.facts().measure(&LeaseMeasure::RELEASED),
            Some(Cost::OBJECT + Cost::REFERENCE)
        );
        let ready = step.reserve().unwrap();
        if accepted {
            ready.accept();
        } else {
            drop(ready);
        }
        assert_eq!(
            region_fixtures::storage_work(&lease).live_objects,
            if accepted { 1 } else { 2 }
        );
        assert_eq!(lease.object(leaf).unwrap(), leaf_object);
        if !accepted {
            assert_eq!(lease.object(parent).unwrap(), parent_object);
            let child = lease.read(parent_object, &LeaseMember::List(0)).unwrap();
            assert_eq!(lease.object(child).unwrap(), leaf_object);
        } else {
            assert!(lease.object(parent).is_err());
        }
    }
    let step = lease
        .stage(LeaseOp::Transfer {
            holds: &holds,
            releases: &holds,
        })
        .unwrap();
    assert_eq!(step.facts().delta(), 0);
    assert_eq!(step.facts().measure(&LeaseMeasure::RELEASED), Some(0));
    step.reserve().unwrap().accept();
    assert_eq!(lease.object(leaf).unwrap(), leaf_object);
}
