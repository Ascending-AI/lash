use super::*;

/// K-EVAL-008: a resumed call restores its existing incoming value and saved
/// source operands, rather than consuming arguments transferred to the callee.
pub(super) fn calls() {
    use crate::admission::{Allowance, SourceResources};
    use crate::compile::room::{self, ExitKind, Phase, Point, publication::*};
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{
        JournalEvent, Owner as Root, Program, PublicationValue as PV, RootValue, Slot,
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
    let run = Embedder::new("main { let f = fn(x) { return x } return (7, apply f(1)) }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &BTreeMap::new(),
        &Default::default(),
    ));
    let at = Point {
        site: source.site.child(1).child(0).child(1),
        phase: Phase::Restore,
    };
    let interval = source
        .compile_event(&at, ExitKind::Normal)
        .expect("an entered call has a canonical source resumption");
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut ledger = store.borrow_mut();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let (invocation, entered) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(entered.admit(4096).ok().unwrap()).unwrap();
    let slots = source.owner_slots().unwrap();
    let saved_site = source.site.child(1).child(0).child(0);
    let saved = slots
        .continuation(&saved_site, TaskId::MAIN, invocation.id())
        .unwrap();
    let operand = slots
        .root(
            &room::Owner::Operand(saved_site),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    let result = slots
        .root(
            &room::Owner::Operand(at.site.clone()),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    let incoming = Root::Incoming(TaskId::MAIN);
    let foreign = invocation.owner(Slot::Argument(999));
    let value = Value::Int(1);
    let preserved = Value::Int(7);
    let entered = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[],
            &[
                (incoming, RootValue::Value(&value)),
                (saved, RootValue::Value(&preserved)),
                (foreign, RootValue::Value(&preserved)),
            ],
        )
        .unwrap();
    ledger.commit(entered.admit(4096).ok().unwrap()).unwrap();
    let before = ledger.balance().unwrap();
    let position = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        recovery: [5; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: Some((
                Point {
                    phase: Phase::Enter,
                    ..at.clone()
                },
                3,
            )),
            departure: None,
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
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..position
    };
    let allowance = Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 0,
    };
    let make = |saved_input, result_input, recreate| {
        Program::new(vec![JournalEvent {
            inputs: vec![incoming, saved_input],
            outputs: vec![
                (
                    result,
                    if recreate {
                        PV::Native(value)
                    } else {
                        PV::Existing(result_input)
                    },
                ),
                (operand, PV::Existing(saved_input)),
            ],
            ..Default::default()
        }])
    };
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    for (saved_input, result_input, recreate, valid) in [
        (saved, incoming, false, true),
        (foreign, incoming, false, false),
        (saved, saved, false, false),
        (saved, incoming, true, false),
    ] {
        let resources = SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 0,
        };
        assert_eq!(
            frontier
                .evaluate_interval(
                    Arc::clone(&interval),
                    &mut Execution,
                    &ledger,
                    &arena,
                    heap,
                    make(saved_input, result_input, recreate),
                    next.clone(),
                    allowance.clone(),
                    resources.facts(&allowance).unwrap()
                )
                .is_ok(),
            valid,
            "only the actual incoming and the exact continuation owners may be restored"
        );
    }
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
            make(saved, incoming, false),
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
            make(saved, incoming, false),
            ExitKind::Normal,
            &next,
        )
        .ok()
        .unwrap();
    let accepted = prepared
        .admit_exact(
            &frontier,
            &ledger,
            resources,
            &allowance,
            before.total().unwrap(),
        )
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
    for owner in [incoming, saved] {
        assert!(ledger.holding(owner, heap.pages()).unwrap().is_none());
    }
    for owner in [result, operand, foreign] {
        assert!(ledger.holding(owner, heap.pages()).unwrap().is_some());
    }
    assert_eq!(charged, 0);
}
