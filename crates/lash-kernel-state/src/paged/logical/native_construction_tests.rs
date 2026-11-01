use super::*;

// A native retains its input until its fresh body has been constructed. The
// sufficient envelope also keeps the output occurrence live throughout, but
// exact admission must use the real body events and atomic output transfer.
pub(super) fn check() {
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
