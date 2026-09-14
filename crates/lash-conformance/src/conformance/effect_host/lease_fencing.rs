use super::*;
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
/// `expire_lease` forces the lease already-expired. Both mutate the same store
/// the controllers share.
pub struct EffectLeaseFencingBackend {
    pub make_controller: EffectLeaseControllerFactory,
    pub steal_lease: EffectLeaseMutator,
    pub expire_lease: EffectLeaseMutator,
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
