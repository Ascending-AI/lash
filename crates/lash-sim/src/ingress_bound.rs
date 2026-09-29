//! ADR 0109 §1.8 for the ingress obligation (FIG-3851): the detection bounds
//! of an accepted input's drive, on the virtual clock over the SQLite store.
//!
//! A reconcile tick runs every `T` = 10 s ± 10 %; each law steps the clock by
//! the slowest tick, so every bound is asserted at its worst case.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lash_core::engine::{
    DriveRequestId, EngineRefusal, NoScopeClose, ReconcileCursor, ReconcileTick,
};
use lash_core::runtime::drive::relay::{ObligationRelay, RelayPolicy};
use lash_core::runtime::drive::{IngressRelay, ReconcileParts, reconcile_once};
use lash_core::runtime::recovery_lease::RecoveryDuties;
use lash_core::store::ingress_obligation::ingress_obligation_id;
use lash_core::store::{ObligationKind, ObligationLedger, ObligationState, StallReason};
use lash_core::{
    InputId, RuntimeStore, SessionCatalogStore as _, SessionDriver, SessionId, SessionStore,
    SessionWorkEngine, StoreSet as _,
};

use crate::clock::SimClock;

/// The virtual clock's wall reading, as the stores and the relay stamp it.
fn now_ms(clock: &SimClock) -> u64 {
    let clock: &dyn lash_core::Clock = clock;
    clock.timestamp_ms()
}

/// The slowest reconcile tick: `T` = 10 s plus its 10 % jitter.
const TICK_MS: u64 = 11_000;

/// The engine a relay asks for drives: it records every ask, and refuses the
/// asks its script names.
struct Engine {
    clock: Arc<SimClock>,
    asks: Mutex<Vec<(u64, String)>>,
    retry: Mutex<bool>,
    refuse: Mutex<BTreeSet<String>>,
}

#[async_trait::async_trait]
impl SessionWorkEngine for Engine {
    fn schedule_drive(&self, _: &SessionId, _: DriveRequestId) {
        panic!("ingress asks for its drive through request_drive");
    }

    async fn request_drive(
        &self,
        _: &SessionId,
        request: DriveRequestId,
    ) -> Result<(), EngineRefusal> {
        self.asks
            .lock()
            .expect("asks")
            .push((now_ms(&self.clock), request.as_str().to_owned()));
        if self
            .refuse
            .lock()
            .expect("refusals")
            .contains(request.as_str())
        {
            return Err(EngineRefusal::Permanent {
                code: lash_core::RuntimeErrorCode::SessionWorkUnavailable,
                message: "the engine refuses this drive".to_owned(),
            });
        }
        if *self.retry.lock().expect("retry") {
            return Err(EngineRefusal::Retryable(
                "the engine is unavailable".to_owned(),
            ));
        }
        Ok(())
    }

    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        driver
    }
}

struct World {
    clock: Arc<SimClock>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    engine: Arc<Engine>,
    relay: IngressRelay,
    ledger: Arc<dyn ObligationLedger>,
    ops: lash_core::facade_support::DurableSessionOps,
    store: Arc<dyn RuntimeStore>,
    session: SessionId,
}

async fn world() -> World {
    let clock = SimClock::new();
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("sim memory store set"),
    );
    let session = SessionId::from("ingress-bound");
    let factory = stores.session_store_factory();
    factory
        .admit_session(&lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
            owning_process_id: None,
        })
        .await
        .expect("create the session");
    let store: Arc<dyn RuntimeStore> = factory;
    let engine = Arc::new(Engine {
        clock: clock.clone(),
        asks: Mutex::new(Vec::new()),
        retry: Mutex::new(false),
        refuse: Mutex::new(BTreeSet::new()),
    });
    let ledger = stores.obligation_ledger(ObligationKind::Ingress);
    let relay = IngressRelay::new(
        Arc::clone(&ledger),
        Arc::clone(&engine) as Arc<dyn SessionWorkEngine>,
        clock.clone(),
    );
    let ops = lash_core::facade_support::DurableSessionOps::new(
        session.clone(),
        relay.clone(),
        Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::default()),
    );
    World {
        clock,
        stores,
        engine,
        relay,
        ledger,
        ops,
        store,
        session,
    }
}

impl World {
    /// Accept `text` through the producer: its admission, then its
    /// immediate delivery.
    async fn accept(&self, text: &str) -> InputId {
        let view = SessionStore::new(Arc::clone(&self.store), self.session.clone())
            .expect("valid session id");
        self.ops
            .enqueue_turn_input(
                &view,
                lash_core::TurnInput::text(text),
                lash_core::TurnInputIngress::NextTurn,
                None,
                lash_core::RunSpec::default(),
            )
            .await
            .expect("accept the input")
            .input_id
    }

    /// Commit `text` through the store alone: the producer died between its
    /// commit and its immediate attempt.
    async fn commit_only(&self, text: &str) -> InputId {
        self.store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                self.session.clone(),
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text(text),
            ))
            .await
            .expect("commit the input")
            .input_id
    }

    /// One reconcile tick under `duties`.
    async fn tick(&self, duties: RecoveryDuties) -> ReconcileTick {
        let relays: [Arc<dyn ObligationRelay>; 1] = [Arc::new(self.relay.clone())];
        let factory = self.stores.session_store_factory();
        let tick = reconcile_once(
            &ReconcileParts {
                sessions: factory.as_ref(),
                work: self.engine.as_ref(),
                scopes: &NoScopeClose,
                processes: None,
                clock: self.clock.as_ref(),
                duties,
                relays: &relays,
            },
            &ReconcileCursor::default(),
            NonZeroUsize::new(64).expect("a page"),
        )
        .await;
        assert!(tick.failures.is_empty(), "{:?}", tick.failures);
        tick
    }

    /// A follower's duties on this store: SQLite claims due rows on the
    /// leader alone.
    fn follower(&self) -> RecoveryDuties {
        RecoveryDuties {
            leader: false,
            due_claims: !self.stores.recovery_leader().due_claims_need_leader(),
        }
    }

    fn asks(&self) -> Vec<(u64, String)> {
        self.engine.asks.lock().expect("asks").clone()
    }

    async fn state(&self, input: &InputId) -> Option<ObligationState> {
        self.ledger
            .state(&ingress_obligation_id(input.as_str()))
            .await
            .expect("obligation state")
    }
}

fn ask(input: &InputId) -> String {
    ask_at(input, lash_core::drive::FIRST_INGRESS_ATTEMPT)
}

/// The drive the `attempt`th claim of `input`'s obligation asks for.
fn ask_at(input: &InputId, attempt: u32) -> String {
    lash_core::drive::ingress_drive_request(input.as_str(), attempt)
        .as_str()
        .to_owned()
}

/// Immediate: an accepted input's drive is asked for before its producer's
/// call returns, at the instant of its admission. The engine accepting the
/// ask does not deliver the obligation: its claim holds for the drive's
/// admission, which settles it.
#[tokio::test]
async fn an_accepted_input_is_attempted_before_its_producer_returns() {
    let world = world().await;
    let admitted_at = now_ms(&world.clock);
    let input = world.accept("now").await;
    assert_eq!(world.asks(), vec![(admitted_at, ask(&input))]);
    assert_eq!(world.state(&input).await, Some(ObligationState::Claimed));
}

/// Lost immediate attempt: a row whose producer died after its commit is
/// claimed by `due_at + T`. On SQLite only the leader claims it; a follower's
/// tick asks for nothing.
#[tokio::test]
async fn a_lost_immediate_attempt_is_claimed_within_one_tick() {
    let world = world().await;
    let due_at = now_ms(&world.clock);
    let input = world.commit_only("lost").await;
    assert_eq!(world.state(&input).await, Some(ObligationState::Due));
    world.tick(world.follower()).await;
    assert!(world.asks().is_empty(), "a SQLite follower claims nothing");

    world.clock.advance_by(TICK_MS).await;
    world.tick(RecoveryDuties::ALL).await;
    let asks = world.asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let (asked_at, request) = &asks[0];
    assert_eq!(request, &ask(&input));
    assert!(
        *asked_at <= due_at + TICK_MS,
        "claimed {} ms after due",
        asked_at - due_at
    );
    assert_eq!(world.state(&input).await, Some(ObligationState::Claimed));
    world.clock.advance_by(TICK_MS).await;
    world.tick(RecoveryDuties::ALL).await;
    assert_eq!(
        world.asks().len(),
        1,
        "a row whose ask holds its claim is not asked again"
    );
}

/// Lapsed claim: the engine accepted the ask and lost the drive before it
/// admitted the row (an operator kill). Nothing settles the claim, so it is
/// retaken by `claimed_at + claim_ttl + T` and asked again under its next
/// attempt: the engine would answer the first attempt's request from the
/// invocation it lost.
#[tokio::test]
async fn an_ask_nothing_admitted_is_asked_again_under_its_next_attempt() {
    let world = world().await;
    let claimed_at = now_ms(&world.clock);
    let input = world.accept("lost drive").await;
    assert_eq!(world.asks(), vec![(claimed_at, ask(&input))]);
    let bound = claimed_at + RelayPolicy::default().claim_ttl_ms + TICK_MS;
    while world.asks().len() == 1 {
        assert!(
            now_ms(&world.clock) <= bound,
            "the lapsed claim is retaken by {bound}"
        );
        world.clock.advance_by(TICK_MS).await;
        world.tick(RecoveryDuties::ALL).await;
    }
    let asks = world.asks();
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert!(asks[1].0 <= bound, "asked again at {}", asks[1].0);
    assert_eq!(
        asks[1].1,
        ask_at(&input, 2),
        "the second ask is a new request"
    );
    assert_eq!(world.state(&input).await, Some(ObligationState::Claimed));
}

/// Retryable failure: attempt `n + 1` follows attempt `n` by its backoff,
/// plus at most one tick; the row stalls after the attempt ceiling, never
/// later, and is asked for no more.
#[tokio::test]
async fn a_retryable_refusal_backs_off_and_stalls_at_the_ceiling() {
    let world = world().await;
    *world.engine.retry.lock().expect("retry") = true;
    let policy = RelayPolicy::default();
    let admitted_at = now_ms(&world.clock);
    let input = world.accept("unavailable").await;
    let ceiling = policy.attempt_ceiling.get();
    let bound: u64 = (1..ceiling)
        .map(|attempt| policy.backoff_ms(attempt) + TICK_MS)
        .sum();
    while world.state(&input).await != Some(ObligationState::Stalled) {
        assert!(
            now_ms(&world.clock) <= admitted_at + bound,
            "the row stalls by {bound} ms after its admission"
        );
        world.clock.advance_by(TICK_MS).await;
        world.tick(RecoveryDuties::ALL).await;
    }
    let asks = world.asks();
    assert_eq!(
        asks.len(),
        usize::try_from(ceiling).expect("a small ceiling"),
        "one attempt per retry up to the ceiling"
    );
    assert_eq!(asks[0].0, admitted_at, "the first attempt was immediate");
    for (index, pair) in asks.windows(2).enumerate() {
        let attempt = u32::try_from(index + 1).expect("a small attempt");
        let gap = pair[1].0 - pair[0].0;
        let backoff = policy.backoff_ms(attempt);
        assert!(
            (backoff..=backoff + TICK_MS).contains(&gap),
            "attempt {} came {gap} ms after attempt {attempt}, backoff {backoff} ms",
            attempt + 1
        );
        assert_eq!(pair[1].1, ask_at(&input, attempt + 1));
    }
    let stalled = world
        .ops
        .stalled_ingress(input.as_str())
        .await
        .expect("stalled read")
        .expect("the input's delivery stalled");
    assert_eq!(stalled.reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled.attempts, ceiling);
    for _ in 0..3 {
        world.clock.advance_by(policy.max_backoff_ms).await;
        world.tick(RecoveryDuties::ALL).await;
    }
    assert_eq!(
        world.asks().len(),
        asks.len(),
        "a stalled row is asked for no more"
    );
}

/// Refused: a drive the engine refuses for good stalls in the attempt that
/// claimed it, and the rows behind it in the same page are still asked for.
#[tokio::test]
async fn a_refused_ingress_stalls_in_the_pass_that_claims_it() {
    let world = world().await;
    let refused = world.commit_only("refused").await;
    let behind = world.commit_only("behind").await;
    world
        .engine
        .refuse
        .lock()
        .expect("refusals")
        .insert(ask(&refused));
    world.clock.advance_by(TICK_MS).await;
    let tick = world.tick(RecoveryDuties::ALL).await;
    let (_, pass) = tick
        .obligations
        .iter()
        .find(|(kind, _)| *kind == ObligationKind::Ingress)
        .expect("the ingress relay ran");
    assert_eq!((pass.claimed, pass.requested, pass.stalled), (2, 1, 1));
    assert_eq!(world.state(&refused).await, Some(ObligationState::Stalled));
    assert_eq!(world.state(&behind).await, Some(ObligationState::Claimed));
    let stalled = world
        .ops
        .stalled_ingress(refused.as_str())
        .await
        .expect("stalled read")
        .expect("the refused input's delivery stalled");
    assert_eq!(stalled.reason, StallReason::Refused);
    assert_eq!(stalled.attempts, 1);
}
