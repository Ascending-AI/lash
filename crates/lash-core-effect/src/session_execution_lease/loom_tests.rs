//! Loom model checks for the guard's release-state CAS (FIG-1161 seam 4).
//!
//! Scope, stated plainly: the lease is *advisory*. It serializes the common
//! case so two runners do not duplicate work, but the durable commit/head CAS
//! remains the only authority on who publishes (ADR 0029). These models check
//! only the guard's atomic release transition converging to `RELEASED` under
//! racing release paths; nothing here asserts anything about commit order.
//!
//! The guard is constructed directly rather than through `from_acquisition`
//! because the renewal task's spawn is a runtime duty outside this seam; the
//! CAS under test is `release_state`'s.

use super::*;
use crate::store::{
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore,
};
use lash_core_memory::in_memory_store::InMemorySessionStore;

/// Claim a real durable row so `release_if_live`'s backend call succeeds,
/// then build the guard around it.
fn claimed_guard(
    store: &Arc<InMemorySessionStore>,
    renew_task: tokio::task::JoinHandle<()>,
    loss_cause: u8,
) -> SessionExecutionLeaseGuard {
    let owner = crate::LeaseOwnerIdentity::opaque("loom-owner", "loom-incarnation");
    let outcome = loom::future::block_on(store.try_claim_session_execution_lease_with_token(
        &SessionId::from("loom-lease"),
        &owner,
        "loom-executor",
        &crate::LeaseClaimNonce::new(),
        LeaseTimings::default().ttl_ms(),
    ));
    let SessionExecutionLeaseClaimOutcome::Acquired(SessionExecutionLeaseAcquisition {
        lease, ..
    }) = outcome.expect("a fresh store claim cannot fail")
    else {
        unreachable!("a fresh store claim cannot be busy")
    };
    SessionExecutionLeaseGuard {
        store: Arc::clone(store) as Arc<dyn RuntimePersistence>,
        lease: Arc::new(StdMutex::new(lease)),
        release_state: Arc::new(AtomicU8::new(release_state::LIVE)),
        loss_cause: Arc::new(AtomicU8::new(loss_cause)),
        clock: Arc::new(lash_core_ids::clock::SystemClock),
        guard_id: NEXT_LEASE_GUARD_ID.fetch_add(1, Ordering::Relaxed),
        renew_task,
    }
}

fn released(guard: &SessionExecutionLeaseGuard) -> bool {
    guard.release_state.load(Ordering::Acquire) == release_state::RELEASED
}

/// `release_if_live`'s `LIVE -> RELEASING` CAS racing `mark_released`'s
/// `RELEASED` install: whoever lands first wins; the loser observes the
/// terminal state and returns. The model must never leave the guard
/// mid-transition.
#[test]
fn release_if_live_racing_mark_released_converges_to_released() {
    loom::model(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime for the renewal-task stand-in");
        let _entered = rt.enter();
        let store = Arc::new(InMemorySessionStore::new());
        let guard = Arc::new(claimed_guard(
            &store,
            rt.spawn(std::future::pending()),
            loss_cause::NONE,
        ));

        let releaser = loom::thread::spawn({
            let guard = Arc::clone(&guard);
            move || {
                loom::future::block_on(guard.release_if_live()).expect("in-band release resolves")
            }
        });
        guard.mark_released();
        releaser.join().expect("releaser thread panicked");

        assert!(released(&guard), "the release transition must converge");
    });
}

/// Two in-band releases race the same CAS: the loser falls through the
/// `Err(RELEASING)` arm and retries the identical token-fenced release —
/// the backend's refusal is idempotent cleanup, so both finish and the
/// terminal state is `RELEASED`.
#[test]
fn concurrent_release_if_live_attempts_converge_to_released() {
    loom::model(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime for the renewal-task stand-in");
        let _entered = rt.enter();
        let store = Arc::new(InMemorySessionStore::new());
        let guard = Arc::new(claimed_guard(
            &store,
            rt.spawn(std::future::pending()),
            loss_cause::NONE,
        ));

        let a = loom::thread::spawn({
            let guard = Arc::clone(&guard);
            move || {
                loom::future::block_on(guard.release_if_live()).expect("in-band release resolves")
            }
        });
        let b = loom::thread::spawn({
            let guard = Arc::clone(&guard);
            move || {
                loom::future::block_on(guard.release_if_live()).expect("in-band release resolves")
            }
        });
        a.join().expect("releaser thread panicked");
        b.join().expect("releaser thread panicked");

        assert!(released(&guard), "the release transition must converge");
    });
}

/// A definitive fence-loss verdict observed after the CAS skips the
/// backend release entirely; racing that path against a second release
/// still converges to `RELEASED`.
#[test]
fn store_verdict_release_racing_attempt_skips_backend_and_converges() {
    loom::model(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime for the renewal-task stand-in");
        let _entered = rt.enter();
        let store = Arc::new(InMemorySessionStore::new());
        let guard = Arc::new(claimed_guard(
            &store,
            rt.spawn(std::future::pending()),
            loss_cause::STORE_VERDICT,
        ));

        let releaser = loom::thread::spawn({
            let guard = Arc::clone(&guard);
            move || {
                loom::future::block_on(guard.release_if_live()).expect("in-band release resolves")
            }
        });
        loom::future::block_on(guard.release_if_live()).expect("in-band release resolves");
        releaser.join().expect("releaser thread panicked");

        assert!(released(&guard), "the release transition must converge");
    });
}
