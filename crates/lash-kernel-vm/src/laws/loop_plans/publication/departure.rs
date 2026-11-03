//! K-MACH-008: a compiled departure binds its actual root and terminal frontier.
use super::*;
use crate::compile::room::{self, Phase, Point, publication::*};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical::{Owner, RootValue};
use lash_kernel_value::sites::{Site, Unit};
use std::sync::Arc;
mod literal;

pub(super) fn exits() {
    literal::returns();
    let run = Embedder::new("main { let n = 1 return n }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    ));
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let mut ledger = store.borrow_mut();
    let (invocation, enter) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(enter.admit(u64::MAX).ok().unwrap()).unwrap();
    let at = Point {
        site: source.site.child(1),
        phase: Phase::Commit,
    };
    let slots = source.owner_slots().unwrap();
    let root = |site| {
        slots
            .root(&room::Owner::Operand(site), TaskId::MAIN, invocation.id())
            .unwrap()
    };
    let input = root(at.site.child(0));
    let wrong = root(source.site.child(0).child(0));
    let roots = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[],
            &[
                (input, RootValue::Value(&Value::Int(1))),
                (wrong, RootValue::Value(&Value::Int(2))),
            ],
        )
        .unwrap();
    ledger.commit(roots.admit(u64::MAX).ok().unwrap()).unwrap();
    drop(ledger);
    let ledger = store.borrow();
    let exit = source.departure_exit(&at).unwrap();
    assert!(Arc::ptr_eq(&exit, &source.departure_exit(&at).unwrap()));
    let start = Position {
        at: at.clone(),
        unpaid: Some(at),
        recovery: [7; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    let next = exit.next(&source, &start, None).unwrap();
    assert!(next.unpaid.is_none());
    let mut frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            start.clone(),
        )
        .unwrap();
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 7,
    };
    let mut charged = 7;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    fn resources<'a>(
        charged: &'a mut u64,
        counters: &'a mut crate::machine::tasks::Occurrences,
    ) -> crate::admission::SourceResources<'a> {
        crate::admission::SourceResources {
            charged,
            counters,
            bound: 20,
            slice: 20,
            depth: 1,
            limit: 10,
            safe_point: 7,
        }
    }
    let balance = ledger.balance().unwrap().total().unwrap();
    let mut rejected = Vec::new();
    for (input, next, allowance) in [
        (wrong, next.clone(), allowance.clone()),
        (input, start.clone(), allowance.clone()),
        (
            input,
            next.clone(),
            crate::admission::Allowance {
                units: 1,
                ..allowance.clone()
            },
        ),
    ] {
        let facts = resources(&mut charged, &mut counters)
            .facts(&allowance)
            .unwrap();
        rejected.push(
            exit.prepare(
                &source,
                &frontier,
                &invocation,
                &ledger,
                arena.registration().unwrap(),
                heap,
                Some(input),
                &[],
                next,
                allowance,
                None,
                facts,
            )
            .unwrap()
            .is_none(),
        );
        assert_eq!(charged, 7);
        assert_eq!(ledger.balance().unwrap().total().unwrap(), balance);
        assert_eq!(frontier.position(), &start);
    }
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let prepared = exit
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            Some(input),
            &[],
            next.clone(),
            allowance.clone(),
            None,
            facts,
        )
        .unwrap()
        .unwrap();
    assert!(
        prepared.peak() <= balance,
        "a pure return adds no logical ownership peak"
    );
    rejected.push(
        prepared
            .admit(
                &frontier,
                &ledger,
                resources(&mut charged, &mut counters),
                balance - 1,
            )
            .unwrap()
            .is_none(),
    );
    assert_eq!(
        rejected, [true; 4],
        "wrong operand, successor, fee and insufficient peak must each decline"
    );
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let prepared = exit
        .prepare(
            &source,
            &frontier,
            &invocation,
            &ledger,
            arena.registration().unwrap(),
            heap,
            Some(input),
            &[],
            next.clone(),
            allowance,
            None,
            facts,
        )
        .unwrap()
        .unwrap();
    let accepted = prepared
        .admit(
            &frontier,
            &ledger,
            resources(&mut charged, &mut counters),
            balance,
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    let mut applied = false;
    accepted
        .publish_departure(&mut frontier, &mut ledger, |_| applied = true)
        .unwrap();
    assert!(applied);
    assert_eq!(frontier.position(), &next);
    assert!(
        ledger
            .frame_owners(TaskId::MAIN, invocation.id(), heap.pages())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ledger
            .holding(Owner::Incoming(TaskId::MAIN), heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    assert_eq!(charged, 7);
}
