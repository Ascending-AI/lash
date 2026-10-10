use super::*;

const KERNEL: u32 = 1;
const DOCUMENT: &str = "d0c0";
const KERNEL_WHOLE: u32 = 16;

fn node(id: u8) -> GraphNodeBytes {
    GraphNodeBytes {
        id: [id; 32],
        bytes: vec![id; 8],
    }
}

fn state() -> SealedGraphState {
    SealedGraphState::seal(
        VmOwner::new("process-1"),
        KERNEL,
        DOCUMENT,
        GraphRoot {
            kind: KERNEL_WHOLE,
            id: [2; 32],
        },
        vec![node(2), node(1), node(2)],
    )
}

fn expectation(owner: &VmOwner) -> GraphStateExpectation<'_> {
    GraphStateExpectation {
        owner,
        kernel: VersionRange::exactly(KERNEL),
        document: Some(DOCUMENT),
        root_kinds: &[KERNEL_WHOLE],
        max_nodes: 2,
        max_bytes: 16,
    }
}

/// Every structural mismatch is named, limit equality is admitted, and any
/// change to a sealed fact or to the staged node set breaks the seal.
#[test]
fn the_structural_check_names_each_mismatch_and_the_seal_binds_the_facts() {
    let owner = VmOwner::new("process-1");
    let sealed = state();
    assert_eq!(sealed.nodes().len(), 2, "a node staged twice is kept once");
    assert_eq!(sealed.check(&expectation(&owner)), Ok(()));
    let other = VmOwner::new("process-2");
    assert!(matches!(
        sealed.check(&expectation(&other)),
        Err(GraphStateRefusal::WrongOwner { .. })
    ));
    assert!(matches!(
        sealed.check(&GraphStateExpectation {
            kernel: VersionRange::exactly(2),
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::KernelOutsideReadRange { .. })
    ));
    assert!(matches!(
        sealed.check(&GraphStateExpectation {
            document: Some("0ther"),
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::WrongDocument { .. })
    ));
    assert_eq!(
        sealed.check(&GraphStateExpectation {
            root_kinds: &[27],
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::WrongRootKind {
            found: KERNEL_WHOLE
        })
    );
    assert_eq!(
        sealed.check(&GraphStateExpectation {
            max_nodes: 1,
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::TooManyNodes { limit: 1, len: 2 })
    );
    assert_eq!(
        sealed.check(&GraphStateExpectation {
            max_bytes: 15,
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::TooLarge { limit: 15, len: 16 })
    );

    let mut forged = sealed.clone();
    forged.nodes.push(node(3));
    assert_eq!(
        forged.check(&GraphStateExpectation {
            max_nodes: 3,
            max_bytes: 24,
            ..expectation(&owner)
        }),
        Err(GraphStateRefusal::SealMismatch)
    );
    let mut forged = sealed.clone();
    forged.root.id = [1; 32];
    assert_eq!(
        forged.check(&expectation(&owner)),
        Err(GraphStateRefusal::SealMismatch)
    );
    let mut forged = sealed.clone();
    forged.nodes.swap(0, 1);
    assert_eq!(
        forged.check(&expectation(&owner)),
        Err(GraphStateRefusal::UnsortedNodes)
    );
}

/// The sealed state crosses the worker boundary in the binary transport
/// unchanged.
#[test]
fn sealed_state_round_trips_the_binary_transport() {
    let sealed = state();
    let bytes = rmp_serde::to_vec_named(&sealed).unwrap();
    let back: SealedGraphState = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, sealed);
    assert_eq!(back.check(&expectation(&VmOwner::new("process-1"))), Ok(()));
}
