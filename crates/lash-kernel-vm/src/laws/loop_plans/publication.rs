use super::*;
mod construction;

/// The source publication protocol binds recovery, ownership, charge closes
/// and an exclusive shared admission; equal byte deltas cannot replace them.
pub(super) fn checked_source_publication() {
    construction::successors();
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
        unpaid: Some(interval.events().next().unwrap().point.clone()),
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
        unpaid: None,
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
            start.clone(),
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
    let committed_balance = ledger_store.borrow().balance().unwrap();
    let committed_charge = charged;
    assert!(
        frontier
            .handoff(&mut ledger_store.borrow_mut(), start.clone())
            .is_err()
    );
    assert_eq!(frontier.position(), &next);
    assert_eq!(ledger_store.borrow().balance().unwrap(), committed_balance);
    assert_eq!(charged, committed_charge);
    shared_cell_publication();
    persistent_assignment_publication();
    entered_nested_scope_publication();
}

/// Entered parameters retain their real invocation owners; leaving a nested
/// scope releases its bindings without consuming that invocation's frame.
fn entered_nested_scope_publication() {
    use crate::compile::{
        CodeId,
        room::{self, Phase, Point, publication::*},
    };
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::kernel_internals::{
        PagedHeap,
        logical::{
            FrameEvent, JournalEvent, Ledger, Lifecycle, Owner as Root, Program, Slot, SourceValue,
        },
    };
    use lash_kernel_value::{Name, sites::Site};
    use std::{collections::BTreeSet, sync::Arc};

    #[derive(Clone)]
    struct Facts<'a> {
        ledger: &'a Ledger,
        heap: &'a PagedHeap,
        slots: Arc<room::OwnerSlots>,
        invocation: u64,
    }
    impl room::Facts for Facts<'_> {
        fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
            Some(&[])
        }
        fn measure(&self, measure: &room::Measure) -> Option<i128> {
            let room::Measure::Release(selectors) = measure else {
                return None;
            };
            let mut owners = BTreeSet::new();
            for selector in selectors {
                if let Some(owner) = self.slots.root(selector, TaskId::MAIN, self.invocation) {
                    owners.insert(owner);
                } else {
                    owners.extend(
                        self.slots
                            .group(selector, TaskId::MAIN, self.invocation)?
                            .iter()
                            .copied(),
                    );
                }
            }
            let mut held = Vec::new();
            for owner in owners {
                if self
                    .ledger
                    .holding(owner, self.heap.pages())
                    .ok()?
                    .is_some()
                {
                    held.push(owner);
                }
            }
            self.ledger
                .release_cost(&held, self.heap.pages())
                .ok()
                .map(i128::from)
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
    struct Execution<'a>(Facts<'a>);
    impl room::Execution for Execution<'_> {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(self.0.clone()))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
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

    let run = Embedder::new(
        "main { let f = fn(entry_arg) { if true { let kept = 2 } return entry_arg } return apply f(1) }",
    );
    let (program, _) = run.program(false);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("nested-scope source admits"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("nested-scope source compiles"));
    let code = (0..exe.code_count())
        .map(|i| exe.code(CodeId(i as u32)))
        .find(|code| code.source_room.params.as_ref() == [Name::new("entry_arg")])
        .unwrap();
    let source = Arc::clone(&code.source_room);
    let slot = |name: &str| {
        let index = code
            .slots
            .iter()
            .position(|slot| slot.name == Name::new(name))
            .unwrap();
        let Root::Value { slot, .. } = source.binding_root(index as u32, TaskId::MAIN, 0).unwrap()
        else {
            panic!("source binding has no value owner");
        };
        slot
    };
    let argument_slot = slot("entry_arg");
    let nested_slot = slot("kept");
    let heap = run.machine.source_law_heap();
    let ledger_store = heap.logical_ledger();
    let lifecycle = Lifecycle::new(ledger_store.clone());
    let mut arena = lash_kernel_state::arena::Arena::new();
    let caller = lifecycle
        .enter(TaskId::MAIN, &mut arena, heap, &[], &[])
        .unwrap();
    let argument = caller.owner(Slot::Argument(0));
    lifecycle.replace(argument, Some(SourceValue::Value(Value::Int(1))));
    lifecycle.flush(&mut arena, heap).unwrap();
    // The canonical entry owner consumes the evaluated caller argument and
    // initializes the actual admitted formal slot before the body interval.
    let invocation = lifecycle
        .enter(
            TaskId::MAIN,
            &mut arena,
            heap,
            &[argument],
            &[(argument_slot, SourceValue::Value(Value::Int(1)))],
        )
        .unwrap();
    let parameter = invocation.owner(argument_slot);
    assert!(
        ledger_store
            .borrow()
            .holding(argument, heap.pages())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ledger_store
            .borrow()
            .holding(parameter, heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    let kept = invocation.owner(nested_slot);
    lifecycle.replace(kept, Some(SourceValue::Value(Value::Int(2))));
    lifecycle.flush(&mut arena, heap).unwrap();

    let mut scope = code
        .slots
        .iter()
        .find(|slot| slot.name == Name::new("kept"))
        .unwrap()
        .declared
        .clone();
    scope.path.pop().unwrap();
    let leave = Point {
        site: scope,
        phase: Phase::Leave,
    };
    assert_ne!(leave.site, source.site);
    let interval = source
        .compile_event(&leave, room::ExitKind::Normal)
        .unwrap();
    let ledger = ledger_store.borrow();
    assert_eq!(
        ledger
            .invocation(TaskId::MAIN, invocation.id(), heap.pages())
            .unwrap(),
        invocation
    );
    let before = ledger.balance().unwrap().total().unwrap();
    let next_invocation = ledger.prospective_invocation(TaskId::MAIN).id();
    let start = Position {
        at: leave.clone(),
        unpaid: Some(leave.clone()),
        recovery: [1; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
            departure: None,
        },
    };
    let successor = Point {
        site: source.site.child(1).child(0),
        phase: Phase::Commit,
    };
    assert!(source.room.peak_extra.contains_key(&successor));
    let next = Position {
        unpaid: Some(successor),
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
    let make = |consume_frame: bool, leave_frame: bool| {
        let mut inputs = vec![kept];
        if consume_frame {
            inputs.push(invocation.frame());
        }
        Program::new(vec![JournalEvent {
            inputs,
            frame: leave_frame.then_some(FrameEvent::Leave {
                task: TaskId::MAIN,
                invocation: invocation.id(),
            }),
            ..Default::default()
        }])
    };
    let facts = Facts {
        ledger: &ledger,
        heap,
        slots: source.owner_slots().unwrap(),
        invocation: invocation.id(),
    };
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: true,
        depth: 1,
        safe_point: 7,
    };
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let resource_facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Execution(facts.clone()),
            &ledger,
            &arena,
            heap,
            make(false, false),
            next.clone(),
            allowance.clone(),
            resource_facts,
        )
        .unwrap_or_else(|error| panic!("nested Leave preserves its invocation frame: {error:?}"));
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            before,
        )
        .unwrap()
        .unwrap();
    let prepared = accepted
        .prepare_publication(
            &frontier,
            &ledger,
            arena.registration().unwrap(),
            heap,
            make(false, false),
            interval.exit(),
            &next,
        )
        .unwrap();
    drop(prepared);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
    assert_eq!(charged, 0);
    for (consume_frame, leave_frame) in [(true, false), (false, true)] {
        let resource_facts = resources(&mut charged, &mut counters)
            .facts(&allowance)
            .unwrap();
        assert!(
            frontier
                .evaluate_interval(
                    Arc::clone(&interval),
                    &mut Execution(facts.clone()),
                    &ledger,
                    &arena,
                    heap,
                    make(consume_frame, leave_frame),
                    next.clone(),
                    allowance.clone(),
                    resource_facts
                )
                .is_err()
        );
    }
    let resource_facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            Arc::clone(&interval),
            &mut Execution(facts),
            &ledger,
            &arena,
            heap,
            make(false, false),
            next.clone(),
            allowance.clone(),
            resource_facts,
        )
        .unwrap();
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            before,
        )
        .unwrap()
        .unwrap();
    let prepared = accepted
        .prepare_publication(
            &frontier,
            &ledger,
            arena.registration().unwrap(),
            heap,
            make(false, false),
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
    committed
        .materializations
        .publish(&mut ledger_store.borrow_mut(), |entities| {
            assert!(entities.is_empty())
        })
        .unwrap();
    let ledger = ledger_store.borrow();
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before - 17);
    assert_eq!(
        ledger
            .invocation(TaskId::MAIN, invocation.id(), heap.pages())
            .unwrap(),
        invocation
    );
    assert!(
        ledger
            .holding(invocation.frame(), heap.pages())
            .unwrap()
            .is_some()
    );
    assert!(
        ledger
            .holding(caller.frame(), heap.pages())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        ledger
            .holding(parameter, heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    assert!(ledger.holding(kept, heap.pages()).unwrap().is_none());
    assert_eq!(
        ledger.prospective_invocation(TaskId::MAIN).id(),
        next_invocation
    );
    assert_eq!(charged, 0);
}

/// Persistent main assignments transfer their RHS into the session root;
/// their shared declaration does not authorize a variable-cell edit.
fn persistent_assignment_publication() {
    use crate::compile::room::{self, Phase, Point, publication::*};
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{
        JournalEvent, Owner as Root, Program, PublicationValue, RootValue,
    };
    use lash_kernel_value::{Name, sites::Site};
    use std::sync::Arc;

    struct Facts;
    impl room::Facts for Facts {
        fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
            Some(&[])
        }
        fn measure(&self, measure: &room::Measure) -> Option<i128> {
            match measure {
                room::Measure::Local(_) => Some(17),
                room::Measure::Release(owners)
                    if matches!(owners.as_ref(), [room::Owner::Visible { .. }]) =>
                {
                    Some(17)
                }
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
    struct Execution;
    impl room::Execution for Execution {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(Facts))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
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

    let run = Embedder::new("main { let n = 1 set n = 2 return n }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(lash_kernel_value::sites::Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    ));
    let assignment = source.site.child(1);
    let commit = Point {
        site: assignment.clone(),
        phase: Phase::Commit,
    };
    let interval = source
        .compile_event(&commit, room::ExitKind::Normal)
        .unwrap();
    let heap = run.machine.source_law_heap();
    let ledger_store = heap.logical_ledger();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let mut ledger = ledger_store.borrow_mut();
    let (invocation, prepared) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger
        .commit(prepared.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let owners = source.owner_slots().unwrap();
    let session = owners
        .root(
            &room::Owner::Visible {
                at: assignment.clone(),
                name: Name::new("n"),
            },
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    assert_eq!(session, Root::session(&Name::new("n")));
    let operand = owners
        .root(
            &room::Owner::Operand(assignment.child(0)),
            TaskId::MAIN,
            invocation.id(),
        )
        .unwrap();
    let prepared = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[],
            &[
                (session, RootValue::Value(&Value::Int(1))),
                (operand, RootValue::Value(&Value::Int(2))),
            ],
        )
        .unwrap();
    ledger
        .commit(prepared.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let before = ledger.balance().unwrap().total().unwrap();
    drop(ledger);
    let ledger = ledger_store.borrow();
    let start = Position {
        at: commit.clone(),
        unpaid: Some(commit.clone()),
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
        unpaid: Some(Point {
            site: source.site.child(2).child(0),
            phase: Phase::Commit,
        }),
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
    let make = || {
        Program::new(vec![JournalEvent {
            inputs: vec![operand, session],
            outputs: vec![(session, PublicationValue::Existing(operand))],
            ..Default::default()
        }])
    };
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 1,
        close: None,
        strict: true,
        depth: 1,
        safe_point: 7,
    };
    let mut charged = 0;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let facts = resources(&mut charged, &mut counters)
        .facts(&allowance)
        .unwrap();
    let evaluated = frontier
        .evaluate_interval(
            interval.clone(),
            &mut Execution,
            &ledger,
            &arena,
            heap,
            make(),
            next.clone(),
            allowance.clone(),
            facts,
        )
        .unwrap_or_else(|error| panic!("persistent assignment keeps its session root: {error:?}"));
    let accepted = evaluated
        .admit_interval(
            &frontier,
            &ledger,
            &arena,
            resources(&mut charged, &mut counters),
            &allowance,
            before,
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
            |_, members| assert!(members.is_empty(), "session write is not a cell edit"),
        )
        .unwrap();
    committed
        .materializations
        .publish(&mut ledger_store.borrow_mut(), |entities| {
            assert!(entities.is_empty())
        })
        .unwrap();
    let ledger = ledger_store.borrow();
    assert_eq!(charged, 1);
    assert_eq!(ledger.balance().unwrap().total().unwrap(), before - 17);
    assert_eq!(
        ledger
            .holding(session, heap.pages())
            .unwrap()
            .unwrap()
            .bytes(),
        17
    );
    assert!(ledger.holding(operand, heap.pages()).unwrap().is_none());
}

fn shared_cell_publication() {
    use crate::compile::CodeId;
    use crate::compile::room::{self, Phase, Point, Selection, publication::*};
    use lash_kernel_doc::{self as doc, TaskId, Value, arena::ArenaHandle, heap::Identity};
    use lash_kernel_state::kernel_internals::logical::{
        self, JournalEvent, Member, MemberEdit, Program, PublicationValue, RootValue, Slot,
    };
    use lash_kernel_value::Name;
    use lash_kernel_value::sites::Site;
    use std::sync::Arc;

    struct Facts {
        assignment: Site,
        rhs: Site,
        visible_price: i128,
        dependencies: [ArenaHandle; 1],
    }
    impl room::Facts for Facts {
        fn dependencies(&self) -> Option<&[ArenaHandle]> {
            Some(&self.dependencies)
        }
        fn measure(&self, measure: &room::Measure) -> Option<i128> {
            match measure {
                room::Measure::Local(room::Owner::Operand(site)) if site == &self.rhs => Some(18),
                room::Measure::MemberDelta(site) if site == &self.assignment => Some(1),
                room::Measure::Release(owners) => {
                    owners.iter().try_fold(0, |total, owner| match owner {
                        room::Owner::Operand(site) if site == &self.rhs => Some(total + 18),
                        room::Owner::Visible { at, name }
                            if at == &self.assignment && *name == Name::new("n") =>
                        {
                            Some(total + self.visible_price)
                        }
                        _ => None,
                    })
                }
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
    struct Execution<'a>(&'a Facts);
    impl room::Execution for Execution<'_> {
        fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
            Some(Box::new(Facts {
                assignment: self.0.assignment.clone(),
                rhs: self.0.rhs.clone(),
                visible_price: self.0.visible_price,
                dependencies: self.0.dependencies,
            }))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
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

    for (text, extra) in [
        (
            "main { let n = 1 let f = fn(rhs) { set n = rhs return n } return apply f(256) }",
            0,
        ),
        (
            "main { let f = fn(rhs) { let n = 1 let keep = fn() { return n } set n = rhs return apply keep() } return apply f(256) }",
            16,
        ),
    ] {
        let run = Embedder::new(text);
        let (program, _) = run.program(false);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("shared-cell program admits"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("shared-cell program compiles"));
        let n = Name::new("n");
        let rhs = Name::new("rhs");
        let code = (0..exe.code_count())
            .map(|index| exe.code(CodeId(index as u32)))
            .find(|code| {
                code.source_room.params.as_ref() == [rhs.clone()]
                    && code.source_room.body.iter().any(|stmt| {
                        matches!(
                            stmt,
                            doc::Stmt::Assign {
                                place: doc::Place::Variable(name),
                                value: doc::Rhs::Expr(doc::Expr::Variable(value)),
                                ..
                            } if name == &n && value == &rhs
                        )
                    })
            })
            .unwrap();
        let source = Arc::clone(&code.source_room);
        let assignment_index = source
            .body
            .iter()
            .position(|stmt| {
                matches!(
                    stmt,
                    doc::Stmt::Assign {
                        place: doc::Place::Variable(name),
                        value: doc::Rhs::Expr(doc::Expr::Variable(value)),
                        ..
                    } if name == &n && value == &rhs
                )
            })
            .unwrap();
        let assignment = source.site.child(assignment_index.try_into().unwrap());
        let rhs_site = assignment.child(0);
        let commit = Point {
            site: assignment.clone(),
            phase: Phase::Commit,
        };
        let selections = [
            Selection::Event {
                at: Point {
                    site: rhs_site.clone(),
                    phase: Phase::Commit,
                },
                exit: room::ExitKind::Normal,
            },
            Selection::Event {
                at: commit.clone(),
                exit: room::ExitKind::Normal,
            },
        ];
        let interval = source
            .compile_between(&commit, &commit, &selections, 32)
            .unwrap();
        let mut following = doc::Node::Stmt(&source.body[assignment_index + 1]);
        let mut following_site = source
            .site
            .child((assignment_index + 1).try_into().unwrap());
        while let Some(child) = following.children().first().copied() {
            following = child;
            following_site = following_site.child(0);
        }
        let next_point = Point {
            site: following_site,
            phase: if matches!(
                following,
                doc::Node::Action(_) | doc::Node::Expr(doc::Expr::Act(_))
            ) {
                Phase::Enter
            } else {
                Phase::Commit
            },
        };
        let heap = run.machine.source_law_heap();
        let ledger_store = heap.logical_ledger();
        let mut arena = lash_kernel_state::arena::Arena::new();
        let first = arena.new_cell(Value::Int(1));
        let second = arena.new_cell(Value::Int(1));
        let first_ref = Value::ObjectRef(first);
        let second_ref = Value::ObjectRef(second);
        let rhs_value = Value::Int(256);
        let mut ledger = ledger_store.borrow_mut();
        let baseline = ledger.balance().unwrap().total().unwrap();
        let (caller, prepared) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[
                    (Slot::Binding(0), RootValue::Value(&first_ref)),
                    (Slot::Binding(1), RootValue::Value(&second_ref)),
                ],
            )
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let invocation = ledger.prospective_invocation(TaskId::MAIN);
        let owners = source.owner_slots().unwrap();
        let visible = owners
            .root(
                &room::Owner::Visible {
                    at: assignment.clone(),
                    name: n,
                },
                TaskId::MAIN,
                invocation.id(),
            )
            .unwrap();
        let logical::Owner::Value { slot: n_slot, .. } = visible else {
            panic!("visible variable has no frame slot");
        };
        let n_value = match n_slot {
            Slot::Capture(_) => RootValue::Capture(first),
            Slot::Binding(_) => RootValue::Value(&first_ref),
            _ => panic!("visible variable has another source slot"),
        };
        let rhs_index = code
            .slots
            .iter()
            .position(|slot| slot.name == rhs && slot.declared == source.site)
            .unwrap();
        let logical::Owner::Value { slot: rhs_slot, .. } = source
            .binding_root(rhs_index as u32, TaskId::MAIN, invocation.id())
            .unwrap()
        else {
            panic!("formal argument has no frame slot");
        };
        let (invocation, prepared) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[(n_slot, n_value), (rhs_slot, RootValue::Value(&rhs_value))],
            )
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let operand = owners
            .root(
                &room::Owner::Operand(rhs_site.clone()),
                TaskId::MAIN,
                invocation.id(),
            )
            .unwrap();
        let prepared = ledger
            .prepare_ownership(
                &mut arena,
                heap.pages(),
                &[],
                &[(operand, RootValue::Value(&rhs_value))],
            )
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let before = baseline + 294 + extra;
        assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
        let first_id = arena.identify(first).unwrap();
        drop(ledger);
        let ledger = ledger_store.borrow();
        let start = Position {
            at: commit.clone(),
            unpaid: Some(commit.clone()),
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
            at: commit,
            unpaid: Some(next_point),
            recovery: [2; 32],
            ..start.clone()
        };
        let mut frontier = source
            .frontier(
                &ledger,
                heap,
                &invocation,
                program.link,
                doc::KernelVersion::One,
                start,
            )
            .unwrap();
        let facts = Facts {
            assignment,
            rhs: rhs_site,
            visible_price: extra.into(),
            dependencies: [first],
        };
        let make = |target, consume_visible, replace_visible| {
            let mut inputs = vec![operand];
            if consume_visible {
                inputs.push(visible);
            }
            Program::new(vec![JournalEvent {
                inputs,
                outputs: if replace_visible {
                    vec![(visible, PublicationValue::Existing(visible))]
                } else {
                    vec![]
                },
                member: Some(MemberEdit {
                    handle: target,
                    member: Member::Variable,
                    next: Some(rhs_value),
                }),
                ..Default::default()
            }])
        };
        let allowance = crate::admission::Allowance {
            occurrences: vec![],
            before: 0,
            units: 1,
            close: None,
            strict: true,
            depth: 1,
            safe_point: 7,
        };
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        for bound in 0..=before + 18 {
            let resource_facts = resources(&mut charged, &mut counters)
                .facts(&allowance)
                .unwrap();
            let evaluated = frontier
                .evaluate_interval(
                    Arc::clone(&interval),
                    &mut Execution(&facts),
                    &ledger,
                    &arena,
                    heap,
                    make(first, false, false),
                    next.clone(),
                    allowance.clone(),
                    resource_facts,
                )
                .unwrap_or_else(|error| panic!("valid variable-cell publication: {error:?}"));
            assert!(
                evaluated
                    .admit_interval(
                        &frontier,
                        &ledger,
                        &arena,
                        resources(&mut charged, &mut counters),
                        &allowance,
                        bound,
                    )
                    .unwrap()
                    .is_none()
            );
            assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
            assert_eq!(charged, 0);
            assert!(matches!(arena.cell(first).unwrap(), Value::Int(1)));
            assert!(matches!(arena.cell(second).unwrap(), Value::Int(1)));
        }
        for (target, consume_visible, replace_visible) in [
            (second, false, false),
            (first, true, false),
            (first, true, true),
        ] {
            let resource_facts = resources(&mut charged, &mut counters)
                .facts(&allowance)
                .unwrap();
            if let Ok(evaluated) = frontier.evaluate_interval(
                Arc::clone(&interval),
                &mut Execution(&facts),
                &ledger,
                &arena,
                heap,
                make(target, consume_visible, replace_visible),
                next.clone(),
                allowance.clone(),
                resource_facts,
            ) && let Some(accepted) = evaluated
                .admit_interval(
                    &frontier,
                    &ledger,
                    &arena,
                    resources(&mut charged, &mut counters),
                    &allowance,
                    before + 19,
                )
                .unwrap()
            {
                assert!(
                    accepted
                        .prepare_publication(
                            &frontier,
                            &ledger,
                            arena.registration().unwrap(),
                            heap,
                            make(target, consume_visible, replace_visible),
                            interval.exit(),
                            &next,
                        )
                        .is_err()
                );
            }
            assert_eq!(ledger.balance().unwrap().total().unwrap(), before);
            assert_eq!(charged, 0);
            assert!(matches!(arena.cell(first).unwrap(), Value::Int(1)));
            assert!(matches!(arena.cell(second).unwrap(), Value::Int(1)));
        }
        let resource_facts = resources(&mut charged, &mut counters)
            .facts(&allowance)
            .unwrap();
        let evaluated = frontier
            .evaluate_interval(
                Arc::clone(&interval),
                &mut Execution(&facts),
                &ledger,
                &arena,
                heap,
                make(first, false, false),
                next.clone(),
                allowance.clone(),
                resource_facts,
            )
            .unwrap();
        let accepted = evaluated
            .admit_interval(
                &frontier,
                &ledger,
                &arena,
                resources(&mut charged, &mut counters),
                &allowance,
                before + 19,
            )
            .unwrap()
            .unwrap();
        let prepared = accepted
            .prepare_publication(
                &frontier,
                &ledger,
                arena.registration().unwrap(),
                heap,
                make(first, false, false),
                interval.exit(),
                &next,
            )
            .unwrap();
        drop(ledger);
        let committed = prepared
            .publish(
                &mut frontier,
                &mut ledger_store.borrow_mut(),
                |arena, members| {
                    for edit in members {
                        if let Member::Variable = edit.member {
                            arena.cell_set(edit.handle, edit.next.unwrap()).unwrap();
                        } else {
                            panic!("unexpected member edit");
                        }
                    }
                },
            )
            .unwrap();
        committed
            .materializations
            .publish(&mut ledger_store.borrow_mut(), |_| {})
            .unwrap();
        assert_eq!(charged, 1);
        assert_eq!(
            ledger_store.borrow().balance().unwrap().total().unwrap(),
            baseline + 277 + extra
        );
        assert!(matches!(arena.cell(first).unwrap(), Value::Int(256)));
        assert!(matches!(arena.cell(second).unwrap(), Value::Int(1)));
        for owner in [caller.owner(Slot::Binding(0)), visible] {
            let mut edges = Vec::new();
            ledger_store
                .borrow()
                .holding(owner, heap.pages())
                .unwrap()
                .unwrap()
                .visit_edges(heap.pages(), &mut |identity, _| {
                    edges.push(identity);
                    Ok(())
                })
                .unwrap();
            assert_eq!(edges, [Identity::Object(first_id)]);
        }
    }
}
