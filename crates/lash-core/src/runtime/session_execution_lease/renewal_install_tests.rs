use super::*;
use crate::runtime::in_memory_store::InMemorySessionStore;
use crate::runtime::tests::trace_capture::{CapturedFieldKind, capturing};

const TEST_SESSION_ID: &str = "renewal-install-validation";

fn resident_lease(guard: &SessionExecutionLeaseGuard) -> SessionExecutionLease {
    guard.lease.lock_recover().clone()
}

async fn wait_until(predicate: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal task reached the expected terminal state");
}

async fn assert_renewal_response_refused(
    mutate: fn(&SessionExecutionLease, &mut SessionExecutionLease),
    expected: crate::SessionExecutionLeaseRenewalInstallMismatch,
    refusal_cause: &str,
) {
    let store = Arc::new(InMemorySessionStore::new());
    let timings = LeaseTimings::new(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");

    let ((guard, presented), capture) = capturing(|| async {
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            TEST_SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");
        // The renewal task starts with the guard and can advance the resident
        // lease on another runtime thread. Snapshot it only when the override
        // is ready to be armed so the refusal assertion pins the actual
        // presented lease rather than the acquire-time response.
        let presented = resident_lease(&guard);
        let mut response = presented.clone();
        response.expires_at_epoch_ms = response.expires_at_epoch_ms.saturating_add(1_000);
        mutate(&presented, &mut response);
        store.respond_to_next_session_execution_lease_renewal_with(response.clone());
        assert_eq!(
            validate_renewed_session_execution_lease(&presented, &response),
            Err(expected),
            "the pure validator names the mismatched field"
        );
        wait_until(|| guard.is_lost()).await;
        (guard, presented)
    })
    .await;

    assert!(guard.is_lost(), "a malformed renewal response marks loss");
    assert_eq!(
        resident_lease(&guard),
        presented,
        "a malformed renewal response must never replace the resident fence"
    );
    assert_eq!(
        guard.continuity(),
        None,
        "the existing lease-lost machinery denies further continuity"
    );

    let refusal = capture.exactly_one("session_execution_lease.renewal_install_refused");
    assert_eq!(refusal.target, "lash_core::session_execution_lease");
    assert_eq!(refusal.level, "WARN");
    assert_eq!(refusal.field_count(), 24);
    for field in [
        "event",
        "operation",
        "decision_basis",
        "session_id",
        "presented_owner_id",
        "presented_incarnation_id",
        "current_owner_id",
        "current_incarnation_id",
        "current_token_identity",
        "presented_token_identity",
        "consulted_state",
        "observation_freshness",
        "outcome",
        "refusal_cause",
    ] {
        assert_eq!(refusal.field_kind(field), CapturedFieldKind::Str, "{field}");
    }
    for field in ["owner_matched", "token_matched"] {
        assert_eq!(
            refusal.field_kind(field),
            CapturedFieldKind::Bool,
            "{field}"
        );
    }
    for field in [
        "session_matched",
        "current_fencing_token",
        "generation_matched",
        "current_expires_at_epoch_ms",
        "observed_at_epoch_ms",
        "minimum_expires_at_epoch_ms",
        "expiry_matched",
    ] {
        assert_eq!(
            refusal.field_kind(field),
            CapturedFieldKind::Debug,
            "{field}"
        );
    }
    assert_eq!(
        refusal.field_kind("presented_fencing_token"),
        CapturedFieldKind::U64
    );
    assert_eq!(refusal.field("operation"), "renewal_install");
    assert_eq!(
        refusal.field("decision_basis"),
        "core_renewal_install_validation"
    );
    assert_eq!(refusal.field("refusal_cause"), refusal_cause);
    assert_eq!(refusal.field("outcome"), "refused");
    assert_eq!(refusal.field("consulted_state"), "backend_renewal_response");
    assert_eq!(
        refusal.field("observation_freshness"),
        "backend_renewal_response"
    );
    assert_eq!(
        refusal.field("session_matched"),
        format!(
            "Some({})",
            expected != crate::SessionExecutionLeaseRenewalInstallMismatch::Session
        )
    );
    assert_eq!(
        refusal.field("owner_matched"),
        (expected != crate::SessionExecutionLeaseRenewalInstallMismatch::OwnerIncarnation)
            .to_string()
    );
    assert_eq!(
        refusal.field("token_matched"),
        (expected != crate::SessionExecutionLeaseRenewalInstallMismatch::LeaseToken).to_string()
    );
    assert_eq!(
        refusal.field("generation_matched"),
        format!(
            "Some({})",
            expected != crate::SessionExecutionLeaseRenewalInstallMismatch::FencingToken
        )
    );
    assert_eq!(
        refusal.field("expiry_matched"),
        format!(
            "Some({})",
            expected != crate::SessionExecutionLeaseRenewalInstallMismatch::ExpiryRegressed
        )
    );
    assert_eq!(
        refusal.field("minimum_expires_at_epoch_ms"),
        format!("Some({})", presented.expires_at_epoch_ms)
    );
    assert_ne!(refusal.field("observed_at_epoch_ms"), "None");

    let lost = capture.exactly_one("session_execution_lease.lost");
    assert_eq!(lost.field("outcome"), "lease_lost");
    assert_eq!(lost.field("consulted"), "renewal_response_refused");
    assert!(
        lost.field("error").contains(&expected.to_string()),
        "the terminal loss evidence names the typed mismatch: {lost:?}"
    );
}

#[tokio::test]
async fn renewal_with_rotated_fencing_token_marks_lost_and_never_installs() {
    assert_renewal_response_refused(
        |_, response| response.fencing_token = response.fencing_token.saturating_add(1),
        crate::SessionExecutionLeaseRenewalInstallMismatch::FencingToken,
        "fencing_token",
    )
    .await;
}

#[tokio::test]
async fn renewal_with_rotated_lease_token_marks_lost_and_never_installs() {
    assert_renewal_response_refused(
        |_, response| response.lease_token.push_str("-rotated"),
        crate::SessionExecutionLeaseRenewalInstallMismatch::LeaseToken,
        "lease_token",
    )
    .await;
}

#[tokio::test]
async fn renewal_with_wrong_owner_incarnation_marks_lost_and_never_installs() {
    assert_renewal_response_refused(
        |_, response| response.owner.incarnation_id = "other-incarnation".to_string(),
        crate::SessionExecutionLeaseRenewalInstallMismatch::OwnerIncarnation,
        "owner_incarnation",
    )
    .await;
}

#[tokio::test]
async fn renewal_with_wrong_session_marks_lost_and_never_installs() {
    assert_renewal_response_refused(
        |_, response| response.session_id = "other-session".to_string(),
        crate::SessionExecutionLeaseRenewalInstallMismatch::Session,
        "session",
    )
    .await;
}

#[tokio::test]
async fn renewal_with_regressed_expiry_marks_lost_and_never_installs() {
    assert_renewal_response_refused(
        |presented, response| {
            response.expires_at_epoch_ms = presented.expires_at_epoch_ms.saturating_sub(1);
        },
        crate::SessionExecutionLeaseRenewalInstallMismatch::ExpiryRegressed,
        "expiry",
    )
    .await;
}

#[tokio::test]
async fn renewal_with_advanced_expiry_installs() {
    let store = Arc::new(InMemorySessionStore::new());
    let timings = LeaseTimings::new(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        TEST_SESSION_ID,
        &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        timings,
        Arc::new(crate::runtime::SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");
    let presented = resident_lease(&guard);
    let mut renewed = presented.clone();
    renewed.expires_at_epoch_ms = renewed.expires_at_epoch_ms.saturating_add(1_000);
    assert_eq!(
        validate_renewed_session_execution_lease(&presented, &renewed),
        Ok(())
    );
    store.respond_to_next_session_execution_lease_renewal_with(renewed.clone());

    wait_until(|| resident_lease(&guard) == renewed).await;
    guard.renew_task.abort();

    assert!(!guard.is_lost());
    assert_eq!(resident_lease(&guard), renewed);
}
