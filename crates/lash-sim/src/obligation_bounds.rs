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

use lash_core::drive::ControlIntentRelay;
use lash_core::drive::relay::{RelayPolicy, relay_due};
use lash_core::engine::{
    EngineAck, EnginePage, EngineRefusal, ParkReconcileReport, ParkRecoveryWriter, RootRef,
    ScopeCloseSink, SessionControlEngine,
};
use lash_core::store::ControlIntentStore as _;
use lash_core::store::{
    ControlIntent, ControlIntentId, ControlIntentState, ObligationKind, ObligationState,
    RootTerminal, StallReason,
};
use lash_core::{
    Clock, ClockWallTime as _, DeploymentStore as _, SessionId, StoreError, StoreSet, TurnId,
};

use crate::clock::SimClock;

/// The reconcile tick's nominal interval.
const TICK_MS: u64 = 10_000;
/// The longest a jittered tick waits: `T` + 10%.
const TICK_MAX_MS: u64 = 11_000;

/// The owner of the closed session's scope: every close attempt is one
/// delivery attempt reaching the engine half. It fails while `failures`
/// remain.
struct CountingClose {
    clock: Arc<SimClock>,
    failures: AtomicU32,
    attempts: Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl ScopeCloseSink for CountingClose {
    async fn close_root_scope(&self, _: &RootTerminal) -> Result<(), StoreError> {
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
    relay: ControlIntentRelay,
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
        let clock = SimClock::new();
        clock.advance_by(1_000).await;
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("sim memory store set");
        let factory = stores.session_store_factory();
        let session = SessionId::from(format!("obligation-bounds-{name}"));
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                policy: lash_core::testing::mock_session_policy(),
                owning_process_id: None,
            })
            .await
            .expect("session store");
        if root {
            store
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
        });
        let scope_close = Arc::new(lash_core::drive::ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&factory) as Arc<dyn lash_core::DeploymentStore>,
            Arc::clone(&close) as Arc<dyn ScopeCloseSink>,
        ));
        let relay = ControlIntentRelay::new(
            stores.obligation_ledger(ObligationKind::ControlIntent),
            factory,
            Arc::new(Work(engine)),
            Arc::clone(&close) as Arc<dyn ScopeCloseSink>,
            scope_close,
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        Self {
            clock,
            stores,
            close,
            relay,
            intent,
            armed_at,
        }
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
                &self.relay,
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
