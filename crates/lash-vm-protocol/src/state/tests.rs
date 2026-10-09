use super::*;

const KERNEL: u32 = 1;
const DOCUMENT: &str = "d0c0";

fn state() -> OpaqueVmState {
    OpaqueVmState::seal(
        VmOwner::new("process-1"),
        KERNEL,
        DOCUMENT,
        b"parked run bytes".to_vec(),
    )
}

fn expectation(owner: &VmOwner) -> StateExpectation<'_> {
    StateExpectation {
        owner,
        kernel: VersionRange::exactly(KERNEL),
        document: Some(DOCUMENT),
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
    for reads in [VersionRange::exactly(2), VersionRange::between(2, 3)] {
        assert_eq!(
            state().check(&StateExpectation {
                kernel: reads,
                ..expectation(&owner)
            }),
            Err(OpaqueStateRefusal::KernelOutsideReadRange {
                found: KERNEL,
                reads
            })
        );
    }
    assert_eq!(
        state().check(&StateExpectation {
            kernel: VersionRange::between(1, 2),
            ..expectation(&owner)
        }),
        Ok(())
    );
    assert!(matches!(
        state().check(&StateExpectation {
            document: Some("0ther"),
            ..expectation(&owner)
        }),
        Err(OpaqueStateRefusal::WrongDocument { .. })
    ));
    assert_eq!(
        state().check(&StateExpectation {
            document: None,
            ..expectation(&owner)
        }),
        Ok(())
    );
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
    assert_eq!(json["kernel"], 1);
    assert_eq!(json["document"], DOCUMENT);
    assert_eq!(json["bytes"], "cGFya2VkIHJ1biBieXRlcw==");
    let back: OpaqueVmState = serde_json::from_value(json).unwrap();
    assert_eq!(back, state());
}

#[test]
fn an_unknown_field_is_refused() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["deferred_resolutions"] = serde_json::json!({});
    assert!(serde_json::from_value::<OpaqueVmState>(json).is_err());
}
