use super::*;

/// K-EVAL-008: entry observes precisely its arguments and keeps them owned.
pub(super) fn entry() {
    use crate::admission::{Allowance, SourceResources};
    use crate::compile::room::{self, ExitKind, Phase, Point, publication::*};
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{Owner, PublicationValue, RootValue, Slot};
    use lash_kernel_value::sites::{Site, Unit};
    use std::{collections::BTreeMap, sync::Arc};

    struct Facts;
    impl room::Facts for Facts {
        fn dependencies(&self) -> Option<&[doc::arena::ArenaHandle]> {
            Some(&[])
        }
        fn measure(&self, _: &room::Measure) -> Option<i128> {
            None
        }
        fn call(&self, _: &Site, _: room::CallSource) -> Option<&room::CheckedRoom> {
            None
        }
        fn raised(&self) -> bool {
            false
        }
        fn resolved(&self, _: &Site) -> Option<lash_kernel_value::FunctionId> {
            None
        }
    }
    struct Execution;
    impl room::Execution for Execution {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(Facts))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }

    let run = Embedder::new("main { let f = fn(x) { return x } return apply f(1) }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &BTreeMap::new(),
        &Default::default(),
    ));
    let at = Point {
        site: source.site.child(1).child(0),
        phase: Phase::Enter,
    };
    let interval = source.compile_event(&at, ExitKind::Action).unwrap();
    let descriptor = ControlExit::compile(&source, Arc::clone(&interval)).unwrap();
    let event = interval.events().next().unwrap();
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut ledger = store.borrow_mut();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let (invocation, entered) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(entered.admit(4096).ok().unwrap()).unwrap();
    let slots = source.owner_slots().unwrap();
    let owners: Vec<_> = event
        .inputs
        .iter()
        .map(|selector| slots.root(selector, TaskId::MAIN, invocation.id()).unwrap())
        .collect();
    assert!(!owners.is_empty());
    let foreign = invocation.owner(Slot::Argument(123));
    let value = Value::Int(1);
    let values: Vec<_> = owners
        .iter()
        .copied()
        .chain([foreign])
        .map(|owner| (owner, RootValue::Value(&value)))
        .collect();
    let holding = ledger
        .prepare_ownership(&mut arena, heap.pages(), &[], &values)
        .unwrap();
    ledger.commit(holding.admit(4096).ok().unwrap()).unwrap();
    let before = ledger.balance().unwrap();
    let position = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        recovery: [4; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: Some((at.clone(), 0)),
        },
    };
    drop(ledger);
    let ledger = store.borrow();
    let mut frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            doc::KernelVersion::One,
            position.clone(),
        )
        .unwrap();
    let next = Position {
        unpaid: Some(Point {
            phase: Phase::Commit,
            ..at.clone()
        }),
        ..position
    };
    let allowance = Allowance {
        occurrences: vec![0],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 0,
    };
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(1);
    let mut variants = vec![(
        ControlRoots {
            borrowed: owners.clone(),
            ..Default::default()
        },
        true,
    )];
    let mut wrong = owners.clone();
    wrong[0] = foreign;
    variants.push((
        ControlRoots {
            borrowed: wrong,
            ..Default::default()
        },
        false,
    ));
    let mut reordered = owners.clone();
    reordered.reverse();
    assert_ne!(reordered, owners);
    variants.push((
        ControlRoots {
            borrowed: reordered,
            ..Default::default()
        },
        false,
    ));
    variants.push((
        ControlRoots {
            inputs: owners.clone(),
            ..Default::default()
        },
        false,
    ));
    variants.push((ControlRoots::default(), false));
    let mut duplicate = owners.clone();
    duplicate.push(owners[0]);
    variants.push((
        ControlRoots {
            borrowed: duplicate,
            ..Default::default()
        },
        false,
    ));
    variants.push((
        ControlRoots {
            borrowed: owners.clone(),
            outputs: vec![(foreign, PublicationValue::Native(value))],
            ..Default::default()
        },
        false,
    ));
    for (event, valid) in variants {
        let resources = SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 0,
        };
        let prepared = descriptor
            .prepare(
                &source,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                &mut Execution,
                event,
                next.clone(),
                allowance.clone(),
                resources.facts(&allowance).unwrap(),
            )
            .unwrap();
        assert_eq!(
            prepared.is_some(),
            valid,
            "only the exact read-only entry arguments are accepted"
        );
    }
    let make = || ControlRoots {
        borrowed: owners.clone(),
        ..Default::default()
    };
    let resources = SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: 10,
        slice: 10,
        depth: 0,
        limit: 10,
        safe_point: 0,
    };
    let prepared = descriptor
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut Execution,
            make(),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    let accepted = prepared
        .admit(&frontier, &ledger, resources, 4096)
        .unwrap()
        .unwrap();
    drop(accepted);
    assert_eq!(charged, 0);
    assert_eq!(
        counters.current(0),
        Some(0),
        "an abandoned Enter consumes no occurrence"
    );
    assert_eq!(ledger.balance().unwrap(), before);
    assert_eq!(frontier.position().unpaid.as_ref(), Some(&at));
    let resources = SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: 10,
        slice: 10,
        depth: 0,
        limit: 10,
        safe_point: 0,
    };
    let prepared = descriptor
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut Execution,
            make(),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    let accepted = prepared
        .admit(&frontier, &ledger, resources, 4096)
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    accepted
        .publish_control(&mut frontier, &mut ledger, |_| {})
        .ok()
        .unwrap();
    assert_eq!(ledger.balance().unwrap(), before);
    for owner in owners.iter().copied().chain([foreign]) {
        assert!(ledger.holding(owner, heap.pages()).unwrap().is_some());
    }
    assert_eq!(charged, 0);
    assert_eq!(counters.current(0), Some(1));
    drop(ledger);
    let ledger = store.borrow();
    // The same borrowed owner list cannot outlive the ledger revision at which
    // its exact contents were observed.
    let resources = SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: 10,
        slice: 10,
        depth: 0,
        limit: 10,
        safe_point: 0,
    };
    let prepared = descriptor
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut Execution,
            make(),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap();
    assert!(
        prepared.is_none(),
        "the old Enter cannot run past its published unpaid head"
    );
    drop(prepared);
    let mut stale_position = frontier.position().clone();
    stale_position.unpaid = Some(at.clone());
    stale_position.occurrence.action = Some((at.clone(), 1));
    let stale = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            doc::KernelVersion::One,
            stale_position,
        )
        .unwrap();
    let next = Position {
        occurrence: stale.position().occurrence.clone(),
        ..next
    };
    let prepared = descriptor
        .prepare(
            &source,
            &stale,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            &mut Execution,
            make(),
            next,
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    ledger.handoff().unwrap();
    assert!(
        prepared
            .admit(&stale, &ledger, resources, 4096)
            .unwrap()
            .is_none()
    );
    let absent = Owner::Incoming(TaskId::MAIN);
    assert!(ledger.holding(absent, heap.pages()).unwrap().is_none());
    let resources = SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: 10,
        slice: 10,
        depth: 0,
        limit: 10,
        safe_point: 0,
    };
    assert!(
        descriptor
            .prepare(
                &source,
                &stale,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                &mut Execution,
                ControlRoots {
                    borrowed: vec![absent],
                    ..Default::default()
                },
                Position {
                    unpaid: Some(Point {
                        phase: Phase::Commit,
                        ..at
                    }),
                    ..stale.position().clone()
                },
                allowance.clone(),
                resources.facts(&allowance).unwrap(),
            )
            .unwrap()
            .is_none()
    );
}
