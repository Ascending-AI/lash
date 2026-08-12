//! Backend-neutral laws for the session-execution-lease renewal write.

use std::sync::Arc;

use crate::{RuntimePersistence, SessionExecutionLease, StoreError};

/// Backend-specific control for forcing an already-validated conditional
/// renewal write to affect no row.
///
/// SQL backends should suppress the actual `UPDATE`; stores without SQL should
/// emulate the same compare-and-set refusal seam. The conformance law owns the
/// assertion so every backend is held to one typed contract.
#[async_trait::async_trait]
pub trait SessionExecutionLeaseRenewalZeroRowInjector: Send + Sync {
    async fn arm(&self, session_id: &str);

    async fn disarm(&self);
}

pub struct SessionExecutionLeaseRenewalZeroRowHandles {
    pub store: Arc<dyn RuntimePersistence>,
    pub injector: Arc<dyn SessionExecutionLeaseRenewalZeroRowInjector>,
}

/// ADR 0053 law: a conditional renewal write affecting no row is a named
/// refusal, never a successful renewal.
pub async fn session_execution_lease_zero_row_renewal_is_refused(
    handles: SessionExecutionLeaseRenewalZeroRowHandles,
) {
    const SESSION_ID: &str = "zero-row-session-lease-renewal";
    let owner = crate::LeaseOwnerIdentity::opaque("zero-row-owner", "zero-row-incarnation");
    let held = handles
        .store
        .try_claim_session_execution_lease(
            SESSION_ID,
            &owner,
            "session-execution-lease-zero-row-renewal-is-refused-executor",
            120_000,
        )
        .await
        .expect("claim zero-row renewal lease")
        .acquired()
        .expect("zero-row renewal lease acquired");

    handles.injector.arm(SESSION_ID).await;
    let renewal = handles
        .store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await;
    handles.injector.disarm().await;

    assert!(
        matches!(
            renewal,
            Err(StoreError::SessionExecutionLeaseRenewalRefused { ref session_id })
                if session_id == SESSION_ID
        ),
        "a zero-row conditional renewal must return the named refusal, got {renewal:?}"
    );
    let durable = handles
        .store
        .get_session_execution_lease(SESSION_ID)
        .await
        .expect("read lease after refused zero-row renewal")
        .expect("refused zero-row renewal preserves the current lease");
    assert_same_lease(&durable, &held);
}

fn assert_same_lease(actual: &SessionExecutionLease, expected: &SessionExecutionLease) {
    assert_eq!(actual.session_id, expected.session_id);
    assert_eq!(actual.owner, expected.owner);
    assert_eq!(actual.lease_token, expected.lease_token);
    assert_eq!(actual.fencing_token, expected.fencing_token);
    assert_eq!(actual.claimed_at_epoch_ms, expected.claimed_at_epoch_ms);
    assert_eq!(actual.expires_at_epoch_ms, expected.expires_at_epoch_ms);
}
