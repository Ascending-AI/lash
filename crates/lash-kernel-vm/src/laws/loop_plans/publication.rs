use super::*;

/// The source publication protocol binds recovery, ownership, charge closes
/// and an exclusive shared admission; equal byte deltas cannot replace them.
pub(super) fn checked_source_publication() {
    use crate::compile::room::{self, Phase, Point, Selection, publication::*};
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{
        self, FrameEvent, JournalEvent, Program, PublicationValue as PV,
    };
    use lash_kernel_value::sites::{Site, Unit};
    use std::sync::Arc;
    let run = Embedder::new("main { return null }");
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
    struct NullFacts(Vec<lash_kernel_doc::arena::ArenaHandle>);
    impl room::Facts for NullFacts {
        fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
            Some(&self.0)
        }

        fn measure(&self, measure: &room::Measure) -> Option<i128> {
            match measure {
                room::Measure::Local(_) => Some(16),
                room::Measure::Release(_) => Some(0),
                _ => None,
            }
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
    #[derive(Default)]
    struct Trace {
        route: Vec<Selection>,
        dependencies: Vec<lash_kernel_doc::arena::ArenaHandle>,
    }
    impl room::Execution for Trace {
        fn event(&mut self, point: &Point) -> Option<Box<dyn room::Facts + '_>> {
            self.route.push(Selection::Event {
                at: point.clone(),
                exit: if point.phase == Phase::Commit && point.site.path == [0] {
                    room::ExitKind::Return
                } else {
                    room::ExitKind::Normal
                },
            });
            Some(Box::new(NullFacts(self.dependencies.clone())))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    let mut trace = Trace::default();
    source.evaluate(&mut trace, 30).unwrap();
    let interval = source
        .compile_interval(&source.site, &trace.route, 30)
        .unwrap();
    let heap = run.machine.source_law_heap();
    let ledger_store = heap.logical_ledger();
    let ledger = ledger_store.borrow();
    let before = ledger.balance().unwrap().total().unwrap();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let invocation = ledger.prospective_invocation(TaskId::MAIN);
    let slots = source.owner_slots().unwrap();
    let owner = |selector| {
        slots
            .root(&selector, TaskId::MAIN, invocation.id())
            .unwrap()
    };
    let operand = owner(room::Owner::Operand(source.site.child(0).child(0)));
    let departure = owner(room::Owner::Departure(source.site.child(0)));
    let make = || {
        Program::new(vec![
            JournalEvent {
                frame: Some(FrameEvent::Enter {
                    task: TaskId::MAIN,
                    invocation: invocation.id(),
                }),
                ..Default::default()
            },
            JournalEvent {
                outputs: vec![(operand, PV::Native(Value::Null))],
                ..Default::default()
            },
            JournalEvent {
                inputs: vec![operand],
                outputs: vec![(departure, PV::Existing(operand))],
                ..Default::default()
            },
            JournalEvent {
                frame: Some(FrameEvent::Leave {
                    task: TaskId::MAIN,
                    invocation: invocation.id(),
                }),
                outputs: vec![(
                    logical::Owner::Incoming(TaskId::MAIN),
                    PV::Existing(departure),
                )],
                ..Default::default()
            },
        ])
    };
    let start = Position {
        at: interval.events().next().unwrap().point.clone(),
        unpaid: interval.events().next().unwrap().point.clone(),
        recovery: [1; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
            departure: None,
        },
    };
    let next = Position {
        at: interval.events().last().unwrap().point.clone(),
        recovery: [2; 32],
        ..start.clone()
    };
    let mut frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            start,
        )
        .unwrap();
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 1,
        close: None,
        strict: true,
        depth: 1,
        safe_point: 7,
    };
    fn resources<'a>(
        charged: &'a mut u64,
        counters: &'a mut crate::machine::tasks::Occurrences,
    ) -> crate::admission::SourceResources<'a> {
        crate::admission::SourceResources {
            charged,
            counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 7,
        }
    }
    let dependent = arena.create_list(&[Value::Int(7)]).handle().unwrap();
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Trace {
                dependencies: vec![dependent],
                ..Default::default()
            },
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap();
    arena.list_set(dependent, 0, Value::Int(9)).unwrap();
    assert!(
        evaluated
            .admit_interval(
                &frontier,
                &ledger,
                &arena,
                resources(&mut charged, &mut counters),
                &allowance,
                1000,
            )
            .unwrap()
            .is_none(),
        "an equal-size callee/cell dependency write invalidates the source capability"
    );
    assert_eq!(charged, 0);
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Trace::default(),
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap();
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            1000,
        )
        .unwrap()
        .unwrap();
    let mut wrong = next.clone();
    wrong.recovery = [9; 32];
    assert!(
        accepted
            .prepare_publication(
                &frontier,
                &ledger,
                arena.registration().unwrap(),
                heap,
                make(),
                interval.exit(),
                &wrong
            )
            .is_err()
    );
    assert_eq!(charged, 0);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Trace::default(),
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap();
    let mut wrong_close = allowance.clone();
    wrong_close.close = Some(crate::compile::expr::Close {
        region: 0,
        total: 1,
        head: 0,
    });
    assert!(
        evaluated
            .admit_interval(
                &frontier,
                &ledger,
                &arena,
                resources(&mut charged, &mut counters),
                &wrong_close,
                1000
            )
            .unwrap()
            .is_none(),
        "a different source charge close is not accepted"
    );
    assert_eq!(charged, 0);
    let oversized = || {
        let mut events = make().events().to_vec();
        events[1].outputs[0].1 = PV::Native(Value::Text(
            lash_kernel_doc::arena::ShortText::new("extra").unwrap(),
        ));
        Program::new(events)
    };
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Trace::default(),
            &ledger,
            &arena,
            heap,
            oversized(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap();
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            1000,
        )
        .unwrap()
        .unwrap();
    assert!(
        accepted
            .prepare_publication(
                &frontier,
                &ledger,
                arena.registration().unwrap(),
                heap,
                oversized(),
                interval.exit(),
                &next
            )
            .is_err(),
        "an understated source obligation cannot publish"
    );
    assert_eq!(charged, 0);
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Trace::default(),
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap();
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            1000,
        )
        .unwrap()
        .unwrap();
    let prepared = accepted
        .prepare_publication(
            &frontier,
            &ledger,
            arena.registration().unwrap(),
            heap,
            make(),
            interval.exit(),
            &next,
        )
        .unwrap();
    drop(ledger);
    let committed = prepared
        .publish(
            &mut frontier,
            &mut ledger_store.borrow_mut(),
            |_, members| assert!(members.is_empty()),
        )
        .unwrap();
    assert_eq!(charged, 1);
    assert_eq!(committed.balance.total().unwrap(), before + 16);
    assert_eq!(committed.position, next);
    assert_eq!(committed.exit, room::ExitKind::Return);
    committed
        .materializations
        .publish(&mut ledger_store.borrow_mut(), |entities| {
            assert!(entities.is_empty())
        })
        .unwrap();
}
