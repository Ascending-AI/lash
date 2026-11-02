use super::*;

// A native retains its input until its fresh body has been constructed. The
// sufficient envelope also keeps the output occurrence live throughout, but
// exact admission must use the real body events and atomic output transfer.
pub(super) fn check() {
    boundary_construction();
    use lash_kernel_doc::heap::PageValue;
    use lash_kernel_doc::{
        NativeResourceContract as Contract, NativeResourceEntity as Kind,
        NativeResourceEvent as Event, NativeResourceExit as Exit,
        NativeResourceHolding as HoldingSpec, NativeResourceOrigin as Origin,
        NativeResourceOwner as NativeOwner, Value,
    };
    let (heap, _) = fresh();
    let access = heap.pages();
    let mut ledger = Ledger::fresh();
    let mut arena = crate::arena::Arena::new();
    let input = Value::Int(7);
    let initial = ledger
        .prepare_publication(
            arena.registration().unwrap(),
            access,
            &[],
            &[(owner(0), RootValue::Value(&input))],
        )
        .unwrap();
    initial
        .admit(17)
        .unwrap_or_else(|_| panic!("the initial computed scalar fits"))
        .publish(&mut ledger, |_, _| {})
        .unwrap();
    // A journal names the body it creates by the next provisional key.
    let entity = arena.next_provisional();
    let contract = Contract {
        exit: Exit::Returned,
        events: vec![Event::Entity {
            owner: NativeOwner(0),
            kind: Kind::List,
            release: None,
            members: vec![
                HoldingSpec {
                    bytes: 17,
                    origin: Origin::Immutable
                };
                2
            ],
        }],
        output: HoldingSpec {
            bytes: 16,
            origin: Origin::Fresh(NativeOwner(0)),
        },
    };
    assert_eq!(contract.peak_extra_bytes(), Some(82));
    let make = || {
        Program::new(vec![JournalEvent {
            native: Some(contract.clone()),
            inputs: vec![owner(0)],
            outputs: vec![(owner(1), PublicationValue::Page(PageValue::List(entity)))],
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::List(vec![PageValue::Int(1.into()), PageValue::Int(2.into())]),
            }],
            ..Default::default()
        }])
    };
    let prepared = make()
        .evaluate(&ledger, &arena)
        .unwrap()
        .prepare(&ledger, arena.registration().unwrap(), &heap, make())
        .unwrap();
    assert_eq!(
        prepared.peak(),
        83,
        "the 17-byte input remains live while the 66-byte native entity is built"
    );
    assert!(
        prepared.admit(82).is_none(),
        "final balance alone cannot admit a native construction"
    );
    assert_eq!(
        arena.next_provisional(),
        entity,
        "declined exact admission returns the entity grant"
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 17);
    let prepared = make()
        .evaluate(&ledger, &arena)
        .unwrap()
        .prepare(&ledger, arena.registration().unwrap(), &heap, make())
        .unwrap();
    assert!(
        prepared.admit(83).is_some(),
        "the sufficient 99-byte envelope is not the exact native refusal threshold"
    );
}

/// Source allocation order includes retired interiors; a rootless cycle remains
/// charged until normalization. Preparing, refusing and rejecting a stale
/// boundary must not consume an identity or invoke its storage callback.
fn boundary_construction() {
    use lash_kernel_doc::heap::{FamilyId, PageValue};
    let (heap, _) = fresh();
    let mut arena = crate::arena::Arena::new();
    let mut ledger = Ledger::fresh();
    let ids: Vec<_> = (0..3)
        .map(|ordinal| EntityId {
            family: FamilyId::PROVISIONAL,
            ordinal,
        })
        .collect();
    let entities = vec![
        VirtualEntity {
            entity: ids[0],
            body: VirtualBody::List(vec![]),
        },
        VirtualEntity {
            entity: ids[1],
            body: VirtualBody::List(vec![PageValue::List(ids[1])]),
        },
        VirtualEntity {
            entity: ids[2],
            body: VirtualBody::List(vec![PageValue::Int(7.into())]),
        },
    ];
    let value = PageValue::List(ids[2]);
    let outputs = [(owner(5), RootValue::Page(&value))];
    let before = (arena.held(), arena.allocations(), arena.next_provisional());
    for swapped in [true, false] {
        let mut bodies = entities.clone();
        if swapped {
            bodies.swap(0, 2);
        }
        {
            let prepared = ledger.prepare_construction_boundary(
                arena.registration().unwrap(),
                &heap,
                &[],
                &outputs,
                &bodies,
            );
            if swapped {
                assert!(
                    prepared.is_err(),
                    "binding traversal cannot choose allocation ordinals"
                );
            } else {
                let (prepared, members) = prepared.unwrap();
                assert_eq!(members, [0, 16, 17]);
                assert_eq!(prepared.balance().unwrap().total().unwrap(), 113);
                assert!(
                    prepared.admit(112).is_err(),
                    "a rootless cycle is still source retained"
                );
            }
        }
        assert_eq!(
            (arena.held(), arena.allocations(), arena.next_provisional()),
            before
        );
        assert_eq!(ledger.balance().unwrap().total().unwrap(), 0);
    }
    let called = std::cell::Cell::new(false);
    let (prepared, _) = ledger
        .prepare_construction_boundary(
            arena.registration().unwrap(),
            &heap,
            &[],
            &outputs,
            &entities,
        )
        .unwrap();
    let accepted = prepared
        .admit(113)
        .unwrap_or_else(|_| panic!("exact construction fits"));
    ledger.handoff().unwrap();
    assert!(
        accepted
            .publish(&mut ledger, |_, _| called.set(true))
            .is_err()
    );
    assert!(!called.get(), "stale acceptance cannot install storage");
    assert_eq!(
        (arena.held(), arena.allocations(), arena.next_provisional()),
        before
    );
    let (prepared, _) = ledger
        .prepare_construction_boundary(
            arena.registration().unwrap(),
            &heap,
            &[],
            &outputs,
            &entities,
        )
        .unwrap();
    prepared
        .admit(113)
        .unwrap_or_else(|_| panic!("exact construction fits"))
        .publish(&mut ledger, |_, edits| {
            assert!(edits.is_empty());
            called.set(true);
        })
        .unwrap();
    assert!(called.get());
    assert_eq!(
        arena.next_provisional().ordinal,
        3,
        "the retired allocation consumed its original ordinal"
    );
    assert!(
        ledger
            .entity_holding(ids[0], heap.pages())
            .unwrap()
            .is_none()
    );
    assert!(
        ledger
            .entity_holding(ids[1], heap.pages())
            .unwrap()
            .is_some()
    );
    assert!(
        ledger
            .entity_holding(ids[2], heap.pages())
            .unwrap()
            .is_some()
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 113);
    ledger
        .normalize(
            heap.pages(),
            crate::paged::census_impl::TraceLimits::default(),
        )
        .unwrap();
    assert_eq!(
        ledger.balance().unwrap().total().unwrap(),
        65,
        "canonical normalization releases the rootless cycle"
    );
}
