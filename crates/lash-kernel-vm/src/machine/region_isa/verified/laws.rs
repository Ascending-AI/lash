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
        vec![("isa.pick", registry.register(definition, None).unwrap())]
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
        let pending = canonical.frames[0].pending.peek().unwrap();
        assert_eq!(pending.roles.len(), 1);
        assert!(pending.roles[0].value.same(&Value::Int(7)));
        assert_eq!(
            pending.roles[0].holding.bytes(),
            lash_kernel_doc::LogicalHeldV1::REFERENCE + 1
        );
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

/// K-FN-004/K-EVAL-008: a real body call must transfer its actual result,
/// release the callee's formals and frame, and restore its caller exactly once.
#[test]
fn a_body_return_leaves_its_frame_and_restores_the_actual_result() {
    let mut run = call_machine();
    let machine = &mut run.machine;
    machine.current = Some(TaskId::MAIN);
    let entry_depth = machine.tasks.get_mut(0).unwrap().unwrap().frames.len() as u32
        + u32::from(!machine.session_cell);
    let id = machine.frame(TaskId::MAIN).unwrap().code;
    let draft = compile_region(&machine.exe, id).unwrap();
    let calls = compile_calls(&machine.exe, &draft).unwrap();
    let program = verify_region(&machine.exe, id, draft, &calls).unwrap();
    let mut lease = RegionLease::acquire(machine, program).unwrap();
    for _ in 0..4 {
        assert_eq!(
            lease.run(StopAt::NextLegalPrefix).unwrap(),
            RegionStop::Handoff
        );
    }
    let Executing::Machine(execution) = &mut lease.execution else {
        unreachable!()
    };
    assert!(matches!(
        execution.code.program.op(execution.pc),
        Some(crate::compile::region_isa::RegionOp::Return { .. })
    ));
    let activation = execution.activation.take().unwrap();
    execution.leave_call(activation).unwrap();
    assert!(execution.parked.is_empty());
    assert_eq!(execution.counters.depth, entry_depth);
    // The source retains one caller frame and one Int(9) occurrence.
    assert_eq!(
        execution.counters.balance,
        lash_kernel_doc::LogicalHeldV1::FRAME + 17
    );
    let call = execution.code.calls.iter().flatten().next().unwrap();
    assert_eq!(execution.pc, call.next);
    let result = execution
        .registers
        .read(0, execution.reached.invocation, call.result)
        .unwrap()
        .bind(&mut execution.view)
        .unwrap();
    assert!(execution.view.scalar(result).unwrap().same(&Value::Int(9)));
}
