//! A root's scope close runs beside the session's next root (FIG-4035), on
//! the double through the core's real drive.
//!
//! A root's run hands its report over and returns once its terminal commit
//! is durable; the close its terminal transaction armed as the root's
//! `ScopeClose` obligation (ADR 0109 §3) runs on the root's `LashTurn`
//! `close` handler, a journal of its own. The session's drive admits its
//! next root without waiting for that close. The laws hold the first root's
//! close at its obligation's claim and prove the second root is admitted and
//! answered while the close is still owed, then that every close is
//! delivered exactly once, a crashed close included.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::StoreError;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, ObligationState, SettleOutcome, StalledObligation,
};
use lash_restate_test::{
    CrashPoint, CrashRule, RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE,
};

/// Holds the first scope close a root's close claims, at its claim, until
/// the law opens it; every other close passes. While it is closed the
/// relay finds no scope close due, so the held close is delivered only by
/// its own run.
#[derive(Default)]
struct CloseGate {
    armed: AtomicBool,
    held: Mutex<Option<ObligationId>>,
    reached: tokio::sync::Notify,
    open: tokio::sync::Notify,
    opened: AtomicBool,
    /// Claims each scope close was granted: a delivery takes exactly one.
    granted: Mutex<BTreeMap<ObligationId, usize>>,
    /// Whether the held claim is waiting at the gate now.
    waiting: AtomicUsize,
}

impl CloseGate {
    fn closed(&self) -> bool {
        self.armed.load(Ordering::SeqCst) && !self.opened.load(Ordering::SeqCst)
    }

    /// Whether `id`'s claim waits: the first claim while the gate is closed
    /// is the one it holds.
    fn holds(&self, id: &ObligationId) -> bool {
        if !self.closed() {
            return false;
        }
        let mut held = self.held.lock().expect("held close");
        held.get_or_insert_with(|| id.clone()) == id
    }

    fn held(&self) -> ObligationId {
        self.held
            .lock()
            .expect("held close")
            .clone()
            .expect("the gate holds a close")
    }

    fn release(&self) {
        self.opened.store(true, Ordering::SeqCst);
        self.open.notify_waiters();
    }

    fn grant(&self, claimed: &Option<ClaimedObligation>) {
        if let Some(claimed) = claimed {
            *self
                .granted
                .lock()
                .expect("granted claims")
                .entry(claimed.id.clone())
                .or_default() += 1;
        }
    }

    fn granted(&self) -> BTreeMap<ObligationId, usize> {
        self.granted.lock().expect("granted claims").clone()
    }
}

/// The `ScopeClose` ledger behind a [`CloseGate`].
struct GatedScopeCloseLedger {
    inner: Arc<dyn ObligationLedger>,
    gate: Arc<CloseGate>,
}

#[async_trait::async_trait]
impl ObligationLedger for GatedScopeCloseLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        if self.gate.closed() {
            return Ok(Vec::new());
        }
        let claimed = self.inner.claim_due(now_ms, claim_ttl_ms, limit).await?;
        for claim in &claimed {
            self.gate.grant(&Some(claim.clone()));
        }
        Ok(claimed)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        if self.gate.holds(id) {
            let open = self.gate.open.notified();
            self.gate.waiting.fetch_add(1, Ordering::SeqCst);
            self.gate.reached.notify_waiters();
            if !self.gate.opened.load(Ordering::SeqCst) {
                open.await;
            }
            self.gate.waiting.fetch_sub(1, Ordering::SeqCst);
        }
        let claimed = self.inner.claim(id, now_ms, claim_ttl_ms).await?;
        self.gate.grant(&claimed);
        Ok(claimed)
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.inner.standing(id).await
    }
}

struct World {
    backend: RestateTestBackend,
    core: lash::LashCore,
    gate: Arc<CloseGate>,
    ledger: Arc<dyn ObligationLedger>,
}

fn text(text: impl Into<String>) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.into(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..Default::default()
    }
}

async fn world(seed: u64) -> World {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let gate = Arc::new(CloseGate::default());
    let layered = {
        let gate = Arc::clone(&gate);
        lash_core::testing::runtime_helpers::LayeredBackend::over(backend.lash_backend())
            .map_obligation_ledgers(move |kind, inner| {
                if kind == ObligationKind::ScopeClose {
                    Arc::new(GatedScopeCloseLedger {
                        inner,
                        gate: Arc::clone(&gate),
                    })
                } else {
                    inner
                }
            })
            .into_backend()
    };
    let ledger = backend
        .lash_backend()
        .obligation_ledger(ObligationKind::ScopeClose);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("root-close")
        .complete(move |_request: LlmRequest| {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move { Ok::<_, LlmTransportError>(text(format!("answer {call}"))) }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(layered, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "root-close",
        ))
        .expect("build the lash core");
    World {
        backend,
        core,
        gate,
        ledger,
    }
}

impl World {
    /// Send `question` and wait for its answer.
    async fn ask(&self, session: &lash::LashSession, question: &str) {
        let handle = session
            .send(lash::TurnInput::text(question))
            .await
            .expect("accept the input");
        let outcome = tokio::time::timeout(Duration::from_secs(30), handle.outcome())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "`{question}` is answered while the first root's scope close is held: \
                     its admission does not wait on that close"
                )
            })
            .expect("the outcome");
        assert!(
            matches!(outcome.status, lash::TurnStatus::Answered),
            "{:?}",
            outcome.status
        );
    }

    async fn state(&self, id: &ObligationId) -> Option<ObligationState> {
        self.ledger.state(id).await.expect("read the scope close")
    }

    /// Wait until every scope close the roots claimed is delivered.
    async fn closes_delivered(&self, roots: usize) -> BTreeMap<ObligationId, usize> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let granted = self.gate.granted();
                let mut delivered = 0;
                for id in granted.keys() {
                    if self.state(id).await == Some(ObligationState::Delivered) {
                        delivered += 1;
                    }
                }
                if granted.len() == roots && delivered == roots {
                    return granted;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "every root's scope close is delivered: {:?}",
                self.gate.granted()
            )
        })
    }

    /// The `LashTurn` key whose `close` the gate holds.
    fn held_close_key(&self) -> String {
        let suffix = "/close";
        let prefix = format!("{TURN_DRIVER_SERVICE}/");
        let running: Vec<String> = self
            .backend
            .server()
            .invocations()
            .into_iter()
            .filter(|invocation| {
                invocation.status != "completed"
                    && invocation.target.starts_with(&prefix)
                    && invocation.target.ends_with(suffix)
            })
            .map(|invocation| {
                invocation.target[prefix.len()..invocation.target.len() - suffix.len()].to_owned()
            })
            .collect();
        assert_eq!(
            running.len(),
            1,
            "one close runs, the held one: {running:?}"
        );
        running.into_iter().next().expect("the held close")
    }
}

/// Wait until the first root's close is held at its claim.
async fn held(gate: &CloseGate) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while gate.waiting.load(Ordering::SeqCst) == 0 {
            let reached = gate.reached.notified();
            if gate.waiting.load(Ordering::SeqCst) > 0 {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(20), reached).await;
        }
    })
    .await
    .expect("the first root's scope close reaches its claim");
}

/// FIG-4035: back-to-back sends. The second input's root is admitted, runs
/// and is answered while the first root's scope close is held at its claim,
/// so its admission is not ordered after that close; the close is then
/// delivered, once, beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_root_is_admitted_while_the_previous_close_is_owed() {
    let world = world(0x4035).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world
        .core
        .session("close-beside")
        .open()
        .await
        .expect("open");

    world.ask(&session, "first question").await;
    held(&world.gate).await;
    let first = world.gate.held();

    world.ask(&session, "second question").await;
    assert_ne!(
        world.state(&first).await,
        Some(ObligationState::Delivered),
        "the first root's close is still owed when the second root is answered"
    );
    assert_eq!(
        world.gate.waiting.load(Ordering::SeqCst),
        1,
        "the first root's close still waits at its claim"
    );

    world.gate.release();
    let granted = world.closes_delivered(2).await;
    assert!(granted.contains_key(&first));
    assert!(
        granted.values().all(|claims| *claims == 1),
        "each root's close was delivered by exactly one claim: {granted:?}"
    );
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending")
            .is_empty(),
        "both inputs were driven"
    );
}

/// FIG-4035: the first root's close crashes after the second root was
/// admitted, with the close still owed in its journal: its retry replays
/// the close, which finds its obligation delivered and delivers nothing
/// again. Both roots' scopes end closed, each by exactly one delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_crashed_after_the_next_root_was_admitted_converges_once() {
    let world = world(0x4036).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world
        .core
        .session("close-crash")
        .open()
        .await
        .expect("open");
    let crashes = Arc::new(AtomicUsize::new(0));
    {
        let crashes = Arc::clone(&crashes);
        world.backend.server().on_crash(Arc::new(move |_: &str| {
            crashes.fetch_add(1, Ordering::SeqCst);
        }));
    }

    world.ask(&session, "first question").await;
    held(&world.gate).await;
    let first = world.gate.held();
    let key = world.held_close_key();
    world.ask(&session, "second question").await;

    // The held close's result never reaches its journal: the crash lands
    // after its delivery settled the obligation, before the run's result is
    // stored, so the retry runs the close body again.
    world.backend.server().crash_on(
        CrashRule::new(CrashPoint::BeforeRunResult { name: None })
            .service(TURN_DRIVER_SERVICE)
            .handler("close")
            .key(key),
    );
    world.gate.release();

    let granted = world.closes_delivered(2).await;
    assert_eq!(
        crashes.load(Ordering::SeqCst),
        1,
        "the held close crashed once"
    );
    assert!(granted.contains_key(&first));
    assert!(
        granted.values().all(|claims| *claims == 1),
        "each root's close was delivered by exactly one claim, the crashed one included: \
         {granted:?}"
    );
    let replayed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let closes: Vec<_> = world
                .backend
                .server()
                .invocations()
                .into_iter()
                .filter(|invocation| invocation.target.ends_with("/close"))
                .collect();
            if closes.len() == 2 && closes.iter().all(|close| close.status == "completed") {
                return closes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both close invocations complete");
    assert!(
        replayed.iter().any(|close| close.attempts >= 2),
        "the crashed close ran again: {replayed:?}"
    );
    assert_eq!(world.state(&first).await, Some(ObligationState::Delivered));
}
