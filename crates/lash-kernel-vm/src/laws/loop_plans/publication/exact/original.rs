//! K-CHG-002: published constructor work is never replayed by its suffix.
use super::*;
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::charges::{AcceptedOriginalPrefix, OriginalPrefix};
use crate::compile::{CodeId, Executable, StmtId, expr::ConstructorPrefix};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{Ledger, RootValue},
    },
};

struct TupleFacts {
    local: u64,
}
impl room::Facts for TupleFacts {
    fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
        Some(&[])
    }
    fn measure(&self, measure: &room::Measure) -> Option<i128> {
        match measure {
            room::Measure::Local(_) => Some(self.local.into()),
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
impl room::Execution for TupleFacts {
    fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
        Some(Box::new(TupleFacts { local: self.local }))
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
fn resources<'a>(
    charged: &'a mut u64,
    counters: &'a mut crate::machine::tasks::Occurrences,
    bound: u64,
) -> SourceResources<'a> {
    let safe_point = *charged;
    SourceResources {
        charged,
        counters,
        bound,
        slice: u64::MAX,
        depth: 0,
        limit: 10,
        safe_point,
    }
}
fn allowance(charged: u64) -> Allowance {
    Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: charged,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one source-prefix fixture and its actual boundary"
)]
fn publish(
    exe: &Executable,
    code: CodeId,
    statement: StmtId,
    pc: u32,
    frontier: &mut Frontier,
    ledger: &mut Ledger,
    heap: &PagedHeap,
    arena: &mut Arena,
    charged: &mut u64,
    counters: &mut crate::machine::tasks::Occurrences,
    previous: Option<AcceptedOriginalPrefix>,
    abandon: bool,
) -> Option<AcceptedOriginalPrefix> {
    let source = &exe.code(code).source_room;
    let (stream, stmt, _) = exe.statement(statement);
    let compiled = ConstructorPrefix::compile(stream, stmt.ops, pc).unwrap();
    let original = OriginalPrefix::compile(exe, code, statement, pc).unwrap();
    let at = Point {
        site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
        phase: Phase::Commit,
    };
    let output = source
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Operand(at.site.clone()),
            TaskId::MAIN,
            frontier.position().occurrence.invocation,
        )
        .unwrap();
    let next = Position {
        at: at.clone(),
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..frontier.position().clone()
    };
    let interval = source.compile_event(&at, room::ExitKind::Normal).unwrap();
    let exact = ExactEvent::compile(source, interval, Successor::Normal).unwrap();
    let need = allowance(*charged);
    let facts = resources(charged, counters, u64::MAX).facts(&need).unwrap();
    let before = (arena.allocations(), ledger.balance().unwrap(), *charged);
    let (tuple, mut installation) = arena.prepare_empty_tuple().unwrap();
    let page =
        crate::machine::project::project_prepared(&mut installation, heap.pages(), &tuple).unwrap();
    let local = logical::Holding::from_page(heap.pages(), &page)
        .unwrap()
        .bytes();
    let (mut registration, batch) = installation.into_parts();
    let bound = exact
        .bind_construction(
            frontier,
            ledger,
            registration.arena(),
            heap,
            &mut TupleFacts { local },
            ExactRoots {
                inputs: vec![],
                outputs: vec![(output, logical::PublicationValue::Page(page.clone()))],
                ..Default::default()
            },
            next,
            need,
            facts,
            ExternalFacts::default(),
            (
                Some(&lash_kernel_doc::NewObject::Tuple(vec![])),
                None,
                None,
                None,
            ),
            None,
        )
        .unwrap();
    let registers = vec![Value::Absent; stmt.regs as usize];
    let bound = bound
        .with_original_prefix(
            frontier,
            ledger,
            registration.arena(),
            heap,
            &original,
            &registers,
            previous.as_ref(),
        )
        .unwrap();
    let transaction = ledger
        .prepare_source_boundary(registration, heap, &[], &[(output, RootValue::Page(&page))])
        .unwrap();
    let prepared = bound
        .finish_external(frontier, ledger, transaction)
        .unwrap();
    let peak = prepared.peak();
    let prepared = prepared.collect(ledger, heap).unwrap();
    assert!(prepared.peak() <= peak);
    let accepted = prepared
        .admit(
            frontier,
            ledger,
            resources(charged, counters, u64::MAX),
            u64::MAX,
        )
        .unwrap()
        .unwrap();
    if abandon {
        drop(accepted);
        assert_eq!(
            (arena.allocations(), ledger.balance().unwrap(), *charged),
            before
        );
        return previous;
    }
    let tape = crate::observe::ChargeTape::new();
    let mut receipt = previous;
    accepted
        .publish_original_event(
            frontier,
            ledger,
            &mut receipt,
            |arena, edits| {
                assert!(edits.is_empty());
                batch.publish(arena);
            },
            Some(&tape),
        )
        .unwrap();
    let expected = compiled
        .pending()
        .iter()
        .flat_map(|close| {
            stream
                .regions
                .events(close.region)
                .iter()
                .take(stream.regions.executed(close.region, pc))
                .map(|event| event.weight)
        })
        .collect::<Vec<_>>();
    // For this fixture the receipt is the first payment in the original region.
    assert_eq!(
        *charged,
        7 + expected.iter().map(|weight| weight.units()).sum::<u64>()
    );
    assert_eq!(tape.steps(), expected);
    receipt
}

pub(super) fn prefixes() {
    for layout in [0, 1, 42] {
        for (text, guard) in [
            ("main { return () }", 0),
            ("main { return same((), 2) }", 0),
        ]
        .into_iter()
        .chain((0..10).map(|guard| ("main { return ((), 2) }", guard)))
        {
            let mut run = Embedder::with(
                text,
                Setup {
                    layout: crate::Layout(layout),
                    probes: Some(crate::laws::layout::helper_probes),
                    ..Setup::default()
                },
            );
            let (program, _) = run.program(false);
            let admitted = program
                .library
                .admit(&program.document, Default::default(), &program.selection)
                .unwrap_or_else(|_| panic!("prefix admitted"));
            let exe = admitted
                .executable(
                    &program.document,
                    &program.library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .unwrap_or_else(|_| panic!("prefix compiled"));
            let code = exe.main.unwrap();
            let statement = exe.block(exe.code(code).body).stmts[0];
            let (stream, stmt, _) = exe.statement(statement);
            let pc = (stmt.ops.start..stmt.ops.end()).find(|pc| matches!(stream.ops[*pc as usize], lash_kernel_exec::Op::MakeTuple { items } if items.len == 0)).unwrap();
            let source = &exe.code(code).source_room;
            let at = Point {
                site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
                phase: Phase::Commit,
            };
            let heap = run.machine.source_law_heap();
            let store = heap.logical_ledger();
            *store.borrow_mut() = Ledger::fresh();
            let mut arena = Arena::new();
            let (invocation, enter) = store
                .borrow()
                .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
                .unwrap();
            store
                .borrow_mut()
                .commit(enter.admit(u64::MAX).ok().unwrap())
                .unwrap();
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
            let mut frontier = source
                .frontier(
                    &store.borrow(),
                    heap,
                    &invocation,
                    program.link,
                    lash_kernel_doc::KernelVersion::One,
                    start.clone(),
                )
                .unwrap();
            let mut charged = 7;
            let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
            assert!(
                publish(
                    &exe,
                    code,
                    statement,
                    pc,
                    &mut frontier,
                    &mut store.borrow_mut(),
                    heap,
                    &mut arena,
                    &mut charged,
                    &mut counters,
                    None,
                    true
                )
                .is_none()
            );
            assert_eq!(frontier.position(), &start);
            let mut receipt = publish(
                &exe,
                code,
                statement,
                pc,
                &mut frontier,
                &mut store.borrow_mut(),
                heap,
                &mut arena,
                &mut charged,
                &mut counters,
                None,
                false,
            )
            .unwrap();
            if guard != 0 {
                if guard >= 8 {
                    receipt.corrupt_recovery_for_law(guard == 8);
                }
                let foreign = Arc::new(room::Source::compile(
                    lash_kernel_doc::constant_pool::PoolOwner::Document(
                        program.document.identity(),
                    ),
                    source.site.clone(),
                    &source.body,
                    &[],
                    Default::default(),
                    &std::collections::BTreeMap::new(),
                    &std::collections::BTreeSet::new(),
                ));
                let foreign_ledger = Ledger::fresh();
                let ledger = store.borrow();
                assert!(
                    receipt
                        .into_replay(
                            if guard == 1 { &foreign } else { source },
                            if guard == 2 { &foreign_ledger } else { &ledger },
                            if guard == 3 { CodeId(code.0 + 1) } else { code },
                            if guard == 4 {
                                StmtId(statement.0 + 1)
                            } else {
                                statement
                            },
                            if guard == 5 { TaskId(99) } else { TaskId::MAIN },
                            invocation.id() + u64::from(guard == 6),
                            charged + u64::from(guard == 7),
                        )
                        .is_err(),
                    "original source handoff must reject forged identity/paid prefix {guard}"
                );
                assert_eq!(
                    *frontier.position(),
                    Position {
                        unpaid: Some(source.next_after_commit(&start.at.site).unwrap()),
                        ..start.clone()
                    }
                );
                continue;
            }
            let end = Point {
                site: stmt.site.clone(),
                phase: Phase::Commit,
            };
            if text == "main { return () }" {
                let root = source
                    .owner_slots()
                    .unwrap()
                    .root(
                        &room::Owner::Operand(end.site.child(0)),
                        TaskId::MAIN,
                        invocation.id(),
                    )
                    .unwrap();
                let position = Position {
                    at: end.clone(),
                    unpaid: Some(end.clone()),
                    ..frontier.position().clone()
                };
                frontier.handoff(&mut store.borrow_mut(), position).unwrap();
                let exit = source.departure_exit(&end).unwrap();
                let next = exit.next(source, frontier.position(), None).unwrap();
                let need = allowance(charged);
                let facts = resources(&mut charged, &mut counters, u64::MAX)
                    .facts(&need)
                    .unwrap();
                let ledger = store.borrow();
                let prepared = exit
                    .prepare(
                        source,
                        &frontier,
                        &invocation,
                        &ledger,
                        arena.registration().unwrap(),
                        heap,
                        Some(root),
                        &[],
                        next,
                        need,
                        None,
                        facts,
                    )
                    .unwrap()
                    .unwrap();
                let prepared = prepared
                    .with_original_return(&frontier, &ledger, &receipt)
                    .unwrap();
                let peak = prepared.peak();
                let prepared = prepared.collect(&ledger, heap).unwrap();
                assert!(prepared.peak() <= peak);
                let paid = charged;
                let accepted = prepared
                    .admit(
                        &frontier,
                        &ledger,
                        resources(&mut charged, &mut counters, u64::MAX),
                        u64::MAX,
                    )
                    .unwrap()
                    .unwrap();
                drop(ledger);
                accepted
                    .publish_original_departure(
                        &mut frontier,
                        &mut store.borrow_mut(),
                        &mut Some(receipt),
                        |_| {},
                        None,
                    )
                    .unwrap();
                assert_eq!(charged, paid, "the return must not repay the constructor");
                assert!(frontier.position().unpaid.is_none());
            } else if text == "main { return same((), 2) }" {
                native(
                    &exe,
                    code,
                    statement,
                    &invocation,
                    &mut frontier,
                    heap,
                    &mut arena,
                    &mut charged,
                    &mut counters,
                    receipt,
                );
            } else {
                let paid = charged;
                let replay = receipt
                    .into_replay(
                        source,
                        &store.borrow(),
                        code,
                        statement,
                        TaskId::MAIN,
                        invocation.id(),
                        charged,
                    )
                    .unwrap();
                assert_eq!(replay.next, pc + 1);
                assert!(
                    !replay.roles.is_empty(),
                    "the completed tuple remains a source role"
                );
                assert!(
                    !replay.events.is_empty(),
                    "the unexecuted suffix retains real debt"
                );
                let expected = ConstructorPrefix::compile(stream, stmt.ops, pc)
                    .unwrap()
                    .pending()
                    .iter()
                    .flat_map(|close| stream.regions.events(close.region).iter().copied())
                    .collect::<Vec<_>>();
                let total = expected
                    .iter()
                    .map(|event| event.weight.units())
                    .sum::<u64>();
                assert_eq!(
                    paid - 7
                        + replay
                            .events
                            .iter()
                            .map(|(_, event)| event.weight.units())
                            .sum::<u64>(),
                    total
                );
                assert!(replay.events.iter().all(|(at, _)| *at >= replay.next));
                for bound in 7..=7 + total {
                    let oracle = lash_kernel_exec::charge::ReferenceCharger::run(
                        7,
                        bound,
                        &expected
                            .iter()
                            .map(|event| event.weight)
                            .collect::<Vec<_>>(),
                    );
                    if bound >= paid {
                        let suffix = lash_kernel_exec::charge::ReferenceCharger::run(
                            paid,
                            bound,
                            &replay
                                .events
                                .iter()
                                .map(|(_, event)| event.weight)
                                .collect::<Vec<_>>(),
                        );
                        match (oracle, suffix) {
                            (
                                lash_kernel_exec::charge::Ending::Within { charged: a },
                                lash_kernel_exec::charge::Ending::Within { charged: b },
                            )
                            | (
                                lash_kernel_exec::charge::Ending::Crossed { charged: a, .. },
                                lash_kernel_exec::charge::Ending::Crossed { charged: b, .. },
                            ) => assert_eq!(a, b),
                            mismatch => {
                                panic!("original suffix changed first refusal: {mismatch:?}")
                            }
                        }
                    }
                }
                run.machine.check_original_recovery_bank(
                    &exe,
                    code,
                    statement,
                    &invocation,
                    replay,
                );
            }
        }
    }
}

struct NativeFacts {
    call: room::CheckedRoom,
    source: room::CallSource,
    released: u64,
    handles: Vec<lash_kernel_doc::arena::ArenaHandle>,
}
impl room::Facts for &NativeFacts {
    fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
        Some(&self.handles)
    }
    fn measure(&self, measure: &room::Measure) -> Option<i128> {
        match measure {
            room::Measure::Release(_) => Some(self.released.into()),
            room::Measure::NativeScratch { .. } => Some(0),
            _ => None,
        }
    }
    fn call(&self, _: &Site, callee: room::CallSource) -> Option<&room::CheckedRoom> {
        (callee == self.source).then_some(&self.call)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}
impl room::Execution for NativeFacts {
    fn event(&mut self, _: &Point) -> Option<Box<dyn room::Facts + '_>> {
        Some(Box::new(&*self))
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
#[expect(
    clippy::too_many_arguments,
    reason = "native suffix of the actual prefix fixture"
)]
fn native(
    exe: &Executable,
    code: CodeId,
    statement: StmtId,
    invocation: &logical::Invocation,
    frontier: &mut Frontier,
    heap: &PagedHeap,
    arena: &mut Arena,
    charged: &mut u64,
    counters: &mut crate::machine::tasks::Occurrences,
    receipt: AcceptedOriginalPrefix,
) {
    use crate::compile::room::charges::{ChargeProgram, FormulaValues};
    let source = &exe.code(code).source_room;
    let (stream, stmt, _) = exe.statement(statement);
    let pc = (stmt.ops.start..stmt.ops.end())
        .find(|pc| {
            matches!(
                stream.ops[*pc as usize],
                lash_kernel_exec::Op::Operator { .. } | lash_kernel_exec::Op::Call { .. }
            )
        })
        .unwrap();
    let (lib, args) = match stream.ops[pc as usize] {
        lash_kernel_exec::Op::Operator { lib, args } | lash_kernel_exec::Op::Call { lib, args } => {
            (crate::compile::LibId(lib.0), args)
        }
        _ => unreachable!(),
    };
    let at = Point {
        site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
        phase: Phase::Commit,
    };
    let slots = source.owner_slots().unwrap();
    let root = |at: &Site| {
        slots
            .root(
                &room::Owner::Operand(at.clone()),
                TaskId::MAIN,
                invocation.id(),
            )
            .unwrap()
    };
    let inputs = [root(&at.site.child(0)), root(&at.site.child(1))];
    let output = root(&at.site);
    let store = heap.logical_ledger();
    let held = store
        .borrow()
        .prepare_source_ownership(
            arena,
            heap,
            &[],
            &[(inputs[1], RootValue::Value(&Value::Int(2)))],
        )
        .unwrap();
    store
        .borrow_mut()
        .commit(held.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let tuple = arena.tuple(vec![], 128).unwrap();
    let values = [tuple, Value::Int(2)];
    assert_eq!(args.len, 2);
    let function = exe.lib(lib);
    let crate::compile::LibRun::Native { native, .. } = &function.run else {
        panic!("same is native")
    };
    let contract = native
        .resource_contract(lash_kernel_doc::Args::new(
            &values,
            arena.objects(),
            &arena.semantics(),
        ))
        .unwrap()
        .unwrap();
    let pending = ConstructorPrefix::compile(
        stream,
        stmt.ops,
        (stmt.ops.start..pc)
            .find(|at| {
                matches!(
                    stream.ops[*at as usize],
                    lash_kernel_exec::Op::MakeTuple { .. }
                )
            })
            .unwrap(),
    )
    .unwrap();
    let charges =
        ChargeProgram::native_packet(exe, code, statement, pc, pending.pending().last().copied())
            .unwrap();
    let debt = charges
        .bind_measured(
            &[FormulaValues {
                arguments: &values,
                result: Value::Bool(false),
            }],
            |_, input, measure| {
                let value = match input {
                    crate::compile::Source::Arg(at) => values.get(at),
                    crate::compile::Source::Result => Some(&Value::Bool(false)),
                    crate::compile::Source::Nothing => None,
                };
                value.map_or(Some(0), |value| match measure {
                    lash_kernel_doc::Measure::Size => crate::data::view_size(arena, value).ok(),
                    lash_kernel_doc::Measure::DeepSize => {
                        crate::data::view_deep_size(arena, value).ok()
                    }
                    lash_kernel_doc::Measure::NestedSize => {
                        crate::data::view_nested_size(arena, value).ok()
                    }
                    lash_kernel_doc::Measure::Magnitude => {
                        Some(crate::data::view_magnitude(arena, value))
                    }
                })
            },
        )
        .unwrap();
    let start = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        ..frontier.position().clone()
    };
    frontier
        .handoff(&mut store.borrow_mut(), start.clone())
        .unwrap();
    let next = Position {
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..start
    };
    let exact = ExactEvent::compile(
        source,
        source.compile_event(&at, room::ExitKind::Normal).unwrap(),
        Successor::Normal,
    )
    .unwrap();
    let mut receipt = Some(receipt);
    for stale in [true, false] {
        let need = debt.native_allowance(*charged).unwrap();
        let facts = resources(charged, counters, u64::MAX).facts(&need).unwrap();
        let ledger = store.borrow();
        let mut execution = NativeFacts {
            call: room::CheckedRoom::native(function.specialization.function, &contract).unwrap(),
            source: function.specialization.resource,
            released: ledger.release_cost(&inputs, heap.pages()).unwrap(),
            handles: values.iter().filter_map(Value::handle).collect(),
        };
        let bound = exact
            .bind_external(
                frontier,
                &ledger,
                arena,
                heap,
                &mut execution,
                ExactRoots {
                    inputs: inputs.to_vec(),
                    outputs: vec![(
                        output,
                        logical::PublicationValue::Native(Value::Bool(false)),
                    )],
                    ..Default::default()
                },
                next.clone(),
                need,
                facts,
                ExternalFacts {
                    operands: &values,
                    arguments: &values,
                    native: Some(&contract),
                    ..Default::default()
                },
                &debt,
            )
            .unwrap()
            .with_original_native(frontier, &ledger, receipt.as_ref().unwrap(), &debt)
            .unwrap();
        let prepared = bound
            .prepare(frontier, &ledger, arena.registration().unwrap(), heap)
            .unwrap();
        let paid = *charged;
        let accepted = prepared
            .admit(
                frontier,
                &ledger,
                resources(charged, counters, u64::MAX),
                u64::MAX,
            )
            .unwrap()
            .unwrap();
        drop(ledger);
        if stale {
            store.borrow_mut().handoff().unwrap();
        }
        let tape = crate::observe::ChargeTape::new();
        let result = accepted.publish_original_event(
            frontier,
            &mut store.borrow_mut(),
            &mut receipt,
            |_, edits| assert!(edits.is_empty()),
            Some(&tape),
        );
        if stale {
            assert!(result.is_err());
            assert!(
                receipt.is_some(),
                "a stale suffix must return the already-paid receipt unchanged"
            );
            assert_eq!(*charged, paid);
            assert!(tape.steps().is_empty());
        } else {
            result.unwrap();
            assert!(
                receipt.is_none(),
                "native completion consumes its paid original prefix"
            );
        }
    }
    assert_eq!(
        *charged,
        7 + debt.units(),
        "native suffix settles each original event once"
    );
}
