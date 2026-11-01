use super::*;

/// K-EVAL-008: entry observes precisely its arguments and keeps them owned.
pub(super) fn entry() {
    use crate::admission::{Allowance, SourceResources};
    use crate::compile::room::{self, ExitKind, Phase, Point, publication::*};
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{
        JournalEvent, Owner, Program, PublicationValue, RootValue, Slot,
    };
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
        JournalEvent {
            borrowed: owners.clone(),
            ..Default::default()
        },
        true,
    )];
    let mut wrong = owners.clone();
    wrong[0] = foreign;
    variants.push((
        JournalEvent {
            borrowed: wrong,
            ..Default::default()
        },
        false,
    ));
    variants.push((
        JournalEvent {
            inputs: owners.clone(),
            ..Default::default()
        },
        false,
    ));
    variants.push((JournalEvent::default(), false));
    let mut duplicate = owners.clone();
    duplicate.push(owners[0]);
    variants.push((
        JournalEvent {
            borrowed: duplicate,
            ..Default::default()
        },
        false,
    ));
    variants.push((
        JournalEvent {
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
        let evaluated = frontier.evaluate_interval(
            Arc::clone(&interval),
            &mut Execution,
            &ledger,
            &arena,
            heap,
            Program::new(vec![event]),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        );
        assert_eq!(
            evaluated.is_ok(),
            valid,
            "only the exact read-only entry arguments are accepted"
        );
    }
    let make = || {
        Program::new(vec![JournalEvent {
            borrowed: owners.clone(),
            ..Default::default()
        }])
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
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Execution,
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
        )
        .ok()
        .unwrap();
    let prepared = evaluated
        .prepare_exact(
            &frontier,
            &ledger,
            arena.registration().unwrap(),
            heap,
            make(),
            ExitKind::Action,
            &next,
        )
        .ok()
        .unwrap();
    let accepted = prepared
        .admit_exact(&frontier, &ledger, resources, &allowance, 4096)
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    accepted
        .publish(&mut frontier, &mut ledger, |_, edits| {
            assert!(edits.is_empty())
        })
        .ok()
        .unwrap();
    assert_eq!(ledger.balance().unwrap(), before);
    for owner in owners.iter().copied().chain([foreign]) {
        assert!(ledger.holding(owner, heap.pages()).unwrap().is_some());
    }
    assert_eq!(charged, 0);
    assert_eq!(counters.current(0), Some(1));
    // The same borrowed owner list cannot outlive the ledger revision at which
    // its exact contents were observed.
    let evaluated = make().evaluate(&ledger, &arena).unwrap();
    ledger.handoff().unwrap();
    assert!(!evaluated.is_current(&ledger, &arena));
    assert!(
        evaluated
            .prepare(&ledger, arena.registration().unwrap(), heap, make())
            .is_err()
    );
    let absent = Owner::Incoming(TaskId::MAIN);
    assert!(ledger.holding(absent, heap.pages()).unwrap().is_none());
    let missing = || {
        Program::new(vec![JournalEvent {
            borrowed: vec![absent],
            ..Default::default()
        }])
    };
    assert!(
        missing()
            .evaluate(&ledger, &arena)
            .unwrap()
            .prepare(&ledger, arena.registration().unwrap(), heap, missing())
            .is_err()
    );
}
