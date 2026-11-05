use super::super::*;
use crate::compile::region_isa::{compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;
use crate::laws::embedder::{Embedder, Setup};
use lash_kernel_doc::TaskId;

fn call_machine() -> Embedder {
    call_expression("return isa.pick(7, 9)")
}

fn call_expression(body: &str) -> Embedder {
    fn probes(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
    ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
        let definition = lash_kernel_doc::parse_definition(
            "function isa.pick(a: Any, b: Any, c?: Any) -> Any\nkernel 1\ncharge 1\nnative\nbody { return b }",
        )
        .unwrap();
        let pick = registry.register(definition, None).unwrap();
        let measured = lash_kernel_doc::parse_definition(
            "function isa.measured(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, deep(a), size(result))\nnative\nbody { return b }",
        )
        .unwrap();
        vec![
            ("isa.pick", pick),
            ("isa.measured", registry.register(measured, None).unwrap()),
        ]
    }
    let mut setup = Setup {
        probes: Some(probes),
        ..Setup::default()
    };
    setup.start.target = crate::Target::Entry("go".into());
    let definition =
        format!("function go() -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}");
    let source = format!(
        "definition {}\nmain {{}}",
        serde_json::to_string(&definition).unwrap()
    );
    Embedder::with(&source, setup)
}

/// K-FN-004/K-EVAL-008: C reads actual ordered caller slots and pads missing
/// formals without inventing a holding or consuming an invocation identity.
#[test]
fn callee_inputs_preserve_order_absence_and_reserved_identity() {
    let mut run = call_machine();
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let code = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, code).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, code, draft, &calls).unwrap();
    let stamp = machine.logical.borrow().stamp();
    let charged = machine.charged;
    let memory = machine.logical_memory.get();
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    for _ in 0..2 {
        assert_eq!(
            lease.run(StopAt::NextLegalPrefix).unwrap(),
            RegionStop::Handoff
        );
    }
    let Executing::Machine(execution) = &mut lease.execution else {
        unreachable!()
    };
    let crate::compile::region_isa::RegionOp::Call { site, .. } =
        execution.code.program.op(execution.pc).unwrap()
    else {
        panic!("the evaluated source operands must reach their call")
    };
    let call = execution.code.call(*site).unwrap();
    let plan = call.plan.static_target().unwrap();
    let target = execution
        .programs
        .target(code, site.index(), plan.code())
        .unwrap();
    target
        .inputs(
            &execution.registers,
            0,
            execution.reached.invocation,
            &mut execution.view,
            &mut execution.call_values,
        )
        .unwrap();
    let actual: Vec<_> = execution
        .call_values
        .iter()
        .map(|value| execution.view.scalar(*value).unwrap())
        .collect();
    assert_eq!(actual.len(), 3);
    for (actual, expected) in actual
        .iter()
        .zip([Value::Int(7), Value::Int(9), Value::Absent])
    {
        assert!(actual.same(&expected));
    }
    let next = execution.invocations.prepare().unwrap();
    let identity = execution.invocations.next_identity(next).unwrap();
    assert!(identity.id() > execution.reached.invocation);
    assert_eq!(
        identity.frame(),
        lash_kernel_state::kernel_internals::logical::Owner::Frame {
            task: TaskId::MAIN,
            invocation: next.id,
        }
    );
    assert_eq!(execution.invocations.prepare().unwrap().id, next.id);
    drop(lease);
    assert!(machine.logical.borrow().is_current(&stamp));
    assert_eq!(
        (machine.charged, machine.logical_memory.get()),
        (charged, memory)
    );
}

/// K-EVAL-008: committing one evaluated operand must preserve it across a
/// subsequent lease acquisition, without reevaluating its producer.
#[test]
fn a_committed_source_operand_survives_reacquisition_and_binding_transfer() {
    let mut setup = Setup::default();
    setup.start.target = crate::Target::Entry("go".into());
    let mut run = Embedder::with(
        r#"definition "function go() -> Any\nkernel 1\nmetering statements\nbody { let x = 7 return x }"
main {}"#,
        setup,
    );
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let code = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, code).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, code, draft, &calls).unwrap();
    let before = (
        machine.logical.borrow().stamp(),
        machine.logical_memory.get(),
        machine.charged,
    );
    let mut lease = RegionLease::acquire(machine, Arc::clone(&program)).unwrap();
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).unwrap(),
        RegionStop::Handoff
    );
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).unwrap(),
        RegionStop::Handoff
    );
    let prefix = lease.materialize().unwrap();
    let image = prefix.reconstruction();
    assert!(image.frames[0].roles.iter().any(|(slot, value)| matches!(
        slot,
        lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
    ) && value.same(&Value::Int(7))));
    let prepared = prefix.prepare_exit().unwrap();
    drop(prepared);
    assert!(machine.logical.borrow().is_current(&before.0));
    assert_eq!(
        (machine.logical_memory.get(), machine.charged),
        (before.1, before.2)
    );

    let mut lease = RegionLease::acquire(machine, Arc::clone(&program)).unwrap();
    lease.run(StopAt::NextLegalPrefix).unwrap();
    lease
        .materialize()
        .unwrap()
        .prepare_exit()
        .unwrap()
        .commit()
        .unwrap();
    assert!(
        machine
            .regs
            .region_values()
            .any(|(_, value)| value.same(&Value::Int(7)))
    );
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).unwrap(),
        RegionStop::Handoff
    );
    let prefix = lease.materialize().unwrap();
    let image = prefix.reconstruction();
    assert!(image.frames[0].roles.iter().any(|(slot, value)| matches!(
        slot,
        lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
    ) && value.same(&Value::Int(7))));
    prefix.prepare_exit().unwrap().commit().unwrap();
    assert!(
        machine
            .frame(TaskId::MAIN)
            .unwrap()
            .slot_states()
            .iter()
            .any(|slot| matches!(slot, crate::machine::SlotState::Value(Value::Int(7))))
    );
}
/// K-EVAL-008/K-FORM-013: publication within a loop retains its active pass,
/// and resumption pays the next loop test once without restarting the loop.
#[test]
fn a_while_prefix_reconstructs_its_active_pass_and_unpaid_test() {
    fn machine() -> (Embedder, Arc<RegionCode>) {
        let mut setup = Setup::default();
        setup.start.target = crate::Target::Entry("go".into());
        let mut run = Embedder::with(
            r#"definition "function go() -> Any\nkernel 1\nmetering statements\nbody { while true { let x = 7 } return null }"
main {}"#,
            setup,
        );
        run.machine.current = Some(TaskId::MAIN);
        let code = run.machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&run.machine.exe, code).unwrap();
        let calls = compile_calls(&run.machine.exe, &draft).unwrap();
        let program = verify_region(&run.machine.exe, code, draft, &calls).unwrap();
        (run, program)
    }
    fn binding(lease: &RegionLease<'_>) -> bool {
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        execution
            .code
            .program
            .recovery(execution.prefix)
            .unwrap()
            .roles
            .iter()
            .any(|(slot, _)| {
                matches!(
                    slot,
                    lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
                )
            })
    }
    fn next_binding(lease: &mut RegionLease<'_>) {
        let mut absent = !binding(lease);
        for _ in 0..64 {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
            if binding(lease) {
                if absent {
                    return;
                }
            } else {
                absent = true;
            }
        }
        panic!("the source loop did not reach its next binding");
    }
    fn pass(image: &Reconstruction, expected: u64) {
        let canonical = image.canonical.as_ref().unwrap();
        assert!(canonical.frames[0].control.iter().any(|control|
            matches!(control, crate::machine::Control::While { started, .. } if *started == expected)));
        assert_eq!(
            canonical.frames[0]
                .source_frontier
                .as_ref()
                .unwrap()
                .position()
                .occurrence
                .loops[0]
                .1,
            expected - 1
        );
        assert!(image.frames[0].roles.iter().any(|(slot, value)| matches!(
            slot,
            lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
        ) && value.same(&Value::Int(7))));
    }
    let (mut split, program) = machine();
    let mut lease = RegionLease::acquire(&mut split.machine, Arc::clone(&program)).unwrap();
    next_binding(&mut lease);
    let prefix = lease.materialize().unwrap();
    pass(prefix.reconstruction(), 1);
    prefix.prepare_exit().unwrap().commit().unwrap();

    let (mut uninterrupted, complete_program) = machine();
    let mut lease = RegionLease::acquire(&mut uninterrupted.machine, complete_program).unwrap();
    next_binding(&mut lease);
    next_binding(&mut lease);
    let complete = lease.materialize().unwrap();
    pass(complete.reconstruction(), 2);
    let expected = complete.reconstruction().counters;

    let mut lease = RegionLease::acquire(&mut split.machine, program).unwrap();
    next_binding(&mut lease);
    let prefix = lease.materialize().unwrap();
    pass(prefix.reconstruction(), 2);
    let actual = prefix.reconstruction().counters;
    assert_eq!(
        (actual.charge, actual.balance, actual.peak, actual.depth),
        (
            expected.charge,
            expected.balance,
            expected.peak,
            expected.depth
        )
    );
    prefix.prepare_exit().unwrap().commit().unwrap();
}

/// K-FN-004/K-LIM-002: frame admission moves the actual formals once,
/// retains caller registers, and refuses before consuming a callee identity.
#[test]
fn static_body_entry_uses_its_own_bank_and_exact_frame_limits() {
    for (memory_offset, depth_offset) in [-1_i128, 0, 1, 16, 17, 18]
        .into_iter()
        .flat_map(|memory| [-1_i64, 0, 1].map(|depth| (memory, depth)))
    {
        let mut run = call_machine();
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let root = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, root).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
        let stamp = machine.logical.borrow().stamp();
        let canonical = (machine.charged, machine.logical_memory.get());
        let mut lease = RegionLease::acquire(machine, program).unwrap();
        for _ in 0..2 {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
        }
        let Executing::Machine(execution) = &mut lease.execution else {
            unreachable!()
        };
        let before = execution.counters;
        let first = execution.invocations.prepare().unwrap().id;
        let required = before.balance
            + lash_kernel_doc::LogicalHeldV1::FRAME
            + lash_kernel_doc::LogicalHeldV1::REFERENCE;
        execution.limits.memory = u64::try_from(i128::from(required) + memory_offset).unwrap();
        execution.limits.depth = u32::try_from(i64::from(before.depth) + 1 + depth_offset).unwrap();
        if memory_offset < 0 || depth_offset < 0 {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Refusal
            );
            let Executing::Machine(execution) = &lease.execution else {
                unreachable!()
            };
            assert_eq!(execution.invocations.prepare().unwrap().id, first);
            assert_eq!(execution.frame_index(), 0);
            assert_eq!(
                (execution.counters.balance, execution.counters.depth),
                (before.balance, before.depth)
            );
        } else {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
            let Executing::Machine(execution) = &mut lease.execution else {
                unreachable!()
            };
            assert_ne!(execution.code.program.code(), root);
            assert_eq!(execution.reached.invocation, first);
            assert_eq!(execution.frame_index(), 1);
            assert_eq!(execution.counters.depth, before.depth + 1);
            assert_eq!(execution.counters.balance, required);
            assert_eq!(execution.counters.peak, before.peak.max(required));
            let target = execution
                .programs
                .target(root, 0, execution.code.program.code())
                .unwrap();
            for (parameter, expected) in
                target
                    .parameters
                    .iter()
                    .zip([Value::Int(7), Value::Int(9), Value::Absent])
            {
                let actual = execution
                    .registers
                    .read(1, first, parameter.output)
                    .unwrap()
                    .scalar(&execution.view)
                    .unwrap();
                assert!(actual.same(&expected));
            }
            // The callee's read of b must use frame 1, while its caller's
            // evaluated operands remain qualified by the caller invocation.
            let result = lease.run(StopAt::NextLegalPrefix).unwrap();
            if memory_offset < 17 {
                assert_eq!(result, RegionStop::Refusal);
            } else {
                assert_eq!(result, RegionStop::Handoff);
                let Executing::Machine(execution) = &lease.execution else {
                    unreachable!()
                };
                let map = execution.code.program.recovery(execution.prefix).unwrap();
                assert!(map.roles.iter().any(|(slot, register)| {
                    matches!(
                        slot,
                        lash_kernel_state::kernel_internals::logical::Slot::Operand(_)
                    ) && execution
                        .read(*register)
                        .unwrap()
                        .scalar(&execution.view)
                        .unwrap()
                        .same(&Value::Int(9))
                }));
            }
        }
        drop(lease);
        assert!(machine.logical.borrow().is_current(&stamp));
        assert_eq!((machine.charged, machine.logical_memory.get()), canonical);
    }
}

/// K-EVAL-008/K-FN-004: stopping inside an entered body publishes both
/// activations, with arguments moved to their actual callee bindings once.
#[test]
fn an_active_static_call_preserves_both_canonical_frames() {
    for commit in [false, true] {
        let mut run = call_machine();
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let root = machine.frame(TaskId::MAIN).unwrap().code;
        let caller = machine.frame(TaskId::MAIN).unwrap().invocation;
        let draft = compile_region(&machine.exe, root).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let occurrence = calls[0].occurrence();
        let initial_count = occurrence.map(|index| {
            machine
                .tasks
                .get_mut(0)
                .unwrap()
                .unwrap()
                .counters
                .current(index)
                .unwrap()
        });
        let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
        let stamp = machine.logical.borrow().stamp();
        let before = (machine.charged, machine.logical_memory.get());
        let mut lease = RegionLease::acquire(machine, program).unwrap();
        for _ in 0..3 {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
        }
        let prefix = lease.materialize().unwrap();
        let image = prefix.reconstruction();
        assert_eq!(image.frames.len(), 2);
        if let (Some(index), Some(count)) = (occurrence, initial_count) {
            assert_eq!(image.source_counts[index], count + 1);
        }
        assert_eq!(
            (image.frames[0].program, image.frames[0].invocation),
            (root, caller)
        );
        assert!(image.frames[0].roles.is_empty());
        let values: Vec<_> = image.frames[1]
            .roles
            .iter()
            .filter_map(|(slot, value)| {
                matches!(
                    slot,
                    lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
                )
                .then_some(*value)
            })
            .collect();
        assert_eq!(values.len(), 3);
        for (actual, expected) in values
            .iter()
            .zip([Value::Int(7), Value::Int(9), Value::Absent])
        {
            assert!(actual.same(&expected));
        }
        let canonical = image.canonical.as_ref().unwrap();
        assert_eq!(
            canonical.frames[0]
                .source_frontier
                .as_ref()
                .unwrap()
                .position()
                .at
                .phase,
            crate::compile::room::Phase::Commit
        );
        let library = canonical.frames[1].library.as_ref().unwrap();
        assert_eq!(library.args.len(), 3);
        assert!(library.args[2].same(&Value::Absent));
        let prepared = prefix.prepare_exit().unwrap();
        if commit {
            assert_eq!(prepared.commit().unwrap(), RegionStop::Handoff);
            let task = machine.tasks.get_mut(0).unwrap().unwrap();
            assert_eq!(task.frames.len(), 2);
            let top = task.frames.last_mut().unwrap().unwrap();
            assert_ne!(top.invocation, caller);
            for (actual, expected) in
                top.slot_states()
                    .iter()
                    .zip([Value::Int(7), Value::Int(9), Value::Absent])
            {
                assert!(
                    matches!(actual, crate::machine::SlotState::Value(value) if value.same(&expected))
                );
            }
        } else {
            drop(prepared);
            assert!(machine.logical.borrow().is_current(&stamp));
            assert_eq!((machine.charged, machine.logical_memory.get()), before);
            assert_eq!(machine.tasks.get_mut(0).unwrap().unwrap().frames.len(), 1);
        }
    }
}

/// K-EVAL-008: a callee entry moves a suspended expression operand to its
/// canonical continuation root, preserving the value and its exact holding.
#[test]
fn a_nested_call_reconstructs_the_saved_expression_operand() {
    for commit in [false, true] {
        let mut run = call_expression("return isa.pick(7, isa.pick(11, 13))");
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let root = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, root).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
        let stamp = machine.logical.borrow().stamp();
        let before = (machine.charged, machine.logical_memory.get());
        let mut lease = RegionLease::acquire(machine, program).unwrap();
        let mut entered = false;
        for _ in 0..16 {
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
            let Executing::Machine(execution) = &lease.execution else {
                unreachable!()
            };
            if execution.frame_index() == 1 {
                entered = true;
                break;
            }
        }
        assert!(
            entered,
            "the inner source call must enter within its fixed source-prefix bound"
        );
        let prefix = lease.materialize().unwrap();
        let image = prefix.reconstruction();
        assert_eq!(image.frames.len(), 2);
        let canonical = image.canonical.as_ref().unwrap();
        let pending = canonical.frames[0].pending.source.as_ref().unwrap();
        assert_eq!(pending.roles.len(), 1);
        assert!(pending.roles[0].1.same(&Value::Int(7)));
        let retained_owner = lash_kernel_state::kernel_internals::logical::Owner::Value {
            task: TaskId::MAIN,
            invocation: canonical.frames[0].invocation,
            slot: pending.roles[0].0,
        };
        assert!(image.frames[0].roles.iter().any(|(slot, value)| matches!(
            slot,
            lash_kernel_state::kernel_internals::logical::Slot::Continuation(_)
        ) && value.same(&Value::Int(7))));
        assert_eq!(
            image.counters.balance,
            2 * lash_kernel_doc::LogicalHeldV1::FRAME
                + 4 * lash_kernel_doc::LogicalHeldV1::REFERENCE
                + 3
        );
        let prepared = prefix.prepare_exit().unwrap();
        if commit {
            prepared.commit().unwrap();
            assert_eq!(machine.tasks.get_mut(0).unwrap().unwrap().frames.len(), 2);
            assert_eq!(machine.logical_memory.get(), 195);
            assert_eq!(
                machine
                    .logical
                    .borrow()
                    .holding(retained_owner, machine.heap.pages())
                    .unwrap()
                    .unwrap()
                    .bytes(),
                lash_kernel_doc::LogicalHeldV1::REFERENCE + 1,
            );
        } else {
            drop(prepared);
            assert!(machine.logical.borrow().is_current(&stamp));
            assert_eq!((machine.charged, machine.logical_memory.get()), before);
        }
    }
}

/// K-MACH-005: an exhausted slice retains the reached operands and must stop
/// before entering a callee, in both charge and source-pace modes.
#[test]
fn an_exhausted_production_slice_preserves_the_exact_caller_prefix() {
    for charging in [false, true] {
        for request in [StopAt::NextLegalPrefix, StopAt::NextBoundary] {
            let mut run = call_machine();
            let machine = &mut run.machine;
            machine.current = Some(TaskId::MAIN);
            if !charging {
                machine.bounds.charge = crate::Charge::Off;
            }
            let root = machine.frame(TaskId::MAIN).unwrap().code;
            let draft = compile_region(&machine.exe, root).unwrap();
            let calls = compile_calls(&machine.exe, &draft).unwrap();
            let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
            let stamp = machine.logical.borrow().stamp();
            let before = (machine.charged, machine.logical_memory.get());
            let mut lease = RegionLease::acquire(machine, program).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    lease.run(StopAt::NextLegalPrefix).unwrap(),
                    RegionStop::Handoff
                );
            }
            let reached = *lease.prefix();
            let Executing::Machine(execution) = &mut lease.execution else {
                unreachable!()
            };
            execution.limits.slice = execution.counters.progress;
            let counters = execution.counters;
            assert_eq!(lease.run(request).unwrap(), RegionStop::Slice);
            assert_eq!(*lease.prefix(), reached);
            assert_eq!(lease.run(request).unwrap(), RegionStop::Slice);
            let prefix = lease.materialize().unwrap();
            let image = prefix.reconstruction();
            assert_eq!(image.issued_invocations, 0);
            assert_eq!(image.frames.len(), 1);
            assert_eq!(
                (
                    image.counters.charge,
                    image.counters.progress,
                    image.counters.balance,
                    image.counters.depth
                ),
                (
                    counters.charge,
                    counters.progress,
                    counters.balance,
                    counters.depth
                )
            );
            let values: Vec<_> = image.frames[0]
                .roles
                .iter()
                .map(|(_, value)| *value)
                .collect();
            assert_eq!(values.len(), 2);
            for (actual, expected) in values.iter().zip([7, 9]) {
                assert!(actual.same(&Value::Int(expected)));
            }
            drop(prefix.prepare_exit().unwrap());
            assert!(machine.logical.borrow().is_current(&stamp));
            assert_eq!((machine.charged, machine.logical_memory.get()), before);
        }
    }
}
/// K-LIM-002/K-EVAL-008: a literal's exact source occurrence is retained only
/// when its 17-byte holding fits; refusal preserves the preceding live roles.
#[test]
fn a_scalar_prefix_obeys_the_exact_source_memory_cutoff() {
    prepared_native_formula();
    use lash_kernel_doc::LogicalHeldV1 as Cost;
    for offset in [-1_i128, 0, 1] {
        for commit in [false, true] {
            let mut run = call_expression("let x = 7 return x");
            let machine = &mut run.machine;
            machine.current = Some(TaskId::MAIN);
            let root = machine.frame(TaskId::MAIN).unwrap().code;
            let draft = compile_region(&machine.exe, root).unwrap();
            let calls = compile_calls(&machine.exe, &draft).unwrap();
            let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
            let entry = machine.logical_memory.get();
            assert_eq!(entry, Cost::FRAME);
            let word = Cost::REFERENCE + 1;
            machine.bounds.memory = u64::try_from(i128::from(entry + word) + offset).unwrap();
            let stamp = machine.logical.borrow().stamp();
            let charged = machine.charged;
            let mut lease = RegionLease::acquire(machine, program).unwrap();
            let stop = if offset < 0 {
                RegionStop::Refusal
            } else {
                RegionStop::Handoff
            };
            assert_eq!(lease.run(StopAt::NextLegalPrefix).unwrap(), stop);
            let prefix = lease.materialize().unwrap();
            let image = prefix.reconstruction();
            let accepted = u64::from(offset >= 0) * word;
            assert_eq!(image.counters.balance, entry + accepted);
            assert_eq!(image.counters.peak, entry + accepted);
            assert_eq!(image.issued_invocations, 0);
            let roles = &image.frames[0].roles;
            if offset < 0 {
                assert!(roles.is_empty());
                assert!(image.pending.is_none());
            } else {
                let [(_, value)] = roles.as_ref() else {
                    panic!("one evaluated literal is source-live")
                };
                assert!(value.same(&Value::Int(7)));
            }
            let prepared = prefix.prepare_exit().unwrap();
            if commit {
                assert_eq!(prepared.commit().unwrap(), stop);
                assert_eq!(machine.logical_memory.get(), entry + accepted);
                assert_eq!(
                    machine.logical.borrow().balance().unwrap().total().unwrap(),
                    entry + accepted
                );
                let actual: Vec<_> = machine
                    .regs
                    .region_values()
                    .map(|(_, value)| value)
                    .collect();
                if offset < 0 {
                    assert!(actual.is_empty());
                    let reached_charge = machine.charged;
                    let limit = machine.bounds.memory;
                    let crate::Step::Ended(crate::End::Error(crate::RunError::Bound(exceeded))) =
                        run.try_run().unwrap()
                    else {
                        panic!("the driver must report the committed source refusal")
                    };
                    assert_eq!(exceeded.bound, crate::Bound::Memory);
                    assert_eq!(exceeded.limit, limit);
                    assert_eq!(run.machine.charged, reached_charge);
                } else {
                    assert!(matches!(actual.as_slice(), [Value::Int(7)]));
                }
            } else {
                drop(prepared);
                assert!(machine.logical.borrow().is_current(&stamp));
                assert_eq!(
                    (machine.charged, machine.logical_memory.get()),
                    (charged, entry)
                );
            }
        }
    }
}

/// K-FN-004: Return releases its frame and transfers the actual result
/// through Incoming before the caller Restore.
#[test]
fn a_returned_call_restores_its_result_after_releasing_the_callee() {
    completed_root();
    reopened_return_phases();
    let mut run = call_machine();
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let root = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, root).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
    let stamp = machine.logical.borrow().stamp();
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    let mut entered = false;
    let mut restored = false;
    for _ in 0..32 {
        let result = lease.run(StopAt::NextLegalPrefix);
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        assert_eq!(
            result.unwrap_or_else(|error| panic!(
                "{error:?} at {:?}: {:?}",
                execution.pc,
                execution
                    .code
                    .program
                    .step(execution.pc)
                    .map(|step| &step.at)
            )),
            RegionStop::Handoff
        );
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        entered |= execution.frame_index() == 1;
        if entered && execution.frame_index() == 0 && execution.returning.is_none() {
            let map = execution.code.program.recovery(execution.prefix).unwrap();
            if map.graph.incoming().is_none() {
                assert!(map.roles.iter().any(|(_, reg)| {
                    execution
                        .read(*reg)
                        .unwrap()
                        .scalar(&execution.view)
                        .unwrap()
                        .same(&Value::Int(9))
                }));
                assert_eq!(execution.counters.depth, execution.limits.entry_depth);
                restored = true;
                break;
            }
        }
    }
    assert!(entered && restored);
    let prefix = lease.materialize().unwrap();
    assert_eq!(prefix.reconstruction().frames.len(), 1);
    prefix.prepare_exit().unwrap().commit().unwrap();
    assert!(!machine.logical.borrow().is_current(&stamp));
}

/// K-LIM-002/K-EVAL-008: a constructor accepts one typed allocation and
/// releases its evaluated source operands before publishing the result.
#[test]
fn a_constructor_transfers_its_actual_operands_once() {
    member_operand_peak();
    captured_constructor();
    use lash_kernel_doc::LogicalHeldV1 as Cost;
    let word = Cost::REFERENCE + 1;
    for (body, tuple) in [
        ("let x = [7, 9] return x", false),
        ("let x = (7, 9) return x", true),
    ] {
        let result = Cost::REFERENCE + 2 * word + if tuple { 0 } else { Cost::OBJECT };
        let balance = Cost::FRAME + result;
        let peak = balance + if tuple { 0 } else { 2 * word };
        for offset in [-1_i128, 0, 1] {
            for commit in [false, true] {
                let mut run = call_expression(body);
                let machine = &mut run.machine;
                machine.current = Some(TaskId::MAIN);
                let root = machine.frame(TaskId::MAIN).unwrap().code;
                let draft = compile_region(&machine.exe, root).unwrap();
                let calls = compile_calls(&machine.exe, &draft).unwrap();
                let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
                let stamp = machine.logical.borrow().stamp();
                let memory = machine.logical_memory.get();
                machine.bounds.memory = u64::try_from(i128::from(peak) + offset).unwrap();
                let mut lease = RegionLease::acquire(machine, program).unwrap();
                for _ in 0..2 {
                    assert_eq!(
                        lease.run(StopAt::NextLegalPrefix).unwrap(),
                        RegionStop::Handoff
                    );
                }
                let stopped = if offset < 0 {
                    RegionStop::Refusal
                } else {
                    RegionStop::Handoff
                };
                assert_eq!(lease.run(StopAt::NextLegalPrefix).unwrap(), stopped);
                let prefix = lease.materialize().unwrap();
                let image = prefix.reconstruction();
                assert_eq!(image.frames.len(), 1);
                let (expected_balance, expected_peak) = if offset < 0 {
                    (Cost::FRAME + 2 * word, Cost::FRAME + 2 * word)
                } else {
                    (balance, peak)
                };
                assert_eq!(image.counters.balance, expected_balance);
                assert_eq!(image.counters.peak, expected_peak);
                let value = if offset < 0 {
                    assert_eq!(image.frames[0].roles.len(), 2);
                    None
                } else {
                    assert_eq!(image.frames[0].roles.len(), 1);
                    Some(image.frames[0].roles[0].1)
                };
                let prepared = prefix.prepare_exit().unwrap();
                if commit {
                    assert_eq!(prepared.commit().unwrap(), stopped);
                    assert_eq!(
                        machine.logical.borrow().balance().unwrap().total().unwrap(),
                        expected_balance
                    );
                    if let Some(value) = value {
                        let arena = machine.arena.borrow();
                        let handle = value.handle().unwrap();
                        let actual = match arena.body(handle).unwrap() {
                            lash_kernel_doc::arena::Body::List(values)
                            | lash_kernel_doc::arena::Body::Tuple(values) => {
                                values.chunks.resident(0).unwrap()
                            }
                            _ => panic!("the source constructor must retain its kind"),
                        };
                        assert_eq!(actual.len(), 2);
                        assert!(actual[0].same(&Value::Int(7)));
                        assert!(actual[1].same(&Value::Int(9)));
                    }
                } else {
                    drop(prepared);
                    assert!(machine.logical.borrow().is_current(&stamp));
                    assert_eq!(machine.logical_memory.get(), memory);
                }
            }
        }
    }
}

fn member_operand_peak() {
    use lash_kernel_doc::LogicalHeldV1 as Cost;
    for offset in [-1_i128, 0, 1] {
        let mut run = call_expression("let x = [7] set x[0] = 9 return x");
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let root = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, root).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
        let mut lease = RegionLease::acquire(machine, program).unwrap();
        for _ in 0..32 {
            let Executing::Machine(execution) = &mut lease.execution else {
                unreachable!()
            };
            if matches!(execution.code.program.op(execution.pc), Some(crate::compile::region_isa::RegionOp::Heap { kernel, .. }) if matches!(kernel.kind(), crate::compile::region_isa::HeapKind::Index { .. }))
            {
                break;
            }
            assert_eq!(
                lease.run(StopAt::NextLegalPrefix).unwrap(),
                RegionStop::Handoff
            );
        }
        let Executing::Machine(execution) = &mut lease.execution else {
            unreachable!()
        };
        let before = execution.counters;
        // K-LIM-002: base, index and RHS remain live while canonical member
        // scratch borrows another occurrence of each. Overwrite grows no body.
        let peak = before.balance + 3 * Cost::REFERENCE + 1;
        execution.limits.memory = u64::try_from(i128::from(peak) + offset).unwrap();
        assert_eq!(
            lease.run(StopAt::NextLegalPrefix).unwrap(),
            if offset < 0 {
                RegionStop::Refusal
            } else {
                RegionStop::Handoff
            }
        );
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        assert_eq!(
            execution.counters.peak,
            if offset < 0 {
                before.peak
            } else {
                before.peak.max(peak)
            }
        );
        lease
            .materialize()
            .unwrap()
            .prepare_exit()
            .unwrap()
            .commit()
            .unwrap();
    }
}

fn captured_constructor() {
    let mut run = call_expression("let n = 7 let f = fn() { return n } return f");
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let root = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, root).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    let mut found = false;
    for _ in 0..32 {
        assert_eq!(
            lease.run(StopAt::NextLegalPrefix).unwrap(),
            RegionStop::Handoff
        );
        let Executing::Machine(execution) = &mut lease.execution else {
            unreachable!()
        };
        let code = Arc::clone(&execution.code);
        let map = code.program.recovery(execution.prefix).unwrap();
        for (_, reg) in &map.roles {
            let value = execution.value(*reg).unwrap();
            if execution.view.kind(value).unwrap() == lash_kernel_doc::ValueKind::Closure {
                let captures = execution.view.callable(value).unwrap().captures().to_vec();
                assert_eq!(captures.len(), 1);
                let cell = execution.view.object(captures[0]).unwrap();
                let value = execution
                    .view
                    .read(
                        cell,
                        &lash_kernel_state::kernel_internals::logical::LeaseMember::Variable,
                    )
                    .unwrap();
                assert!(execution.view.scalar(value).unwrap().same(&Value::Int(7)));
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    assert!(found);
    lease
        .materialize()
        .unwrap()
        .prepare_exit()
        .unwrap()
        .commit()
        .unwrap();
}

fn completed_root() {
    for (body, expected) in [
        ("return 9", Value::Int(9)),
        (
            "let n = 7 let f = fn() { return n } return apply f()",
            Value::Int(7),
        ),
        (
            "let f = fn(a, b) { return b } return apply f(7, 9)",
            Value::Int(9),
        ),
        ("let x = 7", Value::Null),
        ("", Value::Null),
    ] {
        let mut run = call_expression(body);
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let root = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, root).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
        let mut lease = RegionLease::acquire(machine, program).unwrap();
        let outcome = lease.run(StopAt::NextBoundary);
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        assert_eq!(
            outcome.unwrap_or_else(|error| panic!(
                "uninterrupted {body}: {error:?} at {:?}",
                execution
                    .code
                    .program
                    .step(execution.pc)
                    .map(|step| &step.at)
            )),
            RegionStop::Complete,
            "{body}"
        );
        let prefix = lease.materialize().unwrap();
        assert!(prefix.reconstruction().frames.is_empty());
        assert!(
            prefix.reconstruction().payload.unwrap().same(&expected),
            "{body}"
        );
        prefix.prepare_exit().unwrap().commit().unwrap();
        assert_eq!(machine.task(TaskId::MAIN).unwrap().frames.len(), 0);
        assert!(matches!(
            machine.task(TaskId::MAIN).unwrap().record.state,
            lash_kernel_state::paged::TaskStateRecord::Resuming { .. }
        ));
    }
}

// K-CHG-003: a prepared answer without size metadata is executed once and
// charged from its actual operands and result.
fn prepared_native_formula() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ANSWERS: AtomicU64 = AtomicU64::new(0);
    struct Native;
    impl lash_kernel_doc::NativeFunction for Native {
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, lash_kernel_doc::NativeError>
        {
            match args.values() {
                [value @ Value::Int(_)] => {
                    lash_kernel_doc::NativeResourceContract::returned(args, value).map(Some)
                }
                _ => Ok(None),
            }
        }
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<Value, lash_kernel_doc::NativeError> {
            panic!("the certified immediate answer must not be executed again")
        }
        fn specialization(&self) -> lash_kernel_doc::NativeSpecialization {
            lash_kernel_doc::NativeSpecialization {
                immediate_resources: true,
                immediate: Some(lash_kernel_doc::PreparedNative {
                    entry: |args| match args {
                        [Value::Int(value)] => {
                            ANSWERS.fetch_add(1, Ordering::Relaxed);
                            Some(Value::Int(*value))
                        }
                        _ => None,
                    },
                    charged_entry: None,
                    operation: "isa native formula",
                    pair: None,
                }),
                ..Default::default()
            }
        }
    }
    fn probes(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
    ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
        let definition = lash_kernel_doc::parse_definition("function isa.native(a: Int) -> Int\nkernel 1\ncharge sum(17, size(a), size(result))\nnative\n").unwrap();
        vec![(
            "isa.native",
            registry
                .register(definition, Some(Arc::new(Native)))
                .unwrap(),
        )]
    }
    let mut setup = Setup {
        probes: Some(probes),
        ..Setup::default()
    };
    setup.start.target = crate::Target::Entry("go".into());
    let mut run = Embedder::with(
        r#"definition "function go() -> Any\nkernel 1\nmetering statements\nbody { return isa.native(7) }"
main {}"#,
        setup,
    );
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let root = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, root).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, root, draft, &calls).unwrap();
    let entry_charge = machine.charged;
    ANSWERS.store(0, Ordering::Relaxed);
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).unwrap(),
        RegionStop::Handoff
    );
    let Executing::Machine(execution) = &lease.execution else {
        unreachable!()
    };
    let before = execution.counters.charge;
    assert!(before >= entry_charge);
    assert_eq!(ANSWERS.load(Ordering::Relaxed), 0);
    assert_eq!(
        lease.run(StopAt::NextLegalPrefix).unwrap(),
        RegionStop::Handoff
    );
    let Executing::Machine(execution) = &mut lease.execution else {
        unreachable!()
    };
    assert_eq!(execution.counters.charge - before, 17 + 2 + 2);
    assert_eq!(ANSWERS.load(Ordering::Relaxed), 1);
    let map = execution.code.program.recovery(execution.prefix).unwrap();
    assert!(map.roles.iter().any(|(_, reg)| {
        execution
            .read(*reg)
            .unwrap()
            .scalar(&execution.view)
            .unwrap()
            .same(&Value::Int(7))
    }));
}

/// Ruling 3 / K-EVAL-008: every affordable Return/Leave prefix survives a cold
/// reopen with its result and already paid work, without re-entering a scope.
fn reopened_return_phases() {
    for (body, expected, expected_returns) in [
        ("return 9", Some(Value::Int(9)), 1),
        (
            "if true { let x = 9 return x } return 0",
            Some(Value::Int(9)),
            1,
        ),
        ("let x = [7, 9] return x", None, 1),
        ("let x = [7] let y = [x, x] return y", None, 1),
        ("return isa.pick(7, 9)", Some(Value::Int(9)), 2),
        (
            "let n = 7 let f = fn() { return n } return apply f()",
            Some(Value::Int(7)),
            2,
        ),
        (
            "let n = 7 let f = fn() { set n = 9 return n } return apply f()",
            Some(Value::Int(9)),
            2,
        ),
        (
            "let f = fn(a, b) { return b } return apply f(7, 9)",
            Some(Value::Int(9)),
            2,
        ),
        ("return num.add(3, isa.pick(7, 9))", Some(Value::Int(12)), 2),
        (
            "let x = 0 while num.lt(x, 1) { set x = num.add(x, 1) } return x",
            Some(Value::Int(1)),
            1,
        ),
        ("let x = [7] if true { return x } return null", None, 1),
        (
            "let f = fn() { let g = fn() { return 9 } return apply g() } return apply f()",
            Some(Value::Int(9)),
            3,
        ),
        ("return isa.measured([7, 9], 3)", Some(Value::Int(3)), 2),
    ] {
        let mut uninterrupted = call_expression(body);
        let machine = &mut uninterrupted.machine;
        machine.current = Some(TaskId::MAIN);
        let id = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, id).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let code = verify_region(&machine.exe, id, draft, &calls).unwrap();
        let mut lease = RegionLease::acquire(machine, code).unwrap();
        let outcome = lease.run(StopAt::NextBoundary);
        let Executing::Machine(execution) = &lease.execution else {
            unreachable!()
        };
        assert_eq!(
            outcome.unwrap_or_else(|error| panic!(
                "uninterrupted {body}: {error:?} at {:?}",
                execution
                    .code
                    .program
                    .step(execution.pc)
                    .map(|step| &step.at)
            )),
            RegionStop::Complete,
            "{body}"
        );
        let prefix = lease.materialize().unwrap();
        let expected_charge = prefix.reconstruction().counters.progress;
        let expected_balance = prefix.reconstruction().counters.balance;
        drop(prefix);
        let mut source = call_expression(body);
        source.machine.force_source_execution();
        let source_result = crate::laws::embedder::result(source.run_to_end(&[]));
        let mut run = call_expression(body);
        let mut charges = std::collections::BTreeSet::new();
        let mut returns = 0;
        let mut leaves = 0;
        let mut completed = false;
        for prefix_index in 0..64 {
            let machine = &mut run.machine;
            machine.current = Some(TaskId::MAIN);
            let id = machine
                .frame(TaskId::MAIN)
                .unwrap_or_else(|error| panic!("{body} prefix {prefix_index}: {error:?}"))
                .code;
            let draft = compile_region(&machine.exe, id).unwrap();
            let calls = compile_calls(&machine.exe, &draft).unwrap();
            let code = verify_region(&machine.exe, id, draft, &calls).unwrap();
            let mut lease = RegionLease::acquire(machine, code)
                .unwrap_or_else(|error| panic!("{body} prefix {prefix_index}: {error:?}"));
            let Executing::Machine(execution) = &lease.execution else {
                unreachable!()
            };
            let returning = matches!(
                execution.code.program.op(execution.pc),
                Some(crate::compile::region_isa::RegionOp::Return { .. })
            );
            let leaving = execution.code.program.step(execution.pc).unwrap().at.phase
                == crate::compile::room::Phase::Leave;
            let outcome = lease.run(StopAt::NextLegalPrefix);
            let Executing::Machine(execution) = &lease.execution else {
                unreachable!()
            };
            let stop = outcome.unwrap_or_else(|error| panic!(
                "{body} prefix {prefix_index}: {error:?}, at {:?}, frames {}, returning {}, departing {}",
                execution.code.program.step(execution.pc).map(|step| &step.at),
                execution.parked.len() + 1, execution.returning.is_some(), execution.departing.is_some()
            ));
            returns += usize::from(returning);
            leaves += usize::from(leaving);
            let current_code = Arc::clone(&execution.code);
            let prefix = lease.materialize().unwrap();
            if let Some(source) = prefix
                .reconstruction()
                .canonical
                .as_ref()
                .and_then(|canonical| canonical.frames.last())
                .and_then(|frame| frame.pending.source.as_ref())
                && source.progress == 2
                && let Some(origin) = &source.departure
            {
                let mut repeated = source.clone();
                repeated.unpaid = origin.clone();
                assert!(
                    super::resume::position(&current_code, &repeated).is_err(),
                    "reopen must reject a continuation that repeats Return: {body}"
                );
            }
            let charge = prefix.reconstruction().counters.progress;
            let balance = prefix.reconstruction().counters.balance;
            charges.extend([charge.saturating_sub(1), charge, charge.saturating_add(1)]);
            let result = prefix.reconstruction().payload;
            if stop == RegionStop::Complete {
                let result = result.unwrap();
                if let Some(expected) = expected {
                    assert!(
                        result.same(&expected),
                        "result survives every reopen: {body}"
                    );
                }
                assert_eq!(charge, expected_charge, "no repeated charge: {body}");
                assert_eq!(balance, expected_balance, "no repeated release: {body}");
                completed = true;
            }
            prefix.prepare_exit().unwrap().commit().unwrap();
            if let Some(result) = result {
                assert_eq!(
                    machine.copy_datum(&result).unwrap(),
                    source_result,
                    "independent source result: {body}"
                );
                assert_eq!(
                    source.machine.charged, charge,
                    "independent source charge: {body}"
                );
            }
            assert_eq!(machine.charged, charge);
            assert_eq!(machine.logical_memory.get(), balance);
            run.reopen(crate::Layout::default(), false);
            assert_eq!(run.machine.charged, charge);
            assert_eq!(run.machine.logical_memory.get(), balance);
            let (root, certificate) = run.checkpoint();
            let end = loop {
                match run.try_run().unwrap_or_else(|error| {
                    panic!("normal driver {body} prefix {prefix_index}: {error:?}")
                }) {
                    crate::Step::Ended(end) => break end,
                    crate::Step::Slice => {}
                    other => panic!("unexpected pause {body} prefix {prefix_index}: {other:?}"),
                }
            };
            assert_eq!(
                crate::laws::embedder::result(end),
                source_result,
                "normal driver resumes source phase: {body} prefix {prefix_index}"
            );
            assert_eq!(
                run.machine.charged, source.machine.charged,
                "normal driver repeats no charge: {body} prefix {prefix_index}"
            );
            run.open_at(&root, certificate, crate::Layout::default(), false);
            if completed {
                break;
            }
        }
        assert!(
            completed,
            "Return/Leave must complete within the fixed bound: {body}"
        );
        assert_eq!(returns, expected_returns, "Return executes once: {body}");
        assert!(leaves >= 1, "defining frame Leave executes: {body}");
        for limit in charges {
            reopened_charge_refusal(body, limit);
        }
    }
}

fn reopened_charge_refusal(body: &str, limit: u64) {
    let mut run = call_expression(body);
    run.bounds.charge_limit = limit;
    run.machine.bounds.charge_limit = limit;
    let mut before = None;
    for reopen in 0..3 {
        let machine = &mut run.machine;
        machine.current = Some(TaskId::MAIN);
        let id = machine.frame(TaskId::MAIN).unwrap().code;
        let draft = compile_region(&machine.exe, id).unwrap();
        let calls = compile_calls(&machine.exe, &draft).unwrap();
        let code = verify_region(&machine.exe, id, draft, &calls).unwrap();
        let mut lease = RegionLease::acquire(machine, code)
            .unwrap_or_else(|error| panic!("{body}, limit {limit}, reopen {reopen}: {error:?}"));
        let stop = lease
            .run(StopAt::NextBoundary)
            .unwrap_or_else(|error| panic!("{body}, limit {limit}, reopen {reopen}: {error:?}"));
        if stop == RegionStop::Complete {
            assert!(
                before.is_none(),
                "a reopened refusal cannot complete: {body}, limit {limit}"
            );
            return;
        }
        assert_eq!(
            stop,
            RegionStop::Refusal,
            "{body}, limit {limit}, reopen {reopen}"
        );
        let prefix = lease.materialize().unwrap_or_else(|error| {
            panic!("materialize {body}, limit {limit}, reopen {reopen}: {error:?}")
        });
        let image = prefix.reconstruction();
        let actual = (
            image.counters.charge,
            image.counters.balance,
            image.counters.depth,
            image.prefix.debt,
        );
        if let Some(before) = before {
            assert_eq!(
                actual, before,
                "a refusal repeats no charge or release: {body}, limit {limit}, reopen {reopen}"
            );
        }
        before = Some(actual);
        prefix.prepare_exit().unwrap().commit().unwrap();
        run.reopen(crate::Layout::default(), false);
    }
}
