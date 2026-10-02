#[cfg(feature = "synthetic-next")]
use crate::testing::ast_builders as b;
use crate::*;
use lash_sansio::VersionRange;
use lash_vm_protocol::{
    OpaqueStateRefusal, OpaqueVmState, StateExpectation, VmContractComponent, VmOwner, VmStateKind,
};

#[cfg(feature = "synthetic-next")]
struct SleepHost;

#[cfg(feature = "synthetic-next")]
impl ExecutionHost for SleepHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "the witness only sleeps or finishes",
            )),
        }
    }
}

#[cfg(feature = "synthetic-next")]
#[tokio::test]
async fn n_parked_state_resumes_on_synthetic_next() {
    assert_eq!(VM_CONTINUATION_FORMAT_VERSION, 30);
    let program = crate::testing::harness::try_compile_program(&b::program(vec![
        b::sleep_for(b::num(1.0)),
        b::finish(b::num(7.0)),
    ]))
    .expect("compile the process witness");
    let mut state = State::new();
    let environment = ExecutionEnvironment::new(&SleepHost).process();
    let mut vm = Vm::from_state(&program, &mut state, &environment).expect("install the witness");
    assert_eq!(
        vm.run_process_until_effect()
            .await
            .expect("run the first effect"),
        VmRunOutcome::EffectCompleted
    );
    let mut wire = serde_json::to_value(vm.suspend().expect("park N's process"))
        .expect("serialize the continuation");
    // The synthetic predecessor and successor have the same payload shape;
    wire["format_version"] = serde_json::json!(29);
    let mut predecessor = vm_contract_versions();
    predecessor.continuation = 29;
    predecessor.snapshot = 14;
    let owner = VmOwner::new("process:upgrade-witness");
    let parked = OpaqueVmState::seal(
        VmStateKind::Continuation,
        owner.clone(),
        predecessor,
        serde_json::to_vec(&wire).expect("encode N's continuation"),
    );
    assert_eq!(
        parked.check(&StateExpectation {
            kind: VmStateKind::Continuation,
            owner: &owner,
            reads: &vm_contract_reads(),
            max_bytes: 1024 * 1024,
        }),
        Ok(()),
        "N+1 must admit every component N parked within its read range"
    );
    let continuation = VmInstance::pristine()
        .open_continuation(parked.bytes())
        .expect("the worker lifts N's continuation through its range decoder");
    assert_eq!(continuation.format_version, VM_CONTINUATION_FORMAT_VERSION);
    let mut resumed = Vm::resume_from(continuation, &program, &environment)
        .expect("resume the predecessor's execution");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("finish the successor"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::Number(7.0)))
    );
}

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
    predecessor.continuation = 29;
    predecessor.snapshot = 14;
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

#[test]
fn component_ranges_match_the_vm_decoders() {
    let contract = vm_contract_versions();
    let reads = vm_contract_reads();
    assert_eq!(reads.admit(contract), Ok(()));
    assert_eq!(reads.continuation, VM_CONTINUATION_READ_RANGE);
    assert_eq!(
        reads.snapshot,
        lash_core_execution::FleetFormat::current()
            .read_window(lash_core_execution::surface_format!(
                LASHLANG_SNAPSHOT_VERSION
            ))
            .supported()
    );
    assert_eq!(
        reads.bytecode,
        VersionRange::exactly(BYTECODE_FORMAT_VERSION)
    );
    assert_eq!(
        reads.accounting,
        VersionRange::exactly(INSTRUCTION_ACCOUNTING_VERSION)
    );
    assert_eq!(reads.abi, VersionRange::exactly(contract.abi));
    assert_eq!(
        LASHLANG_VM_ABI_VERSION,
        format!("lashlang-vm-abi-v{}", contract.abi)
    );
}

#[cfg(feature = "synthetic-next")]
#[test]
fn n_snapshot_reaches_the_guarded_decoder_on_synthetic_next() {
    let snapshot = Snapshot::new(Record::new());
    let bytes = snapshot
        .to_canonical_bytes_stamped(crate::runtime::SnapshotStamps { snapshot: 14 })
        .expect("encode N's snapshot");
    let mut contract = vm_contract_versions();
    contract.continuation = 29;
    contract.snapshot = 14;
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
