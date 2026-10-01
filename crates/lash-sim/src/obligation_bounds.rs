//! ADR 0109 §1.8 detection bounds of the `ControlIntent` obligation (FIG-3600
//! S8-C), asserted on the simulation's virtual clock.
//!
//! A session's close records its `CloseSession` intent and arms the intent
//! row's obligation in one transaction; the intent's engine half is then
//! delivered by the `ControlIntent` relay — immediately by the verb, and by
//! the reconcile tick's due pass after that. Each law drives the real relay
//! over a SQLite store set on a [`SimClock`], ticking every `T` = 10 s ±10%,
//! and measures when each delivery attempt reached the engine half.
//!
//! The SQLite leader-failover leg of the lost-attempt bound (`+ 20.5 s`) is
//! the recovery lease's to assert; a tick here is always the leader's.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::drive::relay::{ObligationRelay, RelayPolicy, relay_due};
use lash_core::drive::{ControlIntentRelay, ScopeCloseRelay};
use lash_core::engine::{
    EngineAck, EnginePage, EngineRefusal, ParkReconcileReport, ParkRecoveryWriter, RootRef,
    ScopeCloseSink, SessionControlEngine,
};
use lash_core::store::{
    ControlIntent, ControlIntentId, ControlIntentState, ObligationKind, ObligationState,
    RootTerminal, StallReason,
};
use lash_core::store::{ControlIntentStore as _, RootStore as _};
use lash_core::{
    Clock, ClockWallTime as _, SessionCatalogStore as _, SessionId, StoreError, StoreSet, TurnId,
};

use crate::clock::SimClock;

mod settlements;

/// The reconcile tick's nominal interval.
const TICK_MS: u64 = 10_000;
/// The longest a jittered tick waits: `T` + 10%.
const TICK_MAX_MS: u64 = 11_000;

/// The owner of the closed session's scope: every close attempt is one
/// delivery attempt reaching the engine half. It fails while `failures`
/// remain. It records when each of the session's roots had its scope
/// closed.
struct CountingClose {
    clock: Arc<SimClock>,
    failures: AtomicU32,
    attempts: Mutex<Vec<u64>>,
    root_closes: Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl ScopeCloseSink for CountingClose {
    async fn close_root_scope(&self, _: &RootTerminal) -> Result<(), StoreError> {
        self.root_closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.clock.logical_ms());
        Ok(())
    }

    async fn close_session_scope(
        &self,
        _: &SessionId,
        _: ControlIntentId,
        _: &[TurnId],
    ) -> Result<(), StoreError> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.clock.logical_ms());
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(StoreError::Backend("the scope owner is unreachable".into()));
        }
        Ok(())
    }
}

/// An engine that refuses every release for good.
struct RefusingEngine;

#[async_trait::async_trait]
impl SessionControlEngine for RefusingEngine {
    async fn reconcile_parks(
        &self,
        _: &dyn ParkRecoveryWriter,
        _: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        Ok(ParkReconcileReport::default())
    }

    async fn resume_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }

    async fn release_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Err(EngineRefusal::Permanent {
            code: lash_core::RuntimeErrorCode::PluginSessionManager,
            message: "the engine refuses the release for good".into(),
        })
    }
}

/// The session work of an engine that holds no drives, over `control`.
struct Work(Arc<dyn SessionControlEngine>);

#[async_trait::async_trait]
impl lash_core::SessionWorkEngine for Work {
    fn schedule_drive(&self, _: &SessionId, _: lash_core::engine::DriveRequestId) {}

    fn install_session_driver(
        &self,
        driver: Arc<dyn lash_core::SessionDriver>,
    ) -> Arc<dyn lash_core::SessionDriver> {
        driver
    }

    fn control(&self) -> Arc<dyn SessionControlEngine> {
        Arc::clone(&self.0)
    }
}

struct World {
    clock: Arc<SimClock>,
    stores: lash_sqlite_store::SqliteStoreSet,
    close: Arc<CountingClose>,
    relay: Arc<ControlIntentRelay>,
    /// The `ScopeClose` kind's relay the intent's engine half closes roots
    /// through.
    scope_close: Arc<ScopeCloseRelay>,
    session: SessionId,
    intent: ControlIntent,
    /// The virtual instant the close committed, its obligation due.
    armed_at: u64,
}

impl World {
    /// A session closed on a fresh store set: its `CloseSession` intent is
    /// recorded and its obligation armed, not yet delivered. `root` gives the
    /// session one open root for the close to release; the scope owner
    /// fails `failures` times.
    async fn closed(
        name: &str,
        root: bool,
        failures: u32,
        engine: Arc<dyn SessionControlEngine>,
    ) -> Self {
        Self::closed_on(name, root, failures, engine, SimClock::new()).await
    }

    /// [`closed`](Self::closed) on `clock`, which an engine the law built
    /// shares.
    async fn closed_on(
        name: &str,
        root: bool,
        failures: u32,
        engine: Arc<dyn SessionControlEngine>,
        clock: Arc<SimClock>,
    ) -> Self {
        clock.advance_by(1_000).await;
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("sim memory store set");
        let factory = stores.session_store_factory();
        let session = SessionId::from(format!("obligation-bounds-{name}"));
        factory
            .admit_session(&lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                config: lash_core::testing::mock_session_policy().into(),
                head: lash_core::SessionCreationHead::CommittedByCreator,
                owning_process_id: None,
            })
            .await
            .expect("session store");
        if root {
            factory
                .bind_root_inputs(&session, &TurnId::from("open-root"), &[])
                .await
                .expect("an open root");
        }
        let armed_at = clock.logical_ms();
        let intent = factory
            .begin_session_close(&session, clock.timestamp_ms())
            .await
            .expect("close")
            .expect("the session exists");
        let close = Arc::new(CountingClose {
            clock: Arc::clone(&clock),
            failures: AtomicU32::new(failures),
            attempts: Mutex::new(Vec::new()),
            root_closes: Mutex::new(Vec::new()),
        });
        let scope_close = Arc::new(ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&factory) as Arc<dyn lash_core::DeploymentStore>,
            Arc::clone(&close) as Arc<dyn ScopeCloseSink>,
        ));
        let relay = Arc::new(ControlIntentRelay::new(
            stores.obligation_ledger(ObligationKind::ControlIntent),
            factory,
            Arc::new(Work(engine)),
            Arc::clone(&close) as Arc<dyn ScopeCloseSink>,
            Arc::clone(&scope_close) as Arc<dyn ObligationRelay>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        ));
        Self {
            clock,
            stores,
            close,
            relay,
            scope_close,
            session,
            intent,
            armed_at,
        }
    }

    fn root_closes(&self) -> Vec<u64> {
        self.close
            .root_closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Where the open root's scope-close obligation stands.
    async fn root_scope_close(&self) -> Option<ObligationState> {
        self.stores
            .obligation_ledger(ObligationKind::ScopeClose)
            .state(&lash_core::store::scope_close_obligation_id(
                &self.session,
                &TurnId::from("open-root"),
            ))
            .await
            .expect("obligation state")
    }

    fn attempts(&self) -> Vec<u64> {
        self.close
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn obligation(&self) -> Option<ObligationState> {
        self.stores
            .obligation_ledger(ObligationKind::ControlIntent)
            .state(self.intent.obligation.as_ref().expect("armed by the close"))
            .await
            .expect("obligation state")
    }

    async fn intent_state(&self) -> ControlIntentState {
        self.stores
            .session_store_factory()
            .load_intent(self.intent.id)
            .await
            .expect("intent read")
            .expect("the close is kept")
            .state
    }

    /// Tick every `T` ±10% (drawn from `seed`), running the due pass each
    /// tick, until `done` answers true or `horizon_ms` of virtual time
    /// passed. Answers the virtual instants of the ticks.
    async fn tick_until(
        &self,
        seed: u64,
        horizon_ms: u64,
        mut done: impl AsyncFnMut(&Self) -> bool,
    ) -> Vec<u64> {
        let mut rng = fastrand::Rng::with_seed(seed);
        let stop = self.clock.logical_ms().saturating_add(horizon_ms);
        let mut ticks = Vec::new();
        while self.clock.logical_ms() < stop {
            self.clock
                .advance_by(rng.u64(TICK_MS - TICK_MS / 10..=TICK_MAX_MS))
                .await;
            ticks.push(self.clock.logical_ms());
            relay_due(
                self.relay.as_ref(),
                self.clock.as_ref(),
                NonZeroUsize::MIN.saturating_add(63),
            )
            .await
            .expect("due pass");
            if done(self).await {
                break;
            }
        }
        ticks
    }
}

/// Immediate: a producer's obligation is attempted before its call returns.
#[tokio::test]
async fn a_control_intents_obligation_is_attempted_before_its_verb_returns() {
    let world = World::closed(
        "immediate",
        false,
        0,
        Arc::new(lash_core::engine::NoEngineControl),
    )
    .await;
    assert_eq!(world.obligation().await, Some(ObligationState::Due));
    let state = world
        .relay
        .deliver_intent(&world.intent)
        .await
        .expect("deliver");
    assert!(matches!(state, ControlIntentState::Acknowledged { .. }));
    assert_eq!(world.attempts(), vec![world.armed_at]);
    assert_eq!(world.obligation().await, Some(ObligationState::Delivered));
}

/// Lost immediate attempt: an obligation no delivery attempted is claimed
/// by `due_at + T`.
#[tokio::test]
async fn a_lost_immediate_attempt_is_claimed_within_one_tick() {
    for seed in 0..8 {
        let world = World::closed(
            &format!("lost-{seed}"),
            false,
            0,
            Arc::new(lash_core::engine::NoEngineControl),
        )
        .await;
        world
            .tick_until(seed, 10 * TICK_MAX_MS, async |world| {
                !world.attempts().is_empty()
            })
            .await;
        let attempts = world.attempts();
        let [first] = attempts.as_slice() else {
            panic!("seed {seed}: one attempt delivers it: {attempts:?}");
        };
        assert!(
            *first <= world.armed_at + TICK_MAX_MS,
            "seed {seed}: claimed at {first}, due at {}",
            world.armed_at
        );
        assert_eq!(world.obligation().await, Some(ObligationState::Delivered));
    }
}

/// Lapsed claim: a delivery that died holding its claim is retaken by
/// `claimed_at + claim_ttl + T`, never before its claim lapsed.
#[tokio::test]
async fn a_lapsed_claim_is_retaken_within_its_ttl_and_one_tick() {
    let policy = RelayPolicy::default();
    for seed in 0..8 {
        let world = World::closed(
            &format!("lapsed-{seed}"),
            false,
            0,
            Arc::new(lash_core::engine::NoEngineControl),
        )
        .await;
        let claimed_at = world.clock.logical_ms();
        world
            .stores
            .obligation_ledger(ObligationKind::ControlIntent)
            .claim(
                world.intent.obligation.as_ref().expect("armed"),
                &lash_core::store::ClaimToken::mint(),
                world.clock.timestamp_ms(),
                policy.claim_ttl_ms,
            )
            .await
            .expect("claim")
            .expect("due");
        // The claimant dies here: nothing settles its claim.
        world
            .tick_until(
                seed,
                policy.claim_ttl_ms + 10 * TICK_MAX_MS,
                async |world| !world.attempts().is_empty(),
            )
            .await;
        let attempts = world.attempts();
        let [retaken] = attempts.as_slice() else {
            panic!("seed {seed}: one attempt retakes it: {attempts:?}");
        };
        assert!(
            *retaken >= claimed_at + policy.claim_ttl_ms,
            "seed {seed}: retaken at {retaken} before the claim of {claimed_at} lapsed"
        );
        assert!(
            *retaken <= claimed_at + policy.claim_ttl_ms + TICK_MAX_MS,
            "seed {seed}: retaken at {retaken}, claimed at {claimed_at}"
        );
        assert_eq!(world.obligation().await, Some(ObligationState::Delivered));
    }
}

/// Retryable failure: attempt `n + 1` comes `min(2^(n−1) s, 15 min)` after
/// attempt `n`, plus at most `T`, and the obligation stalls after the
/// attempt ceiling's attempts — about 1 h 47 min at the defaults — never
/// later. At the ceiling the intent closes `Failed { retryable: false }`.
#[tokio::test]
async fn a_failing_intent_retries_on_its_backoff_and_stalls_at_its_ceiling_never_later() {
    let policy = RelayPolicy::default();
    let ceiling = policy.attempt_ceiling.get();
    for seed in 0..4 {
        let world = World::closed(
            &format!("ceiling-{seed}"),
            false,
            u32::MAX,
            Arc::new(lash_core::engine::NoEngineControl),
        )
        .await;
        // Attempt 1: the verb's own.
        assert!(matches!(
            world
                .relay
                .deliver_intent(&world.intent)
                .await
                .expect("deliver"),
            ControlIntentState::Failed {
                retryable: true,
                ..
            }
        ));
        let bound: u64 = (1..ceiling)
            .map(|attempt| policy.backoff_ms(attempt) + TICK_MAX_MS)
            .sum();
        world
            .tick_until(seed, bound + 10 * TICK_MAX_MS, async |world| {
                world.obligation().await == Some(ObligationState::Stalled)
            })
            .await;
        let attempts = world.attempts();
        assert_eq!(
            attempts.len(),
            usize::try_from(ceiling).expect("ceiling"),
            "seed {seed}: the ceiling's attempts, then a stall: {attempts:?}"
        );
        for (index, pair) in attempts.windows(2).enumerate() {
            let attempt = u32::try_from(index + 1).expect("attempt");
            let gap = pair[1] - pair[0];
            let backoff = policy.backoff_ms(attempt);
            assert!(
                gap >= backoff && gap <= backoff + TICK_MAX_MS,
                "seed {seed}: attempt {} came {gap} ms after attempt {attempt}, backoff {backoff}",
                attempt + 1
            );
        }
        let last = *attempts.last().expect("attempts");
        assert!(
            last - attempts[0] <= bound,
            "seed {seed}: stalled {} ms after the first attempt, bound {bound}",
            last - attempts[0]
        );
        assert!(bound <= 107 * 60 * 1_000 + 15 * TICK_MAX_MS);
        let stalled = world
            .stores
            .obligation_ledger(ObligationKind::ControlIntent)
            .list_stalled(None, NonZeroUsize::MIN)
            .await
            .expect("stalled");
        assert_eq!(stalled[0].reason, StallReason::AttemptsExhausted);
        assert_eq!(stalled[0].attempts, ceiling);
        assert!(matches!(
            world.intent_state().await,
            ControlIntentState::Failed {
                retryable: false,
                ..
            }
        ));
        // Never retried again once stalled.
        world
            .tick_until(seed, policy.max_backoff_ms + TICK_MAX_MS, async |_| false)
            .await;
        assert_eq!(world.attempts().len(), attempts.len());
    }
}

/// Refused: an obligation the engine refuses for good stalls in the pass
/// that claims it.
#[tokio::test]
async fn a_refused_intent_stalls_in_the_pass_that_claims_it() {
    let world = World::closed("refused", true, 0, Arc::new(RefusingEngine)).await;
    let ticks = world
        .tick_until(7, 10 * TICK_MAX_MS, async |world| {
            world.obligation().await != Some(ObligationState::Due)
        })
        .await;
    assert_eq!(ticks.len(), 1, "the first pass claims and stalls it");
    assert_eq!(world.obligation().await, Some(ObligationState::Stalled));
    let stalled = world
        .stores
        .obligation_ledger(ObligationKind::ControlIntent)
        .list_stalled(None, NonZeroUsize::MIN)
        .await
        .expect("stalled");
    assert_eq!(stalled[0].reason, StallReason::Refused);
    assert_eq!(stalled[0].attempts, 1);
    assert!(
        world.attempts().is_empty(),
        "a refused release closes nothing"
    );
}

/// An engine whose every root release waits `release_ms` on the virtual
/// clock — a slow control RPC — recording when each release started and
/// when each park reconcile ran.
struct SlowReleases {
    clock: Arc<SimClock>,
    release_ms: u64,
    releases: Mutex<Vec<u64>>,
    parks: Mutex<Vec<u64>>,
}

struct HeldRecovery;

#[async_trait::async_trait]
impl SessionControlEngine for HeldRecovery {
    async fn reconcile_parks(
        &self,
        _: &dyn ParkRecoveryWriter,
        _: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        std::future::pending().await
    }
    async fn resume_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }
    async fn release_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::Released)
    }
}

/// A stuck leader repair must return within the arm budget, so the fixed
/// interval keeps reaching due retries after the first delivery failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_recovery_preserves_the_interval_and_due_retry_bound() {
    use lash_core::engine::{ReconcileArm, ReconcileCursor, RecoveryPassBudget};
    use lash_core::runtime::drive::{
        RECOVERY_TICK, ReconcileParts, RecoveryInterval, reconcile_once,
    };
    let clock = SimClock::new();
    let work = Arc::new(Work(Arc::new(HeldRecovery)));
    let world = World::closed_on(
        "held-recovery",
        true,
        1,
        Arc::new(HeldRecovery),
        clock.clone(),
    )
    .await;
    let ticks = Arc::new(Mutex::new(Vec::new()));
    let schedule = {
        let clock = clock.clone();
        let sessions = world.stores.session_store_factory();
        let scopes = world.close.clone();
        let relays: Vec<Arc<dyn ObligationRelay>> =
            vec![world.relay.clone(), world.scope_close.clone()];
        let ticks = ticks.clone();
        tokio::spawn(async move {
            let lanes = lash_conformance::deployment_tick_lanes(
                clock.clone(),
                RecoveryPassBudget::default(),
            );
            let mut interval = RecoveryInterval::new(clock.clone(), RECOVERY_TICK);
            let mut cursor = ReconcileCursor::default();
            loop {
                interval.tick().await;
                let at = clock.logical_ms();
                let tick = reconcile_once(
                    &ReconcileParts {
                        sessions: sessions.as_ref(),
                        work: work.as_ref(),
                        scopes: scopes.as_ref(),
                        processes: None,
                        clock: clock.as_ref(),
                        duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                        relays: &relays,
                        lanes: &lanes,
                    },
                    &cursor,
                    NonZeroUsize::MIN.saturating_add(63),
                )
                .await;
                cursor = tick.next.clone();
                ticks.lock().expect("ticks").push((at, tick));
            }
        })
    };
    let start = clock.logical_ms();
    while clock.logical_ms() < start + 25_000 {
        clock.advance_by(100).await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    schedule.abort();
    let _ = schedule.await;
    let obligation = world.obligation().await;
    let ticks = ticks.lock().expect("ticks");
    assert!(
        ticks.len() >= 3,
        "held recovery stopped the interval: {ticks:?}"
    );
    for (index, (at, tick)) in ticks.iter().enumerate() {
        assert!(
            *at <= start + index as u64 * 10_000 + 1_000,
            "tick missed its grid: {ticks:?}"
        );
        assert!(
            tick.failures
                .iter()
                .any(|failure| failure.arm == ReconcileArm::Parks),
            "arm timeout is reported"
        );
    }
    assert_eq!(
        obligation,
        Some(ObligationState::Delivered),
        "the later tick retries due work"
    );
}

#[async_trait::async_trait]
impl SessionControlEngine for SlowReleases {
    async fn reconcile_parks(
        &self,
        _: &dyn ParkRecoveryWriter,
        _: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        self.parks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.clock.logical_ms());
        Ok(ParkReconcileReport::default())
    }

    async fn resume_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }

    async fn release_root(
        &self,
        _: &RootRef,
        _: Option<&lash_core::store::EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        self.releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.clock.logical_ms());
        lash_core::Clock::sleep(
            self.clock.as_ref(),
            std::time::Duration::from_millis(self.release_ms),
        )
        .await;
        Ok(EngineAck::Released)
    }
}

/// ADR 0109 §1.8: a slow delivery starves neither a later kind nor the
/// parks. The deployment's production schedule — the recovery interval,
/// the tick and its per-kind lanes — runs on the virtual clock while the
/// closed session's release, a control RPC, takes ten minutes. Every tick
/// fires on its `T` grid; each tick's parks run within its lane wait; the
/// closed root's scope close, a later kind, is delivered in the first tick
/// beside the slow intent; the intent's kind is reported busy, never
/// re-claimed, while its attempt runs; and the attempt is cut at its
/// budget, its retry due its backoff after it started, and retried by the
/// first tick its lane is free for.
#[tokio::test]
async fn slow_delivery_does_not_starve_later_kinds_or_parks() {
    use lash_core::engine::{ReconcileCursor, RecoveryPassBudget};
    use lash_core::runtime::drive::{
        RECOVERY_TICK, ReconcileParts, RecoveryInterval, reconcile_once,
    };
    use settlements::Progress;

    const FIRST_RELEASE_DELAY_MS: u64 = 100;
    let tick_ms = RECOVERY_TICK.as_millis() as u64;
    let budget = RecoveryPassBudget::default();
    let wait_ms = budget.tick_wait.as_millis() as u64;
    let policy = RelayPolicy::default();

    let clock = SimClock::new();
    let engine = Arc::new(SlowReleases {
        clock: Arc::clone(&clock),
        release_ms: 600_000,
        releases: Mutex::new(Vec::new()),
        parks: Mutex::new(Vec::new()),
    });
    let world = World::closed_on(
        "slow-release",
        true,
        0,
        Arc::clone(&engine) as Arc<dyn SessionControlEngine>,
        Arc::clone(&clock),
    )
    .await;
    assert_eq!(world.obligation().await, Some(ObligationState::Due));
    assert_eq!(
        world.root_scope_close().await,
        Some(ObligationState::Due),
        "the close armed its root's scope close"
    );

    let (control_relay, mut control_settled) =
        settlements::observe(world.relay.clone(), clock.clone(), FIRST_RELEASE_DELAY_MS);
    let (scope_relay, mut scope_settled) =
        settlements::observe(world.scope_close.clone(), clock.clone(), 0);
    let (ready, mut tick_ready) = tokio::sync::mpsc::unbounded_channel();
    let (proceed, mut tick_proceed) = tokio::sync::mpsc::unbounded_channel();
    let (finished, mut tick_finished) = tokio::sync::mpsc::unbounded_channel();

    // The real interval and lanes run independently. The driver orders a
    // timeout settlement before a tick due at the same instant.
    let schedule = {
        let clock = Arc::clone(&clock);
        let factory = world.stores.session_store_factory();
        let work = Work(Arc::clone(&engine) as Arc<dyn SessionControlEngine>);
        let close = Arc::clone(&world.close);
        let relays = vec![control_relay, scope_relay];
        tokio::spawn(async move {
            let lanes = lash_conformance::deployment_tick_lanes(
                Arc::clone(&clock) as Arc<dyn Clock>,
                budget,
            );
            let mut interval =
                RecoveryInterval::new(Arc::clone(&clock) as Arc<dyn Clock>, RECOVERY_TICK);
            let mut cursor = ReconcileCursor::default();
            loop {
                interval.tick().await;
                let ticked_at = clock.logical_ms();
                ready.send(ticked_at).expect("tick observer");
                tick_proceed.recv().await.expect("tick admission");
                let tick = reconcile_once(
                    &ReconcileParts {
                        sessions: factory.as_ref(),
                        work: &work,
                        scopes: close.as_ref(),
                        processes: None,
                        clock: clock.as_ref(),
                        duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                        relays: &relays,
                        lanes: &lanes,
                    },
                    &cursor,
                    NonZeroUsize::MIN.saturating_add(63),
                )
                .await;
                cursor = tick.next.clone();
                finished.send((ticked_at, tick)).expect("tick observer");
            }
        })
    };
    let start = clock.logical_ms();
    let started_ms = clock.timestamp_ms();
    let cutoff = start + policy.attempt_budget_ms;
    let mut ticks = Vec::new();
    for index in 0..=5 {
        let tick_at = start + tick_ms * index;
        clock.advance_to(tick_at).await;
        assert_eq!(tick_ready.recv().await, Some(tick_at));
        if tick_at == cutoff {
            assert!(matches!(
                control_settled.recv().await,
                Some(Progress::Settled(lash_core::store::ObligationSettlement::Retry { due_at_ms, .. }))
                    if due_at_ms == started_ms + policy.base_backoff_ms
            ));
            assert_eq!(world.obligation().await, Some(ObligationState::Due));
        }
        proceed.send(()).expect("schedule is running");
        if index == 0 {
            // Model the awaited work between starting the attempt budget
            // and entering the engine RPC, without spending wall time.
            clock.wait_for_sleep(cutoff).await;
            clock.wait_for_sleep(start + FIRST_RELEASE_DELAY_MS).await;
            assert!(matches!(
                scope_settled.recv().await,
                Some(Progress::Settled(
                    lash_core::store::ObligationSettlement::Delivered
                ))
            ));
            clock.advance_to(start + FIRST_RELEASE_DELAY_MS).await;
            clock
                .wait_for_sleep(start + FIRST_RELEASE_DELAY_MS + engine.release_ms)
                .await;
        } else if tick_at == cutoff {
            clock
                .wait_for_sleep(cutoff + policy.attempt_budget_ms)
                .await;
            clock.wait_for_sleep(cutoff + engine.release_ms).await;
        }
        if index > 0 {
            assert!(matches!(
                scope_settled.recv().await,
                Some(Progress::EmptyPass)
            ));
        }
        clock.wait_for_sleep(tick_at + wait_ms).await;
        clock.advance_to(tick_at + wait_ms).await;
        ticks.push(tick_finished.recv().await.expect("completed tick"));
    }
    schedule.abort();
    assert!(schedule.await.expect_err("schedule aborted").is_cancelled());

    let instants: Vec<u64> = ticks.iter().map(|(at, _)| *at).collect();
    assert_eq!(
        instants.len(),
        6,
        "the interval ticked through the slow release"
    );
    // Cadence: every tick fires on the grid, whatever the release spends.
    for (index, at) in instants.iter().enumerate() {
        let due = start + tick_ms * index as u64;
        assert_eq!(*at, due, "tick {index}: {instants:?}");
    }
    // Parks: each tick's leader arm ran within its lane wait.
    let parks = engine
        .parks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        parks.len(),
        instants.len(),
        "{parks:?} for ticks {instants:?}"
    );
    for (at, park) in instants.iter().zip(&parks) {
        assert_eq!(*park, at + wait_ms, "the parks of the tick at {at}");
    }
    // A later kind: the root's scope close was delivered in the first tick.
    let closes = world.root_closes();
    let [closed] = closes.as_slice() else {
        panic!("one root scope close: {closes:?}");
    };
    assert_eq!(*closed, start, "the later kind ran beside the slow intent");
    assert_eq!(
        world.root_scope_close().await,
        Some(ObligationState::Delivered)
    );
    // The slow kind: busy, never re-claimed, while its attempt ran.
    for (at, tick) in &ticks {
        assert!(tick.failures.is_empty(), "tick at {at}: {tick:?}");
        let busy = if *at == start || *at == cutoff {
            vec![]
        } else {
            vec![ObligationKind::ControlIntent]
        };
        assert_eq!(tick.obligations_busy, busy, "the busy lanes at {at}");
    }
    // The attempt was cut at its budget and retried from its start.
    let releases = engine
        .releases
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let [first, second] = releases.as_slice() else {
        panic!("the cut attempt was retried: {releases:?}");
    };
    assert_eq!(*first, start + FIRST_RELEASE_DELAY_MS, "{releases:?}");
    // The budget starts before the relay's awaited preparation, not when
    // the engine RPC finally begins.
    assert_eq!(
        *second, cutoff,
        "the first free tick retries the cut attempt"
    );
    eprintln!(
        "attempt_start={start}, first_rpc={first}, cutoff={cutoff}, retry_rpc={second}, ticks={instants:?}"
    );
}
