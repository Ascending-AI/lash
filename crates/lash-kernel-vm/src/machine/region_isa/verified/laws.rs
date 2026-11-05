use super::super::*;
use crate::compile::region_isa::{compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;
use crate::laws::embedder::{Embedder, Setup};
use lash_kernel_doc::TaskId;

fn call_machine() -> Embedder {
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
    Embedder::with(
        r#"definition "function go() -> Any\nkernel 1\nmetering statements\nbody { return isa.pick(7, 9) }"
main {}"#,
        setup,
    )
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
