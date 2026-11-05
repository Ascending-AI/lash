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
