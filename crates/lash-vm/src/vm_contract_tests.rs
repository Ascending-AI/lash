use crate::*;
use lash_vm_protocol::{
    OpaqueStateRefusal, OpaqueVmState, StateExpectation, VmContractComponent, VmOwner, VmStateKind,
};

#[test]
fn a_component_outside_its_range_is_refused_typed() {
    let owner = VmOwner::new("process:outside-range");
    let mut contract = vm_contract_versions();
    contract.accounting += 1;
    let parked = OpaqueVmState::seal(
        VmStateKind::Continuation,
        owner.clone(),
        contract,
        Vec::new(),
    );
    let refusal = parked
        .check(&StateExpectation {
            kind: VmStateKind::Continuation,
            owner: &owner,
            reads: &vm_contract_reads(),
            max_bytes: 1024,
        })
        .expect_err("accounting outside the supported range must refuse");
    assert_eq!(
        refusal,
        OpaqueStateRefusal::ComponentOutsideReadRange {
            component: VmContractComponent::Accounting,
            found: INSTRUCTION_ACCOUNTING_VERSION + 1,
            reads: vm_contract_reads().accounting,
        }
    );
    let expected_range = vm_contract_reads().accounting.to_string();
    assert!(
        refusal.to_string().contains("accounting") && refusal.to_string().contains(&expected_range),
        "{refusal}"
    );
}

#[cfg(feature = "synthetic-next")]
#[test]
fn rollback_admits_only_versions_inside_n_ranges() {
    let owner = VmOwner::new("process:rollback-witness");
    let next = vm_contract_versions();
    let mut predecessor = next;
    predecessor.continuation = 1;
    predecessor.snapshot = 1;
    let reads = predecessor.exact_reads();
    for (kind, format, component, range) in [
        (
            VmStateKind::Continuation,
            next.continuation,
            VmContractComponent::Continuation,
            reads.continuation,
        ),
        (
            VmStateKind::Snapshot,
            next.snapshot,
            VmContractComponent::Snapshot,
            reads.snapshot,
        ),
    ] {
        // Isolate the component that determines this state's format.
        let mut contract = predecessor;
        match kind {
            VmStateKind::Continuation => contract.continuation = format,
            VmStateKind::Snapshot => contract.snapshot = format,
        }
        let state = OpaqueVmState::seal(kind, owner.clone(), contract, Vec::new());
        assert_eq!(
            state.check(&StateExpectation {
                kind,
                owner: &owner,
                reads: &reads,
                max_bytes: 1024
            }),
            Err(OpaqueStateRefusal::ComponentOutsideReadRange {
                component,
                found: format,
                reads: range
            }),
        );
        let compatible = OpaqueVmState::seal(kind, owner.clone(), predecessor, Vec::new());
        assert_eq!(
            compatible.check(&StateExpectation {
                kind,
                owner: &owner,
                reads: &reads,
                max_bytes: 1024
            }),
            Ok(()),
        );
    }
}

#[cfg(feature = "synthetic-next")]
#[test]
fn n_snapshot_reaches_the_guarded_decoder_on_synthetic_next() {
    let snapshot = Snapshot::new(Record::new());
    let bytes = snapshot
        .to_canonical_bytes_stamped(crate::runtime::SnapshotStamps { snapshot: 1 })
        .expect("encode N's snapshot");
    let mut contract = vm_contract_versions();
    contract.snapshot = 1;
    let owner = VmOwner::new("session:upgrade-witness");
    let parked = OpaqueVmState::seal(VmStateKind::Snapshot, owner.clone(), contract, bytes);
    assert_eq!(
        parked.check(&StateExpectation {
            kind: VmStateKind::Snapshot,
            owner: &owner,
            reads: &vm_contract_reads(),
            max_bytes: 1024 * 1024,
        }),
        Ok(())
    );
    assert_eq!(
        VmInstance::pristine()
            .open_snapshot(parked.bytes())
            .expect("decode through the FIG-3802 guarded window"),
        snapshot
    );
}
