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
//! CAS under test is `release_state`'s. Its lane lives in a test-local lease
//! port, since the guard reaches the store only through that port.

use super::*;
use crate::store::{SessionExecutionLeaseObservation, SessionExecutionLeaseStore};

/// A test-local lease port: one lane row held by the guard under test. The
/// model checks the guard's release CAS, so the port answers exactly the
/// release a durable backend gives — the holder's first release clears the
/// row, and any later one is the idempotent refusal — and nothing else.
struct LoomLeaseStub {
    held: StdMutex<Option<SessionExecutionLease>>,
}

impl LoomLeaseStub {
    fn holding(lease: SessionExecutionLease) -> Self {
        Self {
            held: StdMutex::new(Some(lease)),
        }
    }
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for LoomLeaseStub {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        _session_id: &SessionId,
        _owner: &crate::LeaseOwnerIdentity,
        _executor_id: &str,
        _claim_nonce: &crate::LeaseClaimNonce,
        _lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        unreachable!("the model builds its guard around an already-held lane")
    }

    async fn renew_session_execution_lease(
        &self,
        _fence: &SessionExecutionLeaseAuthority,
        _lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        unreachable!("the model's renewal task never runs")
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let mut held = self.held.lock_recover();
        match held.as_ref() {
            Some(lease) if lease.lease_token == completion.lease_token => {
                *held = None;
                Ok(())
            }
            _ => Err(StoreError::SessionExecutionLeaseReleaseRefused {
                session_id: completion.session_id.clone(),
            }),
        }
    }

    async fn get_session_execution_lease(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionExecutionLeaseObservation, StoreError> {
        unreachable!("the model never reads the lane")
    }
}

/// A guard around a lane the stub holds.
fn claimed_guard(
    renew_task: tokio::task::JoinHandle<()>,
    loss_cause: u8,
) -> SessionExecutionLeaseGuard {
    let lease = SessionExecutionLease {
        session_id: SessionId::from("loom-lease"),
        owner: crate::LeaseOwnerIdentity::opaque("loom-owner", "loom-incarnation"),
        executor_id: "loom-executor".to_string(),
        lease_token: "loom-lease-token".to_string(),
        fencing_token: 1,
        claimed_at_epoch_ms: 0,
        lease_term_ms: LeaseTimings::default().ttl_ms(),
        expires_at_epoch_ms: LeaseTimings::default().ttl_ms(),
    };
    SessionExecutionLeaseGuard {
        store: Arc::new(LoomLeaseStub::holding(lease.clone())),
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
        let guard = Arc::new(claimed_guard(
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
        let guard = Arc::new(claimed_guard(
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
        let guard = Arc::new(claimed_guard(
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
