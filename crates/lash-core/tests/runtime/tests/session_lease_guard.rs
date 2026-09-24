//! The session-execution-lease guard over a durable store: release that stays
//! retryable until the backend acknowledges it, token-fenced drop release,
//! renewal-failure classification, and borrowed-lane commits.
//!
//! The guard is `lash-core-effect`'s; these laws need a store with real lease
//! semantics, so they run over a SQLite memory backend (ADR 0102) with the
//! recording decorator's lease seams.

use std::sync::Arc;

use crate::runtime::tests::{
    memory_backend, unbound_recording_store, unbound_recording_store_with_clock,
};
use lash_core::SessionId;
use lash_core::facade_support::{LeaseTimings, SystemClock};
use lash_core::runtime::session_execution_lease::{
    SessionExecutionLeaseGuard, commit_runtime_state_with_borrowed_lease,
};
use lash_core::store::{
    RuntimeCommit, RuntimePersistence, SessionCommitStore, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseStore, StoreError,
};
use lash_core::testing::runtime_helpers::{RecordingStore, SessionExecutionLeaseReleaseGate};

/// A clock whose sleeps never return: a guard on it never renews, so its lane
/// lapses on the store's clock.
#[derive(Debug)]
struct NoRenewalClock;

#[async_trait::async_trait]
impl lash_core::Clock for NoRenewalClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        lash_core::Clock::timestamp_datetime(&SystemClock)
    }

    async fn sleep(&self, _duration: std::time::Duration) {
        std::future::pending::<()>().await;
    }

    async fn sleep_until(&self, _deadline: std::time::Instant) {
        std::future::pending::<()>().await;
    }
}

async fn recording_store() -> Arc<RecordingStore> {
    unbound_recording_store(&memory_backend().await).await
}

const SESSION_ID: &str = "cancelled-release";

async fn acquire_gated_guard() -> (
    Arc<RecordingStore>,
    SessionExecutionLeaseGuard,
    Arc<SessionExecutionLeaseReleaseGate>,
) {
    let store = recording_store().await;
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "acquire-gated-guard-executor",
        LeaseTimings::default(),
        Arc::new(SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");
    let gate = store.gate_session_execution_lease_release();
    (store, guard, gate)
}

async fn lease_is_held(store: &Arc<RecordingStore>) -> bool {
    let outcome = store
        .try_claim_session_execution_lease(
            &SessionId::from(SESSION_ID),
            &lash_core::LeaseOwnerIdentity::opaque("peer", "peer-incarnation"),
            "lease-is-held-executor",
            LeaseTimings::default().ttl_ms(),
        )
        .await
        .expect("peer claim attempt");
    matches!(outcome, SessionExecutionLeaseClaimOutcome::Busy { .. })
}

fn borrowed_commit(session_id: &SessionId) -> RuntimeCommit {
    RuntimeCommit::persisted_state_for_test(
        &lash_core::RuntimeSessionState {
            session_id: SessionId::from(session_id.to_string()),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        },
        &[],
    )
}

#[tokio::test]
async fn borrowed_commit_leaves_outer_guard_fence_valid() {
    let store = recording_store().await;
    let persistence: Arc<dyn RuntimePersistence> = store.clone();
    let owner = lash_core::LeaseOwnerIdentity::opaque("borrow-owner", "borrow-incarnation");
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root("borrow-valid"))
        .await
        .expect("bind borrowed-commit session");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&persistence),
        &SessionId::from("borrow-valid"),
        &owner,
        "borrowed-commit-leaves-outer-guard-fence-valid-executor",
        LeaseTimings::default(),
        Arc::new(SystemClock),
    )
    .await
    .expect("claim outer lane")
    .expect("outer lane acquired");

    commit_runtime_state_with_borrowed_lease(
        &guard.borrowed_authority(),
        persistence,
        borrowed_commit(&SessionId::from("borrow-valid")),
        &owner,
    )
    .await
    .expect("borrowed commit succeeds");

    let renewed = store
        .renew_session_execution_lease(&guard.fence(), LeaseTimings::default().ttl_ms())
        .await
        .expect("outer guard remains current after borrowed commit");
    assert_eq!(renewed.lease_token, guard.fence().lease_token);
    guard.release_if_live().await.expect("release outer lane");
}

#[tokio::test]
async fn lapsed_guard_cannot_authorize_borrowed_commit() {
    let store = recording_store().await;
    let persistence: Arc<dyn RuntimePersistence> = store.clone();
    let owner = lash_core::LeaseOwnerIdentity::opaque("lapsed-owner", "lapsed-incarnation");
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root("borrow-lapsed"))
        .await
        .expect("bind lapsed borrowed-commit session");
    let timings = LeaseTimings::from_ttl(std::time::Duration::from_millis(30))
        .expect("valid short lease timings");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&persistence),
        &SessionId::from("borrow-lapsed"),
        &owner,
        "lapsed-guard-cannot-authorize-borrowed-commit-executor",
        timings,
        Arc::new(NoRenewalClock),
    )
    .await
    .expect("claim outer lane")
    .expect("outer lane acquired");
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;

    let error = commit_runtime_state_with_borrowed_lease(
        &guard.borrowed_authority(),
        persistence,
        borrowed_commit(&SessionId::from("borrow-lapsed")),
        &owner,
    )
    .await
    .expect_err("lapsed guard must fail the ordinary execution fence");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
}

/// The release await is the hazardous point: dropping the future there used
/// to mark the lease released without any backend acknowledgement, leaving
/// the durable lease to linger until its TTL with no retry possible.
#[tokio::test]
async fn cancelled_lease_release_stays_retryable_until_the_backend_acknowledges() {
    let (store, guard, gate) = acquire_gated_guard().await;

    let mut release = Box::pin(guard.release_if_live());
    tokio::select! {
        _ = gate.wait_entered() => {}
        result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
    }
    drop(release);

    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        0,
        "the cancelled release never reached the backend"
    );
    assert!(
        lease_is_held(&store).await,
        "an unacknowledged release must not report the lease as free"
    );

    gate.admit_one();
    guard
        .release_if_live()
        .await
        .expect("cancelled release stays retryable");

    assert_eq!(store.session_execution_lease_release_attempt_count(), 1);
    // The peer claim above holds the lease now; assert against the guard's
    // own state instead: a released guard short-circuits further releases.
    gate.admit_one();
    guard
        .release_if_live()
        .await
        .expect("acknowledged release is terminal");
    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        1,
        "an acknowledged release must not be repeated"
    );
}

/// A dropped guard releases out of band with the token of its own claim.
/// When a same-incarnation successor has already rotated the token, the
/// backend refuses the stale release and leaves the successor untouched.
#[tokio::test]
async fn guard_dropped_mid_release_cannot_release_a_successors_lease() {
    let (store, guard, gate) = acquire_gated_guard().await;
    let owner = lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation");

    let mut release = Box::pin(guard.release_if_live());
    tokio::select! {
        _ = gate.wait_entered() => {}
        result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
    }
    drop(release);

    // The same runtime drives again and re-claims the still-live lease.
    let successor_nonce = lash_core::LeaseClaimNonce::for_testing("drop-race-successor-token");
    let successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(SESSION_ID),
            &owner,
            &guard.completion().executor_id,
            &successor_nonce,
            LeaseTimings::default().ttl_ms(),
        )
        .await
        .expect("same-incarnation re-claim")
        .acquired()
        .expect("re-claim refreshes the live lease");
    assert_ne!(
        successor.lease_token,
        guard.completion().lease_token,
        "the successor must rotate the lock-lifecycle token"
    );
    assert_eq!(successor.fencing_token, guard.completion().fencing_token);
    assert_eq!(successor.owner, guard.completion().owner);

    gate.admit_one();
    drop(guard);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_release_attempt_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("best-effort drop release attempted");
    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        1,
        "drop must make one best-effort release attempt"
    );
    assert!(
        lease_is_held(&store).await,
        "the successor's live lease must survive the stale guard's drop"
    );

    // The successor still owns the release.
    gate.admit_one();
    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("successor releases its own lease");
    assert!(!lease_is_held(&store).await);
}

/// A claim rotation or TTL failover may race the successful turn's in-band
/// cleanup. The named refusal proves cleanup is already terminal; surfacing
/// it as `StoreCommitFailed` would falsely report a committed turn as failed.
#[tokio::test]
async fn stale_in_band_release_refusal_is_terminal_and_benign() {
    let store = recording_store().await;
    let owner = lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &owner,
        "stale-in-band-release-refusal-is-terminal-and-benign-executor",
        LeaseTimings::default(),
        Arc::new(SystemClock),
    )
    .await
    .expect("claim predecessor guard")
    .expect("predecessor guard acquired");
    let successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(SESSION_ID),
            &owner,
            &guard.completion().executor_id,
            &lash_core::LeaseClaimNonce::for_testing("in-band-successor-token"),
            LeaseTimings::default().ttl_ms(),
        )
        .await
        .expect("rotate same-incarnation claim")
        .acquired()
        .expect("same-incarnation successor acquired");
    assert_ne!(successor.lease_token, guard.completion().lease_token);

    guard
        .release_if_live()
        .await
        .expect("stale named refusal is terminal and benign");
    assert_eq!(store.session_execution_lease_release_attempt_count(), 1);
    guard
        .release_if_live()
        .await
        .expect("terminal benign refusal is not retried");
    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        1,
        "a terminal named refusal must be acknowledged exactly once"
    );
    assert!(lease_is_held(&store).await);
    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("successor releases its own lease");
}

#[tokio::test]
async fn clean_guard_drop_releases_before_ttl_for_immediate_peer_reclaim() {
    let store = recording_store().await;
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "clean-guard-drop-releases-before-ttl-for-immediate-peer-reclaim-executor",
        LeaseTimings::default(),
        Arc::new(SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");

    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_release_attempt_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("best-effort release completed");

    let peer = store
        .try_claim_session_execution_lease(
            &SessionId::from(SESSION_ID),
            &lash_core::LeaseOwnerIdentity::opaque("peer", "peer-incarnation"),
            "clean-guard-drop-releases-before-ttl-for-immediate-peer-reclaim-executor",
            LeaseTimings::default().ttl_ms(),
        )
        .await
        .expect("peer claim")
        .acquired()
        .expect("clean drop must make the lane immediately reclaimable");
    store
        .release_session_execution_lease(&peer.completion())
        .await
        .expect("peer release");
}

#[tokio::test]
async fn stalled_drop_release_falls_back_to_ttl_without_freeing_the_reclaimer() {
    let clock = Arc::new(lash_core::testing::TestClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&memory_backend().await, store_clock).await;
    let guard_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor",
        LeaseTimings::default(),
        guard_clock,
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");
    let gate = store.gate_session_execution_lease_release();

    drop(guard);
    gate.wait_entered().await;
    let peer_owner = lash_core::LeaseOwnerIdentity::opaque("peer", "peer-incarnation");
    assert!(matches!(
        store
            .try_claim_session_execution_lease(
                &SessionId::from(SESSION_ID),
                &peer_owner,
                "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor",
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("peer before expiry"),
        SessionExecutionLeaseClaimOutcome::Busy { .. }
    ));

    clock.advance(LeaseTimings::default().ttl_ms() + 1);
    let peer = store
        .try_claim_session_execution_lease(
            &SessionId::from(SESSION_ID),
            &peer_owner,
            "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor-2",
            LeaseTimings::default().ttl_ms(),
        )
        .await
        .expect("peer after expiry")
        .acquired()
        .expect("TTL remains the fallback when drop release cannot complete");

    gate.admit_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_release_attempt_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late predecessor release was attempted");
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from(SESSION_ID),
                    &lash_core::LeaseOwnerIdentity::opaque("observer", "observer-incarnation"),
                    "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor-3",
                    LeaseTimings::default().ttl_ms(),
                )
                .await
                .expect("observer claim after stale release"),
            SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "the late predecessor release must not free the TTL reclaimer"
    );

    gate.admit_one();
    store
        .release_session_execution_lease(&peer.completion())
        .await
        .expect("peer release");
}

/// A transient renewal failure does not prove the lease stopped being ours,
/// so it must not be recorded as release completion: the owner still has to
/// ask the backend to release, or a successor waits out the whole TTL.
#[tokio::test]
async fn transient_renewal_failure_still_requires_a_backend_release() {
    let store = recording_store().await;
    let timings = LeaseTimings::new(
        std::time::Duration::from_millis(30),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");
    store.fail_next_session_execution_lease_renewal_with(StoreError::Contended);
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "transient-renewal-failure-still-requires-a-backend-release-executor",
        timings,
        Arc::new(SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal attempt observed");
    assert!(
        !guard.is_lost(),
        "a transient renewal failure must not mark the lease lost"
    );

    guard.release_if_live().await.expect("release");

    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        1,
        "the owner must still ask the backend to release the lease"
    );
    assert!(
        !lease_is_held(&store).await,
        "a successor must not wait out the TTL after a transient renewal failure"
    );
}

/// Refusing a malformed renewal response also leaves the durable lease
/// potentially ours: every backend has already extended its row before
/// returning success, so cleanup must make one token-fenced release attempt.
#[tokio::test]
async fn renewal_install_refusal_still_requires_a_backend_release() {
    let store = recording_store().await;
    let timings = LeaseTimings::new(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "renewal-install-refusal-still-requires-a-backend-release-executor",
        timings,
        Arc::new(SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");
    store.mutate_next_session_execution_lease_renewal(|mut malformed| {
        malformed.lease_token.push_str("-malformed");
        malformed
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !guard.is_lost() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the refused renewal response marks continuity lost");

    guard.release_if_live().await.expect("release");

    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        1,
        "the owner must release after refusing the backend response"
    );
    assert!(
        !lease_is_held(&store).await,
        "a successor must not wait out the TTL after an install refusal"
    );
}

/// The other half of the same rule: a definitive fence rejection *is* loss,
/// and then there is no owner-side release left to perform.
#[tokio::test]
async fn definitive_renewal_fence_rejection_skips_the_owner_side_release() {
    let store = recording_store().await;
    let timings = LeaseTimings::new(
        std::time::Duration::from_millis(30),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");
    store.fail_next_session_execution_lease_renewal_with(
        StoreError::SessionExecutionLeaseExpired {
            session_id: SessionId::from(SESSION_ID.to_string()),
        },
    );
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation"),
        "definitive-renewal-fence-rejection-skips-the-owner-side-release-executor",
        timings,
        Arc::new(SystemClock),
    )
    .await
    .expect("claim lease")
    .expect("lease acquired");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !guard.is_lost() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a rejected renewal fence marks the lease lost");

    guard.release_if_live().await.expect("release");

    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        0,
        "a definitively lost lease has no owner-side release to perform"
    );
}

/// Exercise the real FIG-924-shaped classification path: a same-owner claim
/// rotates durable identity, the old renewal loop receives the new named
/// refusal, marks itself lost, and Drop does not make a doomed release call.
#[tokio::test]
async fn rotated_token_refusal_marks_the_old_renewal_loop_lost() {
    let store = recording_store().await;
    let owner = lash_core::LeaseOwnerIdentity::opaque("owner", "incarnation");
    let timings = LeaseTimings::new(
        std::time::Duration::from_millis(60),
        std::time::Duration::from_millis(10),
    )
    .expect("test lease timings");
    let guard = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        &SessionId::from(SESSION_ID),
        &owner,
        "rotated-token-refusal-marks-the-old-renewal-loop-lost-executor",
        timings,
        Arc::new(SystemClock),
    )
    .await
    .expect("claim predecessor guard")
    .expect("predecessor guard acquired");
    let predecessor_token = guard.completion().lease_token;
    let successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(SESSION_ID),
            &owner,
            &guard.completion().executor_id,
            &lash_core::LeaseClaimNonce::for_testing("renewal-successor-token"),
            60_000,
        )
        .await
        .expect("rotate durable lease token")
        .acquired()
        .expect("same-incarnation successor acquired");
    assert_ne!(successor.lease_token, predecessor_token);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !guard.is_lost() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old renewal loop observes named refusal");
    assert!(store.session_execution_lease_renewal_count() >= 1);
    let release_gate = store.gate_session_execution_lease_release();
    drop(guard);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            release_gate.wait_entered()
        )
        .await
        .is_err(),
        "Drop must not even start a release for a definitively lost guard"
    );
    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        0,
        "Drop must skip a release for a definitively lost guard"
    );
    assert!(lease_is_held(&store).await);
    release_gate.admit_one();
    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("successor releases its own lease");
}
