use super::super::*;
use crate::compile::region_isa::{compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;
use crate::laws::embedder::{Embedder, Setup};
use lash_kernel_doc::TaskId;

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
