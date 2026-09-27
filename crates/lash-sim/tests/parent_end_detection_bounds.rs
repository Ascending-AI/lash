//! ADR 0109 §1.8 detection bounds for the `ParentEnd` obligation (FIG-3853).
//!
//! The ADR's bounds are wall-clock: a lost immediate attempt is claimed by
//! `due_at + T`, a lapsed claim retaken by `claimed_at + claim_ttl + T`, a
//! retry follows its capped backoff plus at most one tick, and an
//! undecodable or refused row stalls in the pass that claims it. The sim
//! owns the clock, so each bound is asserted at tick granularity over the
//! real store and the real relay: the claim the ADR says a tick takes is
//! the claim this suite takes, never earlier and never a tick later.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-expect-in-tests only exempts #[test] functions, and the probe helpers around them in this target are test code too"
)]

use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::{Arc, Mutex};

use lash_core::runtime::drive::ParentEndRelay;
use lash_core::runtime::drive::relay::{RelayPolicy, relay_due};
use lash_core::store::{
    ObligationId, ObligationKey, ObligationKind, ObligationLedger, ObligationSettlement,
    ObligationState, SettleOutcome, StallReason,
};
use lash_core::testing::TestClock;
use lash_core::{
    Ancestry, CancelRequest, ClockWallTime, Lifetime, LifetimeDecision, NativeProcessWork,
    PluginError, ProcessId, ProcessInput, ProcessProvenance, ProcessRegistration, ProcessRegistry,
    ProcessTerminalWait, ProcessWorkSubstrate, ScopeGrant, ScopeId, SessionScope,
};

/// A `ProcessWorkSubstrate` whose `deliver_cancel` pops a scripted error
/// before succeeding: the retryable and ceiling bounds need a delivery the
/// engine cannot make yet, on a port that otherwise behaves like the
/// native one (whose delivery is the registry write the apply performs).
struct ScriptedPort {
    inner: NativeProcessWork,
    errors: Mutex<VecDeque<PluginError>>,
}

impl ScriptedPort {
    fn new(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            inner: NativeProcessWork::for_registry(registry),
            errors: Mutex::new(VecDeque::new()),
        }
    }

    /// Fail the next `count` deliveries with `error`.
    fn fail(&self, count: usize, error: PluginError) {
        self.errors
            .lock()
            .expect("scripted errors")
            .extend(std::iter::repeat_n(error, count));
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for ScriptedPort {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> Result<lash_core::facade_support::ProcessAdmissionReport, PluginError> {
        Ok(lash_core::facade_support::ProcessAdmissionReport::default())
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessTerminalWait, PluginError> {
        self.inner.await_process_terminal(process_id).await
    }

    async fn deliver_cancel(
        &self,
        process_id: &ProcessId,
        request: &CancelRequest,
        key: &str,
    ) -> Result<(), PluginError> {
        if let Some(error) = self.errors.lock().expect("scripted errors").pop_front() {
            return Err(error);
        }
        self.inner.deliver_cancel(process_id, request, key).await
    }

    async fn publish_process_terminal(
        &self,
        process_id: &ProcessId,
        output: &lash_core::ProcessAwaitOutput,
        key: &str,
    ) -> Result<(), PluginError> {
        self.inner
            .publish_process_terminal(process_id, output, key)
            .await
    }
}

/// The world a bound law runs in: one memory backend on a clock the law
/// advances, its registry, its `ParentEnd` ledger and the scripted port
/// deliveries route through.
struct World {
    clock: Arc<TestClock>,
    registry: Arc<dyn ProcessRegistry>,
    ledger: Arc<dyn ObligationLedger>,
    port: Arc<ScriptedPort>,
}

impl World {
    async fn new() -> Self {
        let clock = Arc::new(TestClock::new(1_000_000));
        Self::over(
            lash_sqlite_store::SqliteBackend::memory_with_clock(clock.clone())
                .await
                .expect("a memory backend opens"),
            clock,
        )
        .await
    }

    async fn over(backend: lash_sqlite_store::SqliteBackend, clock: Arc<TestClock>) -> Self {
        let backend = lash_core::Backend::from(backend);
        let registry = backend.process_registry();
        Self {
            clock,
            port: Arc::new(ScriptedPort::new(Arc::clone(&registry))),
            registry,
            ledger: backend.obligation_ledger(ObligationKind::ParentEnd),
        }
    }

    /// The relay the reconcile tick runs (ADR 0109 §1.4): the generic loop
    /// over this ledger, this registry and this process port.
    fn relay(&self) -> ParentEndRelay {
        ParentEndRelay::new(
            Arc::clone(&self.ledger),
            Arc::clone(&self.registry),
            Arc::clone(&self.port) as Arc<dyn ProcessWorkSubstrate>,
            Arc::clone(&self.clock) as Arc<dyn lash_core::Clock>,
        )
    }

    /// One tick of the due-obligation arm, bounded like the reconcile pass.
    async fn tick(&self) -> lash_core::engine::RelayPass {
        relay_due(
            &self.relay(),
            self.clock.as_ref(),
            NonZeroUsize::new(256).expect("the page bound is non-zero"),
        )
        .await
        .expect("the due-obligation tick runs")
    }

    /// Register a `Detached` process and return its scope: the parent a law
    /// ends, and the scope its `Until` children may name while it is open.
    async fn parent_scope(&self, session: &str, label: &str) -> ScopeId {
        let parent = self
            .registry
            .register_process(ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                ProcessProvenance::session(SessionScope::new(session)),
                Lifetime::Detached,
            ))
            .await
            .unwrap_or_else(|error| panic!("register parent {label}: {error}"));
        ScopeId::process(parent.id)
    }

    /// End `scope`'s parent and return the obligation the record armed in
    /// the same transaction — the row a lost immediate attempt leaves.
    async fn record_end(&self, scope: &ScopeId) -> ObligationId {
        self.registry
            .record_parent_end(scope)
            .await
            .expect("record the parent's end");
        self.registry
            .get_parent_end_plan(scope)
            .await
            .expect("read the plan's ledger row")
            .expect("the record wrote the row")
            .obligation_id
            .expect("the record arms the row's obligation due immediately")
    }

    /// Register a live `Until` child of `scope` — the child the plan owes a
    /// `ParentEnded` cancel.
    async fn until_child(&self, session: &str, scope: &ScopeId) -> ProcessId {
        let mut registration = ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            ProcessProvenance::session(SessionScope::new(session)),
            Lifetime::Detached,
        );
        registration.ancestry = Ancestry::from_scopes([scope.clone()]);
        registration.lifetime = LifetimeDecision::Until {
            scope: scope.clone(),
            grant: ScopeGrant::Ancestor,
        };
        self.registry
            .register_process(registration)
            .await
            .expect("register the until child")
            .id
    }

    /// Whether `child`'s registry row carries a cancel request — the
    /// observable effect a delivered parent-end cancel has on the native
    /// engine's own evidence.
    async fn cancel_requested(&self, child: &ProcessId) -> bool {
        self.registry
            .get_process(child)
            .await
            .expect("read the child")
            .expect("the child row exists")
            .cancel_request
            .is_some()
    }

    /// The ledger's stalled listing.
    async fn stalled(&self) -> Vec<lash_core::store::StalledObligation> {
        self.ledger
            .list_stalled(None, NonZeroUsize::new(16).expect("non-zero"))
            .await
            .expect("list stalled obligations")
    }
}

/// §1.8: a lost immediate attempt is claimed by `due_at + T`. The record
/// armed the row due now and the live path never applied it — the same
/// durable shape a dead execution leaves. One tick over the due index both
/// claims and delivers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_plan_is_claimed_and_delivered_within_one_tick() {
    let world = World::new().await;
    let scope = world.parent_scope("bounds-session", "due-parent").await;
    let child = world.until_child("bounds-session", &scope).await;
    let obligation = world.record_end(&scope).await;

    // Nothing due before the record; one tick afterward delivers it: that
    // tick is the `due_at + T` bound at sim granularity.
    let pass = world.tick().await;
    assert_eq!(pass.claimed, 1, "the tick claims the armed row");
    assert_eq!(pass.delivered, 1, "the same tick delivers it");
    assert!(
        world.cancel_requested(&child).await,
        "the plan's delivery is the child's recorded cancel request"
    );
    assert_eq!(
        world
            .ledger
            .state(&obligation)
            .await
            .expect("read the obligation's state"),
        Some(ObligationState::Delivered),
        "the obligation settles delivered"
    );
    assert!(
        world
            .registry
            .get_parent_end_plan(&scope)
            .await
            .expect("read the settled plan")
            .expect("the row is kept")
            .settled_at_ms
            .is_some(),
        "the delivery applied and settled the plan"
    );
    assert!(world.stalled().await.is_empty());
}

/// §1.8: a lapsed claim is retaken by `claimed_at + claim_ttl + T` — and a
/// settle the lapsed claim writes is refused by the claim fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lapsed_claim_is_retaken_on_the_next_tick() {
    let world = World::new().await;
    let ttl_ms = 60_000;
    let scope = world.parent_scope("bounds-session", "lapsed-parent").await;
    let obligation = world.record_end(&scope).await;

    // A relay claims the row and dies holding it: its token is fenced by
    // its expiry, which is the retake bound.
    let claimed_at = world.clock.timestamp_ms();
    let stale = world
        .ledger
        .claim(&obligation, claimed_at, ttl_ms)
        .await
        .expect("claim the due obligation")
        .expect("a due obligation claims");

    world.clock.set(claimed_at + ttl_ms - 1);
    let pass = world.tick().await;
    assert_eq!(pass.claimed, 0, "a live claim is not retaken early");

    world.clock.set(claimed_at + ttl_ms);
    let pass = world.tick().await;
    assert_eq!(
        (pass.claimed, pass.delivered),
        (1, 1),
        "the first tick at the claim's expiry retakes and delivers it"
    );

    // The lapsed claim's token settles nothing: the retake fenced it.
    let outcome = world
        .ledger
        .settle(
            &obligation,
            &stale.token,
            ObligationSettlement::Delivered,
            world.clock.timestamp_ms(),
        )
        .await
        .expect("the stale settle answers, not errors");
    assert_eq!(outcome, SettleOutcome::ClaimLost);
}

/// §1.8: retry `n + 1` runs at the capped backoff plus at most `T` — never
/// before the backoff elapses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retryable_delivery_is_reattempted_at_its_backoff() {
    let world = World::new().await;
    let base_ms = 10_000;
    let scope = world.parent_scope("bounds-session", "retry-parent").await;
    let _child = world.until_child("bounds-session", &scope).await;
    let _obligation = world.record_end(&scope).await;
    world.port.fail(
        1,
        PluginError::Session("the engine is unreachable".to_string()),
    );

    let now = world.clock.timestamp_ms();
    let pass = relay_due(
        &world.relay().with_policy(RelayPolicy {
            base_backoff_ms: base_ms,
            max_backoff_ms: 900_000,
            attempt_ceiling: NonZeroU32::new(16).unwrap_or(NonZeroU32::MIN),
            claim_ttl_ms: 60_000,
        }),
        world.clock.as_ref(),
        NonZeroUsize::MIN,
    )
    .await
    .expect("the retryable pass runs");
    assert_eq!(
        (pass.claimed, pass.retried),
        (1, 1),
        "a retryable failure hands the row back"
    );

    world.clock.set(now + base_ms - 1);
    assert!(
        world
            .ledger
            .claim_due(world.clock.timestamp_ms(), 60_000, NonZeroUsize::MIN)
            .await
            .expect("the early due read")
            .is_empty(),
        "the backoff holds the row until its due instant"
    );

    world.clock.set(now + base_ms);
    let pass = world.tick().await;
    assert_eq!(
        (pass.claimed, pass.delivered),
        (1, 1),
        "the first tick at the backoff's end re-attempts and delivers"
    );
}

/// §1.8: a retryable failure stalls `attempts_exhausted` at the kind's
/// attempt ceiling, never later and never sooner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_failures_stall_at_the_attempt_ceiling() {
    let world = World::new().await;
    let ceiling = 2_u32;
    let scope = world.parent_scope("bounds-session", "ceiling-parent").await;
    let _child = world.until_child("bounds-session", &scope).await;
    let obligation = world.record_end(&scope).await;
    world.port.fail(
        ceiling as usize,
        PluginError::Session("the engine stays unreachable".to_string()),
    );

    let relay = world.relay().with_policy(RelayPolicy {
        base_backoff_ms: 10_000,
        max_backoff_ms: 900_000,
        attempt_ceiling: NonZeroU32::new(ceiling).unwrap_or(NonZeroU32::MIN),
        claim_ttl_ms: 60_000,
    });
    async fn tick_at(relay: &ParentEndRelay, world: &World) -> lash_core::engine::RelayPass {
        relay_due(relay, world.clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("the pass runs")
    }

    let pass = tick_at(&relay, &world).await;
    assert_eq!(pass.retried, 1, "attempt 1 backs off");
    world.clock.advance(10_000);
    let pass = tick_at(&relay, &world).await;
    assert_eq!(
        (pass.claimed, pass.stalled),
        (1, 1),
        "the ceiling attempt stalls"
    );

    let stalled = world.stalled().await;
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].id, obligation);
    assert_eq!(stalled[0].reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled[0].attempts, ceiling);

    world.clock.advance(10_000);
    let pass = tick_at(&relay, &world).await;
    assert_eq!(pass.claimed, 0, "a stalled obligation is never retried");
}

/// §1.8: an undecodable row stalls `undecodable` in the pass that claims
/// it, and later rows of the same page still deliver — the prospect's
/// decode-aborts-the-arm failure (PG parent_end.rs ~135-150) gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undecodable_row_stalls_alone_and_its_page_delivers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(TestClock::new(1_000_000));
    let backend = lash_sqlite_store::SqliteBackend::open_with_options_and_clock(
        dir.path().join("store"),
        lash_sqlite_store::SqliteBackendOptions::default(),
        clock.clone(),
    )
    .await
    .expect("a file backend opens");
    let world = World::over(backend, clock).await;

    // A row no write path produces — injected with a payload this build
    // cannot decode — armed like any record's row.
    let corrupt_key = ObligationKey::ParentEnd {
        parent_kind: "turn".to_string(),
        parent_id: "bounds-session/corrupt-turn".to_string(),
    };
    let registry_db = dir.path().join("store/process-registry.db");
    let conn =
        rusqlite::Connection::open(&registry_db).expect("open the process registry database");
    conn.execute(
        "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES ('turn', 'bounds-session/corrupt-turn', 'not-a-scope-payload', 0)",
        [],
    )
    .expect("inject the corrupt ledger row");
    drop(conn);
    let corrupt_obligation = world
        .ledger
        .arm(&corrupt_key, world.clock.timestamp_ms())
        .await
        .expect("arm runs")
        .expect("the injected row arms");

    let healthy_scope = world.parent_scope("bounds-session", "healthy-parent").await;
    let healthy_child = world.until_child("bounds-session", &healthy_scope).await;
    let _healthy_obligation = world.record_end(&healthy_scope).await;

    let pass = world.tick().await;
    assert_eq!(pass.claimed, 2, "the page claims both rows");
    assert_eq!(
        (pass.stalled, pass.delivered),
        (1, 1),
        "the corrupt row stalls and the row behind it delivers in the same pass"
    );

    let stalled = world.stalled().await;
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].id, corrupt_obligation);
    assert_eq!(stalled[0].reason, StallReason::Undecodable);
    assert!(
        world.cancel_requested(&healthy_child).await,
        "the healthy plan's cancel was delivered"
    );
    assert_eq!(
        world
            .ledger
            .state(&corrupt_obligation)
            .await
            .expect("read the stalled state"),
        Some(ObligationState::Stalled),
    );
}

/// No head-of-line starvation (FIG-3853): the due index orders by
/// `obligation_due_at_ms`, not by `ended_at_ms`. A plan recorded first but
/// backed off far must not sit ahead of the row the fleet actually owes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_due_index_serves_the_earliest_due_row_first() {
    let world = World::new().await;
    // A ends first (earliest `ended_at_ms`) but backs off an hour; B ends a
    // millisecond later and stays due at its record instant.
    let scope_a = world.parent_scope("bounds-session", "parent-a").await;
    let obligation_a = world.record_end(&scope_a).await;
    world.clock.advance(1);
    let scope_b = world.parent_scope("bounds-session", "parent-b").await;
    let obligation_b = world.record_end(&scope_b).await;

    let claim_a = world
        .ledger
        .claim(&obligation_a, world.clock.timestamp_ms(), 60_000)
        .await
        .expect("claim a")
        .expect("a is due");
    world
        .ledger
        .settle(
            &obligation_a,
            &claim_a.token,
            ObligationSettlement::Retry {
                due_at_ms: world.clock.timestamp_ms() + 3_600_000,
                error: "a backed off an hour".to_string(),
            },
            world.clock.timestamp_ms(),
        )
        .await
        .expect("a retries far out");

    // At B's due instant only B claims: A is neither served early nor does
    // its earlier `ended_at_ms` push it ahead of B in the index.
    let claims = world
        .ledger
        .claim_due(
            world.clock.timestamp_ms(),
            60_000,
            NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("the due read runs");
    assert_eq!(
        claims
            .iter()
            .map(|claimed| claimed.id.clone())
            .collect::<Vec<_>>(),
        vec![obligation_b.clone()],
        "the due index owes B first; ordering by ended_at would name A"
    );

    // Both due now: the index still serves B (due earlier) before A —
    // `ORDER BY ended_at_ms` would put A first, which is the starvation
    // shape this lane deletes.
    world
        .ledger
        .settle(
            &obligation_b,
            &claims[0].token,
            ObligationSettlement::Retry {
                due_at_ms: world.clock.timestamp_ms(),
                error: "b retries at now".to_string(),
            },
            world.clock.timestamp_ms(),
        )
        .await
        .expect("b retries at now");
    world.clock.advance(3_600_000);
    let claims = world
        .ledger
        .claim_due(
            world.clock.timestamp_ms(),
            60_000,
            NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("the due read runs");
    assert_eq!(
        claims
            .iter()
            .map(|claimed| claimed.id.clone())
            .collect::<Vec<_>>(),
        vec![obligation_b, obligation_a],
        "oldest due first, not oldest ended first"
    );
}
