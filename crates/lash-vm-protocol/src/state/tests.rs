use super::*;

const CONTRACT: VmContract = VmContract {
    bytecode: 30,
    continuation: 29,
    snapshot: 14,
    accounting: 3,
    heap: 3,
    abi: 14,
};
const READS: VmContractReads = CONTRACT.exact_reads();

fn state() -> OpaqueVmState {
    OpaqueVmState::seal(
        VmStateKind::Continuation,
        VmOwner::new("process-1"),
        CONTRACT,
        b"continuation bytes".to_vec(),
    )
}

fn expectation(owner: &VmOwner) -> StateExpectation<'_> {
    StateExpectation {
        kind: VmStateKind::Continuation,
        owner,
        reads: &READS,
        max_bytes: 1024,
    }
}

#[test]
fn the_structural_check_names_each_mismatch() {
    let owner = VmOwner::new("process-1");
    let other = VmOwner::new("process-2");
    assert_eq!(state().check(&expectation(&owner)), Ok(()));
    assert!(matches!(
        state().check(&expectation(&other)),
        Err(OpaqueStateRefusal::WrongOwner { .. })
    ));
    assert!(matches!(
        state().check(&StateExpectation {
            kind: VmStateKind::Snapshot,
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::WrongKind { .. })
    ));
    assert!(matches!(
        state().check(&StateExpectation {
            reads: &VmContractReads {
                abi: VersionRange::exactly(15),
                ..READS
            },
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::ComponentOutsideReadRange {
            component: VmContractComponent::Abi,
            ..
        })
    ));
    assert!(matches!(
        state().check(&StateExpectation {
            reads: &VmContractReads {
                continuation: VersionRange::exactly(30),
                ..READS
            },
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::ComponentOutsideReadRange {
            component: VmContractComponent::Continuation,
            ..
        })
    ));
    assert!(matches!(
        state().check(&StateExpectation {
            max_bytes: 4,
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::TooLarge { .. })
    ));
}

#[test]
fn tampered_bytes_fail_the_hash() {
    let owner = VmOwner::new("process-1");
    let mut tampered = state();
    tampered.bytes[0] ^= 1;
    assert_eq!(
        tampered.check(&expectation(&owner)),
        Err(OpaqueStateRefusal::HashMismatch)
    );
    let mut shortened = state();
    shortened.bytes.pop();
    assert_eq!(
        shortened.check(&expectation(&owner)),
        Err(OpaqueStateRefusal::HashMismatch)
    );
}

#[test]
fn json_carries_the_bytes_as_base64_and_the_facts_readably() {
    let json = serde_json::to_value(state()).unwrap();
    assert_eq!(json["kind"], "continuation");
    assert_eq!(
        json["vm_contract"],
        serde_json::json!({"bytecode":30,"continuation":29,"snapshot":14,"accounting":3,"heap":3,"abi":14})
    );
    assert_eq!(json["bytes"], "Y29udGludWF0aW9uIGJ5dGVz");
    let back: OpaqueVmState = serde_json::from_value(json).unwrap();
    assert_eq!(back, state());
}

#[test]
fn an_unknown_field_is_refused() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["deferred_resolutions"] = serde_json::json!({});
    assert!(serde_json::from_value::<OpaqueVmState>(json).is_err());
    let mut json = serde_json::to_value(state()).unwrap();
    json["vm_contract"]["future_component"] = serde_json::json!(1);
    assert!(serde_json::from_value::<OpaqueVmState>(json).is_err());
    let mut json = serde_json::to_value(state()).unwrap();
    json["vm_contract"] = serde_json::json!("legacy-whole-contract");
    assert!(serde_json::from_value::<OpaqueVmState>(json).is_err());
}

#[test]
fn every_component_is_checked_against_both_range_bounds() {
    let widened = VmContractReads {
        bytecode: VersionRange::between(30, 31),
        continuation: VersionRange::between(29, 30),
        snapshot: VersionRange::between(14, 15),
        accounting: VersionRange::between(3, 4),
        heap: VersionRange::between(3, 4),
        abi: VersionRange::between(14, 15),
    };
    assert_eq!(widened.admit(CONTRACT), Ok(()));
    assert_eq!(
        widened.admit(VmContract {
            bytecode: 31,
            continuation: 30,
            snapshot: 15,
            accounting: 4,
            heap: 4,
            abi: 15
        }),
        Ok(())
    );
    for (component, min, max) in [
        (VmContractComponent::Bytecode, 30, 31),
        (VmContractComponent::Continuation, 29, 30),
        (VmContractComponent::Snapshot, 14, 15),
        (VmContractComponent::Accounting, 3, 4),
        (VmContractComponent::Heap, 3, 4),
        (VmContractComponent::Abi, 14, 15),
    ] {
        for found in [min - 1, max + 1] {
            let mut contract = CONTRACT;
            match component {
                VmContractComponent::Bytecode => contract.bytecode = found,
                VmContractComponent::Continuation => contract.continuation = found,
                VmContractComponent::Snapshot => contract.snapshot = found,
                VmContractComponent::Accounting => contract.accounting = found,
                VmContractComponent::Heap => contract.heap = found,
                VmContractComponent::Abi => contract.abi = found,
            }
            assert_eq!(
                widened.admit(contract),
                Err(OpaqueStateRefusal::ComponentOutsideReadRange {
                    component,
                    found,
                    reads: VersionRange::between(min, max)
                })
            );
        }
    }
}

/// FIG-4645: the state stores its contract and its bytes once each. The
/// format version is the contract component the kind names and the length is
/// the bytes', so neither can disagree with what it is derived from.
#[test]
fn the_format_version_and_length_are_derived_and_stored_nowhere() {
    let owner = VmOwner::new("process-1");
    for (kind, version) in [
        (VmStateKind::Continuation, CONTRACT.continuation),
        (VmStateKind::Snapshot, CONTRACT.snapshot),
    ] {
        let state = OpaqueVmState::seal(kind, owner.clone(), CONTRACT, vec![1, 2, 3]);
        assert_eq!(state.format_version(), version);
        assert_eq!(state.len(), 3);
        let json = serde_json::to_value(&state).unwrap();
        let mut fields = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "bytes",
                "definition_ids",
                "hash",
                "kind",
                "owner",
                "vm_contract"
            ]
        );
        for (duplicate, value) in [("format_version", version), ("len", 3)] {
            let mut json = json.clone();
            json[duplicate] = serde_json::json!(value);
            assert!(
                serde_json::from_value::<OpaqueVmState>(json).is_err(),
                "a stored `{duplicate}` is a second copy and is refused"
            );
        }
    }
}
