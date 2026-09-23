use super::*;
// Only the `#[test]` below uses it, and it is what disambiguates the
// glob's `assert_eq` from the std prelude's, so it is cfg-gated rather
// than dropped.
#[cfg(test)]
use pretty_assertions::assert_eq;

/// One controller bound to a shared durable effect-replay store, paired with a
/// `start_replay` toggle. The toggle exists because `start_replay` is a concrete
/// controller affordance, not a [`RuntimeEffectController`] trait method.
pub struct LeaseFencingController {
    pub controller: Arc<dyn RuntimeEffectController>,
    pub start_replay: Box<dyn Fn() + Send + Sync>,
}

/// A raw mutation applied to the effect-replay row for a given `replay_key`.
/// Backends implement it with a direct row update (SQLite `rusqlite`, Postgres
/// `sqlx`); it is async so Postgres can issue a pooled query.
pub type EffectLeaseMutator = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// Factory that builds a fresh lease-fencing controller bound to one shared
/// durable store with the requested lease TTL. Async so Postgres backends can
/// issue pooled queries during setup.
pub type EffectLeaseControllerFactory = Box<
    dyn Fn(
            std::time::Duration,
            Arc<dyn crate::Clock>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = LeaseFencingController> + Send>>
        + Send
        + Sync,
>;

/// Backend adapter for the effect-replay lease-fencing conformance suite.
///
/// `make_controller` returns a fresh controller bound to one shared durable
/// store with the requested lease TTL. `steal_lease` overwrites the lease
/// owner/token for a `replay_key` (another worker reclaimed the row);
/// `expire_lease` forces the lease already-expired. `fail_renewals` makes
/// every lease renewal of the `replay_key` row fail with a store error (the
/// row and its fence untouched) until `heal_renewals` lifts the fault; claims,
/// takeovers and finalization of the row are unaffected. All of them act on
/// the same store the controllers share.
pub struct EffectLeaseFencingBackend {
    pub make_controller: EffectLeaseControllerFactory,
    pub steal_lease: EffectLeaseMutator,
    pub expire_lease: EffectLeaseMutator,
    pub fail_renewals: EffectLeaseMutator,
    pub heal_renewals: EffectLeaseMutator,
}

/// Clock whose host timestamp and sleep completions are advanced independently.
///
/// The renewal conformance case uses the separate controls to cross the first
/// claim's original expiry on shared-clock stores only after observing a
/// completed renewal cycle. Server-clock stores retain their authoritative
/// lease instant while using the same explicit renewal and backoff gates.
#[derive(Debug)]
struct LeaseFencingClock {
    timestamp_ms: std::sync::atomic::AtomicU64,
    sleep_started: tokio::sync::Semaphore,
    release_sleep: tokio::sync::Semaphore,
}

impl LeaseFencingClock {
    fn new(timestamp_ms: u64) -> Self {
        Self {
            timestamp_ms: std::sync::atomic::AtomicU64::new(timestamp_ms),
            sleep_started: tokio::sync::Semaphore::new(0),
            release_sleep: tokio::sync::Semaphore::new(0),
        }
    }

    fn advance(&self, duration: std::time::Duration) {
        self.timestamp_ms.fetch_add(
            duration.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn await_sleep_started(&self) {
        self.sleep_started
            .acquire()
            .await
            .expect("lease-fencing clock remains open")
            .forget();
    }

    fn release_one_sleep(&self) {
        self.release_sleep.add_permits(1);
    }
}

#[async_trait::async_trait]
impl crate::Clock for LeaseFencingClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.timestamp_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn sleep(&self, _duration: std::time::Duration) {
        self.sleep_started.add_permits(1);
        self.release_sleep
            .acquire()
            .await
            .expect("lease-fencing clock remains open")
            .forget();
    }

    async fn sleep_until(&self, _deadline: std::time::Instant) {
        self.sleep(std::time::Duration::ZERO).await;
    }
}

#[test]
fn lease_fencing_clock_wall_clock_faces_agree() {
    let clock = LeaseFencingClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

fn lease_fencing_system_clock() -> Arc<dyn crate::Clock> {
    Arc::new(crate::facade_support::SystemClock)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn lease_fencing_envelope(replay_key: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(ExecutionScope::turn("session", "turn"), replay_key)
                .expect("valid lease-fencing address"),
            RuntimeAttribution::for_turn("effect-lease-session", "effect-lease-turn", 1, 0),
            replay_key,
        ),
        RuntimeEffectCommand::ExecCode {
            language: "code".to_string(),
            code: "emit".to_string(),
        },
    )
}

/// Run the durable effect-replay lease-fencing conformance suite. Every durable
/// effect-replay controller (SQLite, Postgres, ...) must satisfy the same
/// fencing contract, so the row-level renewal/steal/expiry behavior lives here
/// once instead of in store-specific raw-row tests:
///
/// - a renewed in-progress lease keeps a competing claimant out, then replays;
/// - a stolen lease aborts the original owner with a lease-lost error;
/// - a lease that expires before finalize is rejected with a lease-lost error;
/// - a successor reclaims and executes an effect after its predecessor's lease
///   is explicitly expired.
pub async fn effect_controller_lease_fencing(backend: EffectLeaseFencingBackend) {
    let run = uuid::Uuid::new_v4().to_string();
    lease_fencing_renews_long_running_lease(&backend, &run).await;
    lease_fencing_reports_lease_lost_when_stolen(&backend, &run).await;
    lease_fencing_rejects_finalize_after_expiry(&backend, &run).await;
    lease_fencing_reclaims_explicitly_expired_lease(&backend, &run).await;
    lease_fencing_rejects_stale_derivation_release(&backend, &run).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn lease_fencing_renews_long_running_lease(backend: &EffectLeaseFencingBackend, run: &str) {
    let ttl = std::time::Duration::from_millis(300);
    let renew_interval = ttl / 3;
    let replay_key = format!("lease-renewal-{run}");
    let initial_timestamp = crate::ClockWallTime::timestamp_ms(&crate::facade_support::SystemClock);
    let clock = Arc::new(LeaseFencingClock::new(initial_timestamp));
    let first = (backend.make_controller)(ttl, clock.clone()).await;
    let second = (backend.make_controller)(ttl, clock.clone()).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let first_controller = Arc::clone(&first.controller);
    let first_envelope = envelope.clone();
    let first_release = Arc::clone(&release);
    let first_task = crate::task::spawn(async move {
        first_controller
            .execute_effect(
                first_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    first_release.notified().await;
                    Ok(replay_conformance_exec_outcome("renewed-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("first executor entered");

    // Deliberately complete one renewal cycle, then move the injected timestamp
    // to the original claim's expiry. Shared-clock stores cross that expiry;
    // server-clock stores keep their authoritative lease time. The second
    // observed sleep is the renewal loop re-arming only after the backend
    // accepted and persisted the renewal.
    clock.await_sleep_started().await;
    clock.advance(renew_interval);
    clock.release_one_sleep();
    clock.await_sleep_started().await;
    clock.advance(ttl - renew_interval);

    // A busy observation enters the injected backoff gate. If the renewal did
    // not preserve the lease, the competing executor enters instead and trips
    // the same property assertion as the original wall-clock-raced case.
    let (competing_entered_tx, competing_entered_rx) = tokio::sync::oneshot::channel();
    let competing_controller = Arc::clone(&second.controller);
    let competing_envelope = envelope.clone();
    let competing_task = crate::task::spawn(async move {
        competing_controller
            .execute_effect(
                competing_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = competing_entered_tx.send(());
                    Ok(replay_conformance_exec_outcome("stolen-owner"))
                }),
            )
            .await
    });
    tokio::select! {
        () = clock.await_sleep_started() => {}
        entered = competing_entered_rx => {
            entered.expect("competing executor admission signal");
            panic!("renewed in-progress lease should keep a competing claimant busy");
        }
    }
    competing_task.abort();
    assert!(
        competing_task.await.is_err(),
        "busy competing claimant task aborts"
    );

    release.notify_waiters();
    let first_outcome = first_task
        .await
        .expect("first task joins")
        .expect("renewed owner finalizes");
    assert_replay_conformance_exec_marker(first_outcome, "renewed-owner");

    (second.start_replay)();
    let replayed = second
        .controller
        .execute_effect(
            envelope,
            replay_conformance_failing_executor(Arc::new(Mutex::new(Vec::new()))),
        )
        .await
        .expect("replayed renewed outcome");
    assert_replay_conformance_exec_marker(replayed, "renewed-owner");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn lease_fencing_reports_lease_lost_when_stolen(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    let ttl = std::time::Duration::from_millis(300);
    let replay_key = format!("lease-stolen-{run}");
    let controller = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let never_release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&controller.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&never_release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("should-not-finalize"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    (backend.steal_lease)(replay_key.clone()).await;

    let err = tokio::time::timeout(std::time::Duration::from_secs(2), owner_task)
        .await
        .expect("renewal should notice the stolen lease")
        .expect("owner task joins")
        .expect_err("stolen lease must fail the original owner");
    assert!(
        err.code.as_str().ends_with("_effect_replay_lease_lost"),
        "expected an effect-replay lease-lost error, got code `{}`: {}",
        err.code,
        err.message,
    );
    let _keep_notify_alive = never_release;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn lease_fencing_rejects_finalize_after_expiry(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // A long TTL keeps the renewal task idle during the brief block so the
    // finalize path (not renewal) is the one that observes the expired lease.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-expiry-{run}");
    let controller = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&controller.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("expired-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    (backend.expire_lease)(replay_key.clone()).await;
    release.notify_waiters();

    let err = owner_task
        .await
        .expect("owner task joins")
        .expect_err("expired lease must not finalize");
    assert!(
        err.code.as_str().ends_with("_effect_replay_lease_lost"),
        "expected an effect-replay lease-lost error, got code `{}`: {}",
        err.code,
        err.message,
    );
}

/// A successor must reclaim and execute an effect after its predecessor's
/// lease is explicitly expired.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn lease_fencing_reclaims_explicitly_expired_lease(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    // Keep the successor's lease independent of scheduler timing. The test
    // expires the predecessor through the backend affordance below; a short
    // real TTL would only race successor renewal/finalization under load.
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-explicit-reclaim-{run}");
    let vanished = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let successor = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let never_release = Arc::new(tokio::sync::Notify::new());
    let owner = Arc::clone(&vanished.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&never_release);
    let owner_task = crate::task::spawn(async move {
        owner
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("vanished-owner"))
                }),
            )
            .await
    });
    entered_rx.await.expect("vanished executor entered");
    // Kill the first owner mid-claim so it cannot renew or finalize. The
    // backend affordance then drives expiry as an explicit test event.
    owner_task.abort();
    assert!(owner_task.await.is_err(), "vanished owner task aborts");

    (backend.expire_lease)(replay_key.clone()).await;

    let reclaimed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        successor.controller.execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                Ok(replay_conformance_exec_outcome("successor-owner"))
            }),
        ),
    )
    .await
    .expect("a successor must reclaim the abandoned row after its lease is explicitly expired")
    .expect("successor executes the reclaimed effect");
    assert_replay_conformance_exec_marker(reclaimed, "successor-owner");
    let _keep_notify_alive = never_release;
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture asserts its public outcomes"
)]
async fn lease_fencing_rejects_stale_derivation_release(
    backend: &EffectLeaseFencingBackend,
    run: &str,
) {
    let ttl = std::time::Duration::from_secs(30);
    let key = format!("derivation-release-{run}");
    let first = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let successor = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let mut envelope = lease_fencing_envelope(&key);
    envelope.command = RuntimeEffectCommand::AssistantResponseHooks {
        response: Box::default(),
    };
    let (first_entered, entered) = tokio::sync::oneshot::channel();
    let (fail, failing) = tokio::sync::oneshot::channel();
    let first_envelope = envelope.clone();
    let stale = crate::task::spawn(async move {
        first
            .controller
            .execute_effect(
                first_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    first_entered.send(()).expect("first entered");
                    failing.await.expect("fail first");
                    Err(RuntimeEffectControllerError::retryable_response_derivation(
                        "stale derivation",
                    ))
                }),
            )
            .await
    });
    entered.await.expect("first claimed");
    (backend.expire_lease)(key).await;
    let (successor_entered, entered) = tokio::sync::oneshot::channel();
    let (finish, finishing) = tokio::sync::oneshot::channel();
    let successor_task = crate::task::spawn(async move {
        successor
            .controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    successor_entered.send(()).expect("successor entered");
                    finishing.await.expect("finish successor");
                    Ok(RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::default(),
                        events: Vec::new(),
                    })
                }),
            )
            .await
    });
    entered.await.expect("successor claimed");
    fail.send(()).expect("fail predecessor");
    let error = stale
        .await
        .expect("join predecessor")
        .expect_err("stale release refused");
    assert!(
        error.code.as_str().ends_with("_effect_replay_lease_lost"),
        "{error}"
    );
    finish.send(()).expect("finish successor");
    successor_task
        .await
        .expect("join successor")
        .expect("stale owner must not expire the successor's claim");
}

/// A transient renewal failure proves no lease loss: the running tool must
/// survive it, and the row must finalize `completed` with the tool's own
/// outcome (FIG-3512). The TTL is long so the renewal budget cannot be what
/// this case observes; the injected clock gates each renewal explicitly.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn effect_lease_renew_transient_error_keeps_tool_running(
    backend: EffectLeaseFencingBackend,
) {
    let ttl = std::time::Duration::from_secs(30);
    let replay_key = format!("lease-renew-transient-{}", uuid::Uuid::new_v4());
    let initial_timestamp = crate::ClockWallTime::timestamp_ms(&crate::facade_support::SystemClock);
    let clock = Arc::new(LeaseFencingClock::new(initial_timestamp));
    let owner = (backend.make_controller)(ttl, clock.clone()).await;
    let replayer = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let envelope = lease_fencing_envelope(&replay_key);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let owner_controller = Arc::clone(&owner.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&release);
    let owner_task = crate::task::spawn(async move {
        owner_controller
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("survived-renew-fault"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    // The first renewal runs against a failing store. The renewal loop
    // re-arming afterwards is the observation that the failure was absorbed
    // rather than ending the execution.
    let rearmed = std::time::Duration::from_secs(5);
    clock.await_sleep_started().await;
    (backend.fail_renewals)(replay_key.clone()).await;
    clock.release_one_sleep();
    tokio::time::timeout(rearmed, clock.await_sleep_started())
        .await
        .expect("a failed renewal must keep the renewal cadence and the tool running");
    assert!(
        !owner_task.is_finished(),
        "a transient renewal failure must not end the running tool"
    );

    // The store recovers and the next renewal lands.
    (backend.heal_renewals)(replay_key.clone()).await;
    clock.release_one_sleep();
    tokio::time::timeout(rearmed, clock.await_sleep_started())
        .await
        .expect("the healed renewal keeps the cadence");

    release.notify_one();
    let outcome = owner_task
        .await
        .expect("owner task joins")
        .expect("the tool outlives a transient renewal failure and finalizes");
    assert_replay_conformance_exec_marker(outcome, "survived-renew-fault");

    // The row finalized `completed` with the tool's outcome: a replay reads it
    // back without executing.
    (replayer.start_replay)();
    let replayed = replayer
        .controller
        .execute_effect(
            envelope,
            replay_conformance_failing_executor(Arc::new(Mutex::new(Vec::new()))),
        )
        .await
        .expect("replayed completed outcome");
    assert_replay_conformance_exec_marker(replayed, "survived-renew-fault");
}

/// Signals when the executing effect future is dropped.
struct DroppedSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DroppedSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

/// Renewals that keep failing spend the lease's miss budget: the owner drops
/// the executing tool and reports a lease-lost controller error, but a store
/// error is never sealed as the effect's `failed` terminal — the row stays
/// reclaimable, and the next claim of the address re-executes the effect
/// (FIG-3512).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn effect_lease_renew_errors_past_budget_leave_row_reclaimable(
    backend: EffectLeaseFencingBackend,
) {
    let ttl = std::time::Duration::from_millis(300);
    let replay_key = format!("lease-renew-exhausted-{}", uuid::Uuid::new_v4());
    let owner = (backend.make_controller)(ttl, lease_fencing_system_clock()).await;
    let successor = (backend.make_controller)(
        std::time::Duration::from_secs(30),
        lease_fencing_system_clock(),
    )
    .await;
    let envelope = lease_fencing_envelope(&replay_key);

    // Every renewal of this row fails from the start; the claim itself is
    // unaffected.
    (backend.fail_renewals)(replay_key.clone()).await;

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let never_release = Arc::new(tokio::sync::Notify::new());
    let owner_controller = Arc::clone(&owner.controller);
    let owner_envelope = envelope.clone();
    let owner_release = Arc::clone(&never_release);
    let owner_task = crate::task::spawn(async move {
        owner_controller
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    let _dropped = DroppedSignal(Some(dropped_tx));
                    let _ = entered_tx.send(());
                    owner_release.notified().await;
                    Ok(replay_conformance_exec_outcome("should-not-finalize"))
                }),
            )
            .await
    });
    entered_rx.await.expect("owner executor entered");

    let err = tokio::time::timeout(std::time::Duration::from_secs(10), owner_task)
        .await
        .expect("renewal failures past the budget must end the execution")
        .expect("owner task joins")
        .expect_err("an execution whose lease could not be renewed must not finalize");
    assert!(
        err.code.as_str().ends_with("_effect_replay_lease_lost"),
        "expected an effect-replay lease-lost controller error, got code `{}`: {}",
        err.code,
        err.message,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
        .await
        .expect("the abandoned execution drops the running tool")
        .expect("drop signal delivered");

    (backend.heal_renewals)(replay_key.clone()).await;
    let reclaimed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        successor.controller.execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                Ok(replay_conformance_exec_outcome("reclaimed-owner"))
            }),
        ),
    )
    .await
    .expect("a later claim must reclaim the abandoned row once its lease expires")
    .expect("the reclaimed effect re-executes instead of replaying a sealed error");
    assert_replay_conformance_exec_marker(reclaimed, "reclaimed-owner");
    let _keep_notify_alive = never_release;
}
