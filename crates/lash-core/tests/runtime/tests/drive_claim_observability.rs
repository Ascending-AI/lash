use lash_core::store::ClaimAuthority;
use lash_core::testing::runtime_internals::trace_commit_cas_rejected;
use lash_core::{LeaseOwnerIdentity, SessionId, StoreError};

use super::trace_capture::capturing;

#[tokio::test]
async fn a_rejected_head_cas_traces_the_claimant_without_an_authority() {
    let claimant = LeaseOwnerIdentity::opaque("worker-b", "worker-b:boot-1");
    let ((), capture) = capturing(|| async {
        trace_commit_cas_rejected(
            &SessionId::from("head-cas-no-authority"),
            None,
            &claimant,
            "claimant-executor",
            &StoreError::HeadRevisionConflict {
                expected: 3,
                actual: 4,
            },
        );
    })
    .await;

    let rejected = capture.exactly_one("session_head.commit_cas_rejected");
    assert_eq!(rejected.level, "WARN");
    assert_eq!(rejected.field("session_id"), "head-cas-no-authority");
    assert_eq!(rejected.field("owner_id"), "worker-b");
    assert_eq!(rejected.field("incarnation_id"), "worker-b:boot-1");
    assert_eq!(rejected.field("executor_id"), "claimant-executor");
    assert_eq!(rejected.field("expected_head_revision"), "3");
    assert_eq!(rejected.field("actual_head_revision"), "4");
}

#[tokio::test]
async fn a_rejected_head_cas_traces_the_sealed_drive_authority() {
    let claimant = LeaseOwnerIdentity::opaque("worker-b", "worker-b:boot-1");
    let owner = LeaseOwnerIdentity::opaque("worker-a", "worker-a:boot-1");
    let authority = ClaimAuthority {
        session_id: SessionId::from("head-cas-with-authority"),
        owner,
        executor_id: "owner-executor".to_string(),
        lease_token: "admission".to_string(),
        fencing_token: 1,
    };
    let ((), capture) = capturing(|| async {
        trace_commit_cas_rejected(
            &SessionId::from("head-cas-with-authority"),
            Some(&authority),
            &claimant,
            "claimant-executor",
            &StoreError::HeadRevisionConflict {
                expected: 7,
                actual: 9,
            },
        );
        trace_commit_cas_rejected(
            &SessionId::from("head-cas-with-authority"),
            Some(&authority),
            &claimant,
            "claimant-executor",
            &StoreError::Backend("unrelated backend failure".to_string()),
        );
    })
    .await;

    let rejected = capture.exactly_one("session_head.commit_cas_rejected");
    assert_eq!(rejected.field("owner_id"), "worker-a");
    assert_eq!(rejected.field("incarnation_id"), "worker-a:boot-1");
    assert_eq!(rejected.field("executor_id"), "owner-executor");
    assert_eq!(rejected.field("expected_head_revision"), "7");
    assert_eq!(rejected.field("actual_head_revision"), "9");
}
