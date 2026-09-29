use super::*;

fn state() -> OpaqueVmState {
    OpaqueVmState::seal(
        VmStateKind::Continuation,
        VmOwner::new("process-1"),
        "vm-contract-a",
        29,
        b"continuation bytes".to_vec(),
    )
}

fn expectation(owner: &VmOwner) -> StateExpectation<'_> {
    StateExpectation {
        kind: VmStateKind::Continuation,
        owner,
        vm_contract: "vm-contract-a",
        format_version: 29,
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
            vm_contract: "vm-contract-b",
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::WrongVmContract { .. })
    ));
    assert!(matches!(
        state().check(&StateExpectation {
            format_version: 30,
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::WrongFormatVersion { .. })
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
    assert!(matches!(
        shortened.check(&expectation(&owner)),
        Err(OpaqueStateRefusal::LengthMismatch { .. })
    ));
}

#[test]
fn json_carries_the_bytes_as_base64_and_the_facts_readably() {
    let json = serde_json::to_value(state()).unwrap();
    assert_eq!(json["kind"], "continuation");
    assert_eq!(json["format_version"], 29);
    assert_eq!(json["bytes"], "Y29udGludWF0aW9uIGJ5dGVz");
    let back: OpaqueVmState = serde_json::from_value(json).unwrap();
    assert_eq!(back, state());
}

#[test]
fn an_unknown_field_is_refused() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["deferred_resolutions"] = serde_json::json!({});
    assert!(serde_json::from_value::<OpaqueVmState>(json).is_err());
}
