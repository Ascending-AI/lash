use super::*;

/// Entered parameters retain their real invocation owners; leaving a nested
/// scope releases its bindings without consuming that invocation's frame.
pub(super) fn nested_scope() {
    use crate::compile::{
        CodeId,
        room::{self, Phase, Point, publication::*},
    };
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::kernel_internals::{
        PagedHeap,
        logical::{
            FrameEvent, JournalEvent, Ledger, Owner as Root, Program, PublicationValue, Slot,
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
    let store = ledger_store.clone();
    let mut arena = lash_kernel_state::arena::Arena::new();
    store
        .borrow_mut()
        .transfer_source(&mut arena, heap, &[], &[])
        .unwrap();
    let caller = store
        .borrow_mut()
        .enter_source(TaskId::MAIN, &mut arena, heap, &[], &[])
        .unwrap();
    let argument = caller.owner(Slot::Argument(crate::laws::role_id(0)));
    store
        .borrow_mut()
        .transfer_source(
            &mut arena,
            heap,
            &[],
            &[(
                argument,
                lash_kernel_state::kernel_internals::logical::RootValue::Value(&Value::Int(1)),
            )],
        )
        .unwrap();
    store
        .borrow_mut()
        .transfer_source(&mut arena, heap, &[], &[])
        .unwrap();
    // The canonical entry owner consumes the evaluated caller argument and
    // initializes the actual admitted formal slot before the body interval.
    store
        .borrow_mut()
        .transfer_source(&mut arena, heap, &[], &[])
        .unwrap();
    let invocation = store
        .borrow_mut()
        .enter_source(
            TaskId::MAIN,
            &mut arena,
            heap,
            &[argument],
            &[(argument_slot, PublicationValue::Native(Value::Int(1)))],
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
    store
        .borrow_mut()
        .transfer_source(
            &mut arena,
            heap,
            &[],
            &[(
                kept,
                lash_kernel_state::kernel_internals::logical::RootValue::Value(&Value::Int(2)),
            )],
        )
        .unwrap();
    store
        .borrow_mut()
        .transfer_source(&mut arena, heap, &[], &[])
        .unwrap();

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
        },
    };
    let successor = Point {
        site: source.site.child(1).child(0),
        phase: Phase::Commit,
    };
    assert!(source.room.events.contains_key(&successor));
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
