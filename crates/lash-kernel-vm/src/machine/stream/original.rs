//! Paid constructor prefixes return to the existing precise operation owner.
use super::*;
use crate::compile::room::charges::AcceptedOriginalPrefix;
use lash_kernel_exec::{Weight, charge::RegionEvent};
use lash_kernel_state::kernel_internals::logical::PublicationValue;

impl KernelMachine {
    /// This handoff is synchronous. The receipt and its physical placement are
    /// consumed before the statement can reach a durable scheduling boundary.
    pub(in crate::machine) fn run_original_prefix(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        prefix: AcceptedOriginalPrefix,
    ) -> Eval<Ran> {
        #[cfg(any(test, feature = "perf-witness"))]
        let _execution = execution::Scope::enter(
            &self.islands.census,
            execution::Owner::SourceEvaluator,
            self.islands.counting,
        );
        if self.region.is_some() {
            return Err(fault("original prefix entered an open source region").into());
        }
        let (code, invocation) = {
            let frame = self.frame(running.task)?;
            (frame.code, frame.invocation)
        };
        let source = &running.exe.code(code).source_room;
        let store = self.logical.clone();
        let replay = prefix
            .into_replay(
                source,
                &store.borrow(),
                code,
                running.id,
                running.task,
                invocation,
                self.charged,
            )
            .map_err(|_| fault("original prefix has another source continuation"))?;
        let roots = self
            .frame(running.task)?
            .logical
            .as_ref()
            .ok_or_else(|| fault("original source handoff has no frame ownership"))?
            .bank();
        self.restore_original_bank(running, replay.next, invocation, roots, replay.roles)?;
        self.source_begin(running.task, running.stream, running.stmt, replay.next)?;
        let mut pc = replay.next;
        for (at, event) in replay.events {
            self.run_original_operations(running, &mut pc, at)?;
            self.charge_original_event(event)?;
        }
        self.run_original_operations(running, &mut pc, replay.stop)?;
        self.run_stream_entered(host, running, replay.stop as usize)
    }

    fn restore_original_bank(
        &mut self,
        running: &Running<'_>,
        pc: u32,
        invocation: u64,
        roots: crate::machine::logical_held::BankRoots,
        roles: Box<[(u32, lash_kernel_value::sites::Site, PublicationValue)]>,
    ) -> Eval<()> {
        use crate::machine::registers::{ResumeAt, ResumeValue};
        let mut values = Vec::with_capacity(roles.len());
        let store = self.logical.clone();
        for (reg, site, value) in roles {
            let owner = roots.operand(&site)?;
            let holding = store
                .borrow()
                .holding(owner, self.heap.pages())?
                .ok_or_else(|| fault("original source role lost its accepted owner"))?;
            let value = match value {
                PublicationValue::Native(value) => value,
                PublicationValue::Prepared(value) => value.restore_value(&self.arena.borrow())?,
                PublicationValue::Page(value) => super::super::project::restore_source(
                    &mut self.arena.borrow_mut(),
                    self.heap.pages(),
                    &value,
                )?,
                _ => return Err(fault("original prefix has another value provenance").into()),
            };
            values.push(ResumeValue {
                reg: Reg(reg),
                site: Arc::new(site),
                held: crate::machine::logical_held::Held {
                    value,
                    holding,
                    owner: Some(owner),
                },
            });
        }
        let current = store
            .borrow()
            .invocation(running.task, invocation, self.heap.pages())?;
        self.regs
            .discard_replay_source_bank(&store.borrow(), &self.heap, &current)?;
        let bank = self
            .regs
            .prepare_source_resume(
                ResumeAt {
                    task: running.task,
                    invocation,
                    statement: running.stmt.site.clone(),
                    pc,
                },
                roots,
                running.stmt.regs,
                values,
            )?
            .ok_or_else(|| fault("original source bank changed"))?;
        bank.publish();
        self.regs.bind_source_resume(&store.borrow());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn check_original_recovery_bank(
        &mut self,
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        invocation: &lash_kernel_state::kernel_internals::logical::Invocation,
        replay: crate::compile::room::charges::OriginalReplay,
    ) {
        let roots = crate::machine::logical_held::FrameRoots::prepared(
            self.logical.clone(),
            TaskId::MAIN,
            invocation.clone(),
            Arc::clone(&exe.code(code).source_room),
            Arc::clone(&exe.code(code).slots_by_at),
        )
        .unwrap()
        .bank();
        let running = Running::new(TaskId::MAIN, exe, statement);
        let store = self.logical.clone();
        let stamp = store.borrow().stamp();
        let before = store.borrow().balance().unwrap();
        self.restore_original_bank(&running, replay.next, invocation.id(), roots, replay.roles)
            .unwrap();
        assert!(
            self.regs
                .begin_source_resume(
                    TaskId::MAIN,
                    invocation.id(),
                    &running.stmt.site,
                    replay.next,
                    &store.borrow()
                )
                .unwrap()
        );
        assert!(
            store.borrow().is_current(&stamp),
            "saved roles must be adopted without root publication"
        );
        assert_eq!(store.borrow().balance().unwrap(), before);
    }

    fn run_original_operations(
        &mut self,
        running: &Running<'_>,
        pc: &mut u32,
        stop: u32,
    ) -> Eval<()> {
        for at in *pc..stop {
            let op = running.stream.ops[at as usize];
            if !op.kind().in_region() || matches!(op, Op::Region { .. }) {
                return Err(fault("original suffix crossed an uncertified boundary").into());
            }
            if self.in_region(running, at)? {
                self.source_complete_joins(running, at)?;
            } else {
                self.source_complete(running, at)?;
            }
        }
        *pc = stop;
        Ok(())
    }

    fn charge_original_event(&mut self, event: RegionEvent) -> Eval<()> {
        if !self.charging {
            self.observe_region(&[event]);
            return Ok(());
        }
        let units = match event.weight {
            Weight::Units(units) | Weight::Atomic(units) => units,
        };
        let attempts = match event.weight {
            Weight::Units(_) => units,
            Weight::Atomic(_) => 1,
        };
        for unit in 0..attempts {
            self.charged = self.charged.saturating_add(match event.weight {
                Weight::Units(_) => 1,
                Weight::Atomic(_) => units,
            });
            if self.charged > self.bounds.charge_limit {
                let mut limit = self.bounds.charge_limit;
                if let Err(refused) = self.limiters.charge_reached(self.charged, &mut limit) {
                    self.observe_crossing(event, unit);
                    return Err(refused.into());
                }
                self.bounds.charge_limit = limit;
            }
        }
        self.observe_region(&[event]);
        Ok(())
    }
}

#[cfg(test)]
mod prepared_proof_laws {
    use super::*;
    use crate::admission::{Allowance, SourceResources};
    use crate::compile::room::{
        self, Phase, Point,
        publication::{ExactEvent, Occurrence, Position, Successor},
    };
    use crate::compile::{expr::ConstructorPrefix, room::charges::OriginalPrefix};
    use crate::machine::source_publication::{
        ExternalEntry, LeafExecution, LeafFacts, Prefix,
        construction::storage::{Construction, ConstructionValue},
    };
    use lash_kernel_doc as doc;
    use lash_kernel_state::kernel_internals::logical::Ledger;

    #[test]
    fn accepted_constructor_receipts_restore_cold_values_and_refuse_uninstalled_proofs() {
        let mut failures = Vec::new();
        for cold in [false, true] {
            for guard in 0..5 {
                let mut run = crate::laws::embedder::Embedder::new("main { return ([], 2) }");
                if cold {
                    run.reopen(crate::Layout::default(), true);
                }
                let machine = &mut run.machine;
                let exe = Arc::clone(&machine.exe);
                let code = exe.main.unwrap();
                let statement = exe.block(exe.code(code).body).stmts[0];
                let (stream, stmt, _) = exe.statement(statement);
                let pc = (stmt.ops.start..stmt.ops.end()).find(|pc| matches!(stream.ops[*pc as usize], Op::MakeList { items } if items.len == 0)).unwrap();
                let source = &exe.code(code).source_room;
                let at = Point {
                    site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
                    phase: Phase::Commit,
                };
                let store = machine.logical.clone();
                *store.borrow_mut() = Ledger::fresh();
                let arena_store = machine.arena.clone();
                let mut arena = arena_store.borrow_mut();
                let (invocation, entered) = store
                    .borrow()
                    .prepare_enter(TaskId::MAIN, &mut arena, machine.heap.pages(), &[], &[])
                    .unwrap();
                store
                    .borrow_mut()
                    .commit(entered.admit(u64::MAX).ok().unwrap())
                    .unwrap();
                let output = source
                    .owner_slots()
                    .unwrap()
                    .root(
                        &room::Owner::Operand(at.site.clone()),
                        TaskId::MAIN,
                        invocation.id(),
                    )
                    .unwrap();
                let start = Position {
                    at: at.clone(),
                    unpaid: Some(at.clone()),
                    recovery: [7; 32],
                    occurrence: Occurrence {
                        task: TaskId::MAIN,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                };
                let next = Position {
                    unpaid: Some(source.next_after_commit(&at.site).unwrap()),
                    ..start.clone()
                };
                let link = *exe.instances.keys().next().unwrap();
                let mut frontier = source
                    .frontier(
                        &store.borrow(),
                        &machine.heap,
                        &invocation,
                        link,
                        doc::KernelVersion::One,
                        start.clone(),
                    )
                    .unwrap();
                let interval = source.compile_event(&at, room::ExitKind::Normal).unwrap();
                let exact =
                    ExactEvent::compile(source, Arc::clone(&interval), Successor::Normal).unwrap();
                let compiled = ConstructorPrefix::compile(stream, stmt.ops, pc).unwrap();
                let original = OriginalPrefix::compile(&exe, code, statement, &compiled).unwrap();
                let registers = vec![Value::Absent; stmt.regs as usize];
                let mut charged = 7;
                let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
                let need = Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point: charged,
                };
                let resource_facts = recovery_resources(&mut charged, &mut counters)
                    .facts(&need)
                    .unwrap();
                let (prepared, storage) = Construction {
                    charges: None,
                    value: ConstructionValue::Collection(doc::NewObject::List(vec![])),
                    output,
                    at: at.clone(),
                    release: vec![].into(),
                    inputs: vec![],
                    operands: vec![],
                    depth_limit: 128,
                    heap: &machine.heap,
                }
                .prepare_original(
                    &mut arena,
                    ExternalEntry {
                        event: &exact,
                        frontier: &frontier,
                        ledger: &store.borrow(),
                        heap: &machine.heap,
                        prefix: Prefix {
                            interval,
                            facts: LeafExecution([(at, LeafFacts::default())].into()),
                            start,
                            next,
                            allowance: need,
                        },
                        resources: resource_facts,
                    },
                    (&original, &registers, None),
                )
                .unwrap()
                .unwrap();
                let expected = storage.value;
                let proof = storage.batch.graph_proof().unwrap().value(0);
                let accepted = prepared
                    .admit(
                        &frontier,
                        &store.borrow(),
                        recovery_resources(&mut charged, &mut counters),
                        u64::MAX,
                    )
                    .unwrap()
                    .unwrap();
                let mut receipt = None;
                accepted
                    .publish_original_event(
                        &mut frontier,
                        &mut store.borrow_mut(),
                        &mut receipt,
                        |arena, edits| {
                            assert!(edits.is_empty());
                            storage.batch.publish(arena);
                        },
                        None,
                    )
                    .unwrap();
                let mut chilled = None;
                if cold && guard == 0 {
                    arena.identify(expected.handle().unwrap()).unwrap();
                    store
                        .borrow_mut()
                        .rekey(&arena.take_rekeyed(), machine.heap.pages())
                        .unwrap();
                    chilled = arena.chill(expected.handle().unwrap()).unwrap();
                    assert!(chilled.is_some());
                    assert!(arena.needs_decode(expected.handle().unwrap()).unwrap());
                }
                let mut replay = receipt
                    .unwrap()
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
                let index = replay.roles.iter().position(|(_,_,value)| matches!(value, PublicationValue::Prepared(value) if value.same(&proof))).expect("the accepted receipt retains this constructor proof");
                if guard == 1 {
                    let mut installation = arena.prepare_install(vec![]).unwrap();
                    let unpublished = installation
                        .prove_values(machine.heap.pages(), &[expected], 128, false)
                        .unwrap()
                        .value(0);
                    if unpublished
                        .restore_value(installation.into_parts().0.arena())
                        .is_ok()
                    {
                        failures.push((cold, 10));
                    }
                    replay.roles[index].2 = PublicationValue::Prepared(unpublished);
                } else if guard == 2 {
                    let graph = crate::machine::source_publication::construction::boundary::prepare_collection(&mut arena, &machine.heap, doc::NewObject::List(vec![]), output, 128).unwrap().unwrap();
                    let rolled_back = graph.outputs[0].proof.clone();
                    drop(graph);
                    replay.roles[index].2 = PublicationValue::Prepared(rolled_back);
                } else if guard == 3 {
                    arena
                        .list_set(expected.handle().unwrap(), 0, Value::Int(9))
                        .unwrap();
                } else if guard == 4 {
                    arena.release(&mut |_| {});
                    arena.create_list(&[Value::Int(9)]);
                }
                let before = (
                    store.borrow().balance().unwrap(),
                    charged,
                    arena.allocations(),
                    arena.next_provisional(),
                );
                drop(arena);
                let roots = crate::machine::logical_held::FrameRoots::prepared(
                    store.clone(),
                    TaskId::MAIN,
                    invocation.clone(),
                    Arc::clone(source),
                    Arc::clone(&exe.code(code).slots_by_at),
                )
                .unwrap()
                .bank();
                let running = Running::new(TaskId::MAIN, &exe, statement);
                let reg = replay.roles[index].0 as usize;
                let restored = machine.restore_original_bank(
                    &running,
                    replay.next,
                    invocation.id(),
                    roots,
                    replay.roles,
                );
                if guard == 0 {
                    restored.unwrap();
                    assert!(
                        machine.regs.get(reg).unwrap().same(&expected),
                        "recovery must adopt the exact installed value"
                    );
                    assert!(
                        machine
                            .regs
                            .begin_source_resume(
                                TaskId::MAIN,
                                invocation.id(),
                                &running.stmt.site,
                                replay.next,
                                &store.borrow()
                            )
                            .unwrap()
                    );
                    if let Some(chilled) = chilled {
                        assert!(
                            arena_store
                                .borrow()
                                .needs_decode(expected.handle().unwrap())
                                .unwrap(),
                            "restoring the receipt does not warm a cold value"
                        );
                        arena_store
                            .borrow_mut()
                            .thaw(expected.handle().unwrap(), chilled)
                            .unwrap();
                    }
                } else if restored.is_ok() {
                    failures.push((cold, guard));
                }
                assert_eq!(
                    (
                        store.borrow().balance().unwrap(),
                        charged,
                        arena_store.borrow().allocations(),
                        arena_store.borrow().next_provisional()
                    ),
                    before,
                    "recovery publishes no roots, grants, storage or charges"
                );
            }
        }
        assert!(
            failures.is_empty(),
            "prepared recovery accepted invalid proofs: {failures:?}"
        );
    }
    fn recovery_resources<'a>(
        charged: &'a mut u64,
        counters: &'a mut crate::machine::tasks::Occurrences,
    ) -> SourceResources<'a> {
        let safe_point = *charged;
        SourceResources {
            charged,
            counters,
            bound: u64::MAX,
            slice: u64::MAX,
            depth: 0,
            limit: 10,
            safe_point,
        }
    }
}
