//! FIG-7614's native boundary law runs inside helper_frames_agree.
use super::*;
use crate::machine::source_publication::native_storage::{NativeBoundary, NativeStorage};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::logical::{Ledger, Owner, ScratchEvent, Slot};
use lash_kernel_value::sites::{Site, Unit};
use std::{cell::RefCell, rc::Rc};

impl KernelMachine {
    /// A speculative miss must return every slot before precise execution
    /// retries the same canonical native, including an allocated BigInt.
    pub(crate) fn assert_abandoned_native_slots(&self, id: lash_kernel_value::FunctionId) {
        let function = self.exe.lib(self.exe.library_lib(&id).unwrap());
        let LibRun::Native { native, .. } = &function.run else {
            panic!("installed native");
        };
        let ledger = Rc::new(RefCell::new(Ledger::fresh()));
        let mut arena = Arena::new();
        let arguments = [doc::Value::Int(i64::MAX), doc::Value::Int(7)];
        let expected = {
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let value = nursery.allocate(doc::NewObject::List(vec![]));
            nursery.discard();
            value.handle().unwrap()
        };
        let before = ledger.borrow().balance().unwrap();
        for _ in 0..2 {
            let contract = native
                .resource_contract(doc::Args::new(
                    &arguments,
                    arena.objects(),
                    &arena.semantics(),
                ))
                .unwrap()
                .unwrap();
            let abandoned = NativeStorage {
                contract,
                function,
                args: arguments.to_vec(),
                output: Owner::Value {
                    task: doc::TaskId::MAIN,
                    invocation: 1,
                    slot: Slot::Operand(0),
                },
                at: Point {
                    site: Site::new(Unit::Main, vec![0]),
                    phase: Phase::Commit,
                },
                release: vec![].into_boxed_slice(),
                ledger: Rc::clone(&ledger),
                heap: &self.heap,
                charging: true,
                limits: self.limits,
                room: None,
                known: None,
            }
            .prepare_boundary(&mut arena, &[], |_, _, _, _| Ok(None::<()>))
            .unwrap();
            assert!(abandoned.is_none());
            drop(abandoned);
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let next = nursery.allocate(doc::NewObject::List(vec![]));
            assert_eq!(
                next.handle(),
                Some(expected),
                "a declined native must return its nursery slots before handoff"
            );
            nursery.discard();
            assert_eq!(ledger.borrow().balance().unwrap(), before);
        }
        let (result, built) = {
            let semantics = arena.semantics();
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let mut counter = doc::WorkCounter::new(None);
            let result = native
                .call(doc::NativeCall {
                    args: doc::Args::new(&arguments, arena.objects(), &semantics),
                    new: &mut nursery,
                    counter: &mut counter,
                })
                .unwrap();
            (result, nursery.into_new())
        };
        assert!(matches!(result, doc::Value::BigInt(_)));
        assert_eq!(result.handle(), Some(expected));
        arena.install(built).unwrap();
    }
    pub(crate) fn assert_native_boundary_law(&self, id: lash_kernel_value::FunctionId) {
        let function = self.exe.lib(self.exe.library_lib(&id).unwrap());
        let LibRun::Native { native, .. } = &function.run else {
            panic!("installed native")
        };
        let code = self.exe.main.unwrap();
        let statement = self.exe.block(self.exe.code(code).body).stmts[0];
        let (stream, stmt, _) = self.exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    lash_kernel_exec::Op::Call { .. } | lash_kernel_exec::Op::Operator { .. }
                )
            })
            .unwrap();
        let charges =
            room::charges::ChargeProgram::native_completion(&self.exe, code, statement, pc)
                .unwrap();
        let ledger = Rc::new(RefCell::new(Ledger::fresh()));
        let mut arena = Arena::new();
        let owner = |index| Owner::Value {
            task: doc::TaskId::MAIN,
            invocation: 1,
            slot: Slot::Operand(index),
        };
        let input = doc::Value::Int(0);
        let initial = ledger
            .borrow()
            .prepare_source_boundary(
                arena.registration().unwrap(),
                &self.heap,
                &[],
                &[(
                    owner(0),
                    lash_kernel_state::kernel_internals::logical::RootValue::Value(&input),
                )],
            )
            .unwrap()
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("input fits"));
        initial
            .publish(&mut ledger.borrow_mut(), |_, _| {})
            .unwrap();
        let at = Point {
            site: Site::new(Unit::Main, vec![0]),
            phase: Phase::Commit,
        };
        let recipe = |mode, contract| NativeStorage {
            contract,
            function,
            args: vec![doc::Value::Int(mode)],
            output: owner(1),
            at: at.clone(),
            release: vec![].into_boxed_slice(),
            ledger: Rc::clone(&ledger),
            heap: &self.heap,
            charging: true,
            limits: self.limits,
            room: None,
            known: None,
        };
        let contract = |mode, arena: &Arena| {
            native
                .resource_contract(doc::Args::new(
                    &[doc::Value::Int(mode)],
                    arena.objects(),
                    &arena.semantics(),
                ))
                .unwrap()
                .unwrap()
        };
        let mut wrong = recipe(0, contract(0, &arena));
        wrong.known = Some((doc::Value::Int(8), 0));
        assert!(
            wrong
                .prepare_for_law(&mut arena, &[owner(0)])
                .unwrap()
                .is_none(),
            "same-price native result must match its canonical prepared entry"
        );
        let mut exact = recipe(0, contract(0, &arena));
        exact.known = Some((doc::Value::Int(7), 0));
        let exact = exact
            .prepare_for_law(&mut arena, &[owner(0)])
            .unwrap()
            .unwrap();
        assert!(matches!(exact.storage.value(), doc::Value::Int(7)));
        drop(exact);
        let mut scratch = recipe(6, contract(6, &arena));
        scratch.known = Some((doc::Value::Int(7), 0));
        let scratch = scratch
            .prepare_for_law(&mut arena, &[owner(0)])
            .unwrap()
            .unwrap();
        assert_eq!(
            scratch.storage.work(),
            2,
            "a prepared entry cannot erase installed scratch phases"
        );
        assert_eq!(scratch.storage.contract().peak_scratch_bytes(), Some(40));
        drop(scratch);
        let balance = ledger.borrow().balance().unwrap();
        let identity = arena.next_provisional();
        let allocations = arena.allocations();
        // Changed result bytes, a dishonest graph and an unknown input contract
        // must leave both the ledger and the physical nursery untouched.
        assert!(
            recipe(2, contract(2, &arena))
                .prepare_for_law(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(3, contract(3, &arena))
                .prepare_for_law(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(4, contract(0, &arena))
                .prepare_for_law(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(5, contract(5, &arena))
                .prepare_for_law(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.borrow().balance().unwrap(), balance);
        assert_eq!(arena.next_provisional(), identity);
        assert_eq!(arena.allocations(), allocations);
        let scalar = recipe(0, contract(0, &arena))
            .prepare_for_law(&mut arena, &[owner(0)])
            .unwrap()
            .expect("scalar native ran");
        assert!(matches!(scalar.storage.value(), doc::Value::Int(7)));
        assert_eq!(native_units(&charges, &scalar.storage), 4);
        assert_eq!(scalar.transaction.peak(), 17);
        drop(scalar);
        // The graph call's actual nonzero input has a one-byte magnitude.
        let list_input = doc::Value::Int(1);
        let update = ledger
            .borrow()
            .prepare_source_boundary(
                arena.registration().unwrap(),
                &self.heap,
                &[owner(0)],
                &[(
                    owner(0),
                    lash_kernel_state::kernel_internals::logical::RootValue::Value(&list_input),
                )],
            )
            .unwrap()
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("nonzero input fits"));
        update.publish(&mut ledger.borrow_mut(), |_, _| {}).unwrap();
        let balance = ledger.borrow().balance().unwrap();
        let phases = contract(1, &arena);
        let events = native_storage::scratch_events(owner(1), &phases).unwrap();
        let scratch = |index| Owner::Value {
            task: doc::TaskId::MAIN,
            invocation: 1,
            slot: Slot::Scratch(index),
        };
        assert!(matches!(events.as_slice(), [
            ScratchEvent::Begin { owner: a, bytes: 40 }, ScratchEvent::End { owner: b },
            ScratchEvent::Begin { owner: c, bytes: 20 }, ScratchEvent::Begin { owner: d, bytes: 30 },
            ScratchEvent::End { owner: e }, ScratchEvent::End { owner: f },
        ] if *a == scratch(0) && *b == scratch(0) && *c == scratch(1)
            && *d == scratch(2) && *e == scratch(2) && *f == scratch(1)));
        assert_eq!(phases.peak_scratch_bytes(), Some(50));
        // 17-byte input + a 66-byte list + the 20-byte lease still live
        // during the entity allocation. The 30-byte lease ended beforehand.
        let peak = balance.total().unwrap() + 86;
        let NativeBoundary {
            transaction,
            storage,
        } = recipe(1, phases.clone())
            .prepare_for_law(&mut arena, &[owner(0)])
            .unwrap()
            .expect("actual native boundary executed");
        assert_eq!(transaction.peak(), peak);
        assert_eq!(storage.work(), 2);
        assert_eq!(native_units(&charges, &storage), 6);
        assert!(matches!(
            storage.formula_values().arguments,
            [doc::Value::Int(1)]
        ));
        assert_eq!(
            storage.measure(crate::compile::Source::Result, doc::Measure::Size),
            Some(3)
        );
        assert!(transaction.admit(peak - 1).is_err());
        drop(storage);
        assert_eq!(arena.next_provisional(), identity);
        assert_eq!(arena.allocations(), allocations);
        let NativeBoundary {
            transaction,
            storage,
        } = recipe(1, phases)
            .prepare_for_law(&mut arena, &[owner(0)])
            .unwrap()
            .expect("retry runs canonical native");
        let value = storage.value();
        transaction
            .admit(peak)
            .unwrap_or_else(|_| panic!("exact native peak fits"))
            .publish(&mut ledger.borrow_mut(), |arena, edits| {
                assert!(edits.is_empty());
                storage.publish(arena);
            })
            .unwrap();
        let doc::Value::List(handle) = value else {
            panic!("canonical list result")
        };
        assert_eq!(arena.len(handle).unwrap(), 2);
        assert_eq!(arena.next_provisional().ordinal, identity.ordinal + 1);
        assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), 82);
        assert!(
            ledger
                .borrow()
                .holding(owner(0), self.heap.pages())
                .unwrap()
                .is_none()
        );
        assert!(
            ledger
                .borrow()
                .holding(owner(1), self.heap.pages())
                .unwrap()
                .is_some()
        );
        println!(
            "native boundary ABI executed: changed result/body/unknown/refusal rejected; ordered scratch peak {peak}; 1 graph published"
        );
    }
}

impl KernelMachine {
    pub(crate) fn assert_external_native_boundary_law(&self, id: lash_kernel_value::FunctionId) {
        use crate::compile::room::charges::ChargeProgram;
        use crate::compile::room::publication::{ExactEvent, Successor};
        use crate::machine::source_publication::native_storage::SourceNative;
        use lash_kernel_exec::{Op, OpRange};
        let function = self.exe.lib(self.exe.library_lib(&id).unwrap());
        let LibRun::Native { native, .. } = &function.run else {
            panic!("installed native")
        };
        let code = self.exe.main.unwrap();
        let source = &self.exe.code(code).source_room;
        let statement = self.exe.block(self.exe.code(code).body).stmts[0];
        let (stream, stmt, _) = self.exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    Op::Call { .. } | Op::Operator { .. }
                )
            })
            .unwrap();
        let charges = ChargeProgram::compile(
            &self.exe,
            code,
            statement,
            OpRange {
                start: stmt.ops.start,
                len: pc + 1 - stmt.ops.start,
            },
        )
        .unwrap();
        let at = Point {
            site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
            phase: Phase::Commit,
        };
        assert_eq!(
            charges.first(),
            &at,
            "the no-argument call is the genuine unpaid head"
        );
        let ledger = self.heap.logical_ledger();
        let mut arena = Arena::new();
        let (invocation, enter) = ledger
            .borrow()
            .prepare_enter(doc::TaskId::MAIN, &mut arena, self.heap.pages(), &[], &[])
            .unwrap();
        ledger
            .borrow_mut()
            .commit(enter.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let output = source
            .owner_slots()
            .unwrap()
            .root(
                &room::Owner::Operand(at.site.clone()),
                doc::TaskId::MAIN,
                invocation.id(),
            )
            .unwrap();
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: [3; 32],
            occurrence: Occurrence {
                task: doc::TaskId::MAIN,
                invocation: invocation.id(),
                loops: vec![],
                action: None,
            },
        };
        let next = Position {
            unpaid: Some(source.next_after_commit(&at.site).unwrap()),
            ..start.clone()
        };
        let link = *self.exe.instances.keys().next().unwrap();
        let mut frontier = source
            .frontier(
                &ledger.borrow(),
                &self.heap,
                &invocation,
                link,
                doc::KernelVersion::One,
                start.clone(),
            )
            .unwrap();
        let interval = source.compile_event(&at, ExitKind::Normal).unwrap();
        let event = ExactEvent::compile(source, Arc::clone(&interval), Successor::Normal).unwrap();
        let contract = native
            .resource_contract(doc::Args::new(
                &[doc::Value::Absent],
                arena.objects(),
                &arena.semantics(),
            ))
            .unwrap()
            .unwrap();
        let before = ledger.borrow().balance().unwrap().total().unwrap();
        let identity = arena.next_provisional();
        let allocations = arena.allocations();
        let recipe = || NativeStorage {
            contract: contract.clone(),
            function,
            args: vec![doc::Value::Absent],
            output,
            at: at.clone(),
            release: vec![].into(),
            ledger: Rc::clone(&ledger),
            heap: &self.heap,
            charging: true,
            limits: self.limits,
            room: None,
            known: None,
        };
        let prefix = || Prefix {
            interval: Arc::clone(&interval),
            facts: LeafExecution([(at.clone(), LeafFacts::default())].into()),
            start: start.clone(),
            next: next.clone(),
            allowance: Allowance {
                occurrences: vec![],
                before: 0,
                units: 0,
                close: None,
                strict: true,
                depth: u64::from(function.operation()),
                safe_point: 0,
            },
        };
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        // Source role, omitted-argument value and ordered scratch checks are
        // independent of the price and body checks in native preparation.
        for guard in 0..3 {
            let mut prefix = prefix();
            let facts = external_resources(&mut charged, &mut counters, u64::MAX)
                .facts(&prefix.allowance)
                .unwrap();
            let prepared = recipe().prepare_boundary(&mut arena, &[], |arena, storage, page, entities| {
                use room::publication::{ExactRoots, ExternalFacts};
                storage.bind_facts(&mut prefix.facts)?;
                let debt = charges.bind_measured(&[storage.formula_values()], |_, source, measure| storage.measure(source, measure)).unwrap();
                prefix.allowance.units = debt.units();
                let mut scratch = native_storage::scratch_events(output, storage.contract())?;
                if guard == 2 {
                    let len = scratch.len();
                    scratch.swap(len - 1, len - 2);
                }
                let wrong = [doc::Value::Null];
                let arguments = if guard == 0 { &wrong[..] } else { storage.arguments() };
                let wrong_owner = invocation.owner(Slot::Operand(u32::MAX));
                let bound = event.bind_external(&frontier, &ledger.borrow(), arena, &self.heap, &mut prefix.facts,
                    ExactRoots { inputs: vec![], outputs: vec![(if guard == 1 { wrong_owner } else { output }, lash_kernel_state::kernel_internals::logical::PublicationValue::Prepared(page.clone()))], ..Default::default() },
                    prefix.next, prefix.allowance, facts,
                    ExternalFacts { operands: &[], arguments, member: None, entities, scratch: &scratch, native: Some(storage.contract()) }, &debt);
                assert!(bound.is_err(), "external guard {guard} must reject the changed same-price fact");
                Ok(Some(()))
            }).unwrap();
            drop(prepared);
            assert_eq!(arena.next_provisional(), identity);
            assert_eq!(arena.allocations(), allocations);
            assert_eq!(frontier.position(), &start);
        }
        let expected_peak = before + contract.peak_extra_bytes().unwrap();
        // The charge and memory owners refuse independently. Neither refusal
        // installs a graph, spends a provisional identity or changes the head.
        let mut units = 0;
        for (bound, memory) in [(0, u64::MAX), (u64::MAX, expected_peak - 1)] {
            let (prepared, storage) = {
                let prefix = prefix();
                let facts = external_resources(&mut charged, &mut counters, bound)
                    .facts(&prefix.allowance)
                    .unwrap();
                SourceNative {
                    recipe: recipe(),
                    charges: &charges,
                    inputs: vec![],
                    operands: vec![],
                }
                .prepare(
                    &mut arena,
                    ExternalEntry {
                        event: &event,
                        frontier: &frontier,
                        ledger: &ledger.borrow(),
                        heap: &self.heap,
                        prefix,
                        resources: facts,
                    },
                )
            }
            .unwrap()
            .expect("certified external native prepared");
            assert_eq!(prepared.peak(), expected_peak);
            units = charges
                .bind_measured(&[storage.storage.formula_values()], |_, source, measure| {
                    storage.storage.measure(source, measure)
                })
                .unwrap()
                .units();
            assert!(
                prepared
                    .admit(
                        &frontier,
                        &ledger.borrow(),
                        external_resources(&mut charged, &mut counters, bound),
                        memory
                    )
                    .unwrap()
                    .is_none()
            );
            drop(storage);
            assert_eq!(charged, 0);
            assert_eq!(arena.next_provisional(), identity);
            assert_eq!(arena.allocations(), allocations);
            assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), before);
            assert_eq!(frontier.position(), &start);
        }
        assert!(units > 0);
        let (prepared, prepared_native) = {
            let prefix = prefix();
            let facts = external_resources(&mut charged, &mut counters, units)
                .facts(&prefix.allowance)
                .unwrap();
            SourceNative {
                recipe: recipe(),
                charges: &charges,
                inputs: vec![],
                operands: vec![],
            }
            .prepare(
                &mut arena,
                ExternalEntry {
                    event: &event,
                    frontier: &frontier,
                    ledger: &ledger.borrow(),
                    heap: &self.heap,
                    prefix,
                    resources: facts,
                },
            )
        }
        .unwrap()
        .expect("actual external native certificate");
        let native_storage::NativePublication { storage, .. } = prepared_native;
        assert!(
            storage.formula_values().arguments.is_empty(),
            "omitted optional arguments are padded only for execution"
        );
        assert!(matches!(storage.arguments(), [doc::Value::Absent]));
        let value = storage.value();
        assert_eq!(
            units,
            charges
                .bind_measured(&[storage.formula_values()], |_, source, measure| storage
                    .measure(source, measure))
                .unwrap()
                .units()
        );
        let accepted = prepared
            .admit(
                &frontier,
                &ledger.borrow(),
                external_resources(&mut charged, &mut counters, units),
                expected_peak,
            )
            .unwrap()
            .unwrap();
        accepted
            .publish_event(&mut frontier, &mut ledger.borrow_mut(), |arena, edits| {
                assert!(edits.is_empty());
                storage.publish(arena);
            })
            .unwrap();
        assert_eq!(charged, units);
        assert_eq!(frontier.position(), &next);
        assert_eq!(arena.next_provisional().ordinal, identity.ordinal + 1);
        let doc::Value::List(handle) = value else {
            panic!("canonical graph result")
        };
        assert_eq!(arena.len(handle).unwrap(), 2);
        assert_eq!(
            ledger.borrow().balance().unwrap().total().unwrap(),
            before + contract.retained_bytes().unwrap()
        );
        println!(
            "external exact native certificate published 1 graph; optional padding, canonical {units}-unit debt and {expected_peak}-byte peak checked"
        );
    }
}

fn external_resources<'a>(
    charged: &'a mut u64,
    counters: &'a mut crate::machine::tasks::Occurrences,
    bound: u64,
) -> SourceResources<'a> {
    SourceResources {
        charged,
        counters,
        bound,
        slice: u64::MAX,
        depth: 1,
        limit: 10,
        safe_point: 0,
    }
}

impl NativeStorage<'_> {
    fn prepare_for_law<'a>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
    ) -> Result<Option<NativeBoundary<'a>>, HeapFault> {
        Ok(self
            .prepare_boundary(arena, inputs, |_, _, _, _| Ok(Some(())))?
            .map(|(boundary, ())| boundary))
    }
}

fn native_units(
    charges: &room::charges::ChargeProgram,
    storage: &native_storage::PreparedNative,
) -> u64 {
    charges
        .bind_measured(&[storage.formula_values()], |_, source, measure| {
            storage.measure(source, measure)
        })
        .unwrap()
        .units()
}
