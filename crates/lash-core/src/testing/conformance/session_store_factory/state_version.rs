use std::sync::Arc;

use super::session_store_request;

/// The ADR 0077 marker is read before guarded payloads, and its admission seam
/// is unavailable until the caller owns the session execution lease.
pub(super) async fn session_state_version_admission_contract(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "session-state-version-admission",
        "session-state-version-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session-state marker fixture");
    assert_eq!(
        store
            .read_session_state_version()
            .await
            .expect("read fresh marker"),
        crate::store::CURRENT_SESSION_STATE_VERSION
    );

    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed a guarded session head payload");
    store
        .stamp_session_state_version_and_corrupt_payload_for_testing(
            crate::store::CURRENT_SESSION_STATE_VERSION + 1,
        )
        .await
        .expect("stamp newer marker above an undecodable payload");

    let owner = crate::LeaseOwnerIdentity::opaque("state-admission-owner", "incarnation");
    let no_lease = crate::SessionExecutionLeaseAuthority {
        session_id: request.session_id.clone(),
        owner: owner.clone(),
        executor_id: "state-admission-executor".to_string(),
        lease_token: "not-a-live-token".to_string(),
        fencing_token: 1,
    };
    let ordering_error = store
        .admit_session_state(&no_lease)
        .await
        .expect_err("admission must validate the lease before consulting migration state");
    assert!(
        matches!(
            ordering_error,
            crate::StoreError::SessionExecutionLeaseExpired { .. }
                | crate::StoreError::SessionExecutionLeaseRenewalRefused { .. }
        ),
        "lease validation must precede the marker gate: {ordering_error:?}"
    );

    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &owner,
            "state-admission-executor",
            60_000,
        )
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease acquired");
    let recovery_error = store
        .load_session()
        .await
        .expect_err("recovery must stop at the marker before decoding the corrupt head");
    assert!(
        matches!(
            &recovery_error,
            crate::StoreError::SessionStateVersionNewerThanRuntime {
                found,
                current,
            } if *found == *current + 1
        ),
        "newer marker must win over payload decoding, got {recovery_error:?}"
    );
    let admission_error = store
        .admit_session_state(&lease.fence())
        .await
        .expect_err("newer session generation must refuse admission");
    assert!(matches!(
        admission_error,
        crate::StoreError::SessionStateVersionNewerThanRuntime {
            found,
            current,
        } if found == current + 1
    ));
}
