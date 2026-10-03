use super::*;
use lash_core::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, ObligationState, SettleOutcome, StalledObligation,
};
use lash_core::{DeploymentStore, DeploymentStoreDecorator, RuntimeStoreDecorator, StoreError};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub(super) struct LifecycleGate {
    pin: AtomicBool,
    pin_reached: AtomicBool,
    pin_reads: AtomicUsize,
    closing_reads: AtomicUsize,
    pin_open: Notify,
    close: AtomicBool,
    close_reached: AtomicBool,
    close_open: Notify,
}

impl LifecycleGate {
    fn release_pin(&self) {
        self.pin.store(false, Ordering::SeqCst);
        self.pin_open.notify_waiters();
    }

    fn release_close(&self) {
        self.close.store(false, Ordering::SeqCst);
        self.close_open.notify_waiters();
    }
}

struct GatedCatalog {
    inner: Arc<dyn DeploymentStore>,
    gate: Arc<LifecycleGate>,
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for GatedCatalog {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn authorize_turn_cancel_closure(
        &self,
        fence: &lash_core::store::ShiftFence,
        authorization: &lash_core::TurnCancelClosureAuthorization,
    ) -> Result<lash_core::TurnCancelClosureAuthorizationOutcome, StoreError> {
        let answer = self
            .inner
            .authorize_turn_cancel_closure(fence, authorization)
            .await?;
        let open = self.gate.pin_open.notified();
        if self.gate.pin.load(Ordering::SeqCst) {
            self.gate.pin_reached.store(true, Ordering::SeqCst);
            open.await;
        }
        Ok(answer)
    }

    async fn lookup_session(
        &self,
        session: &lash::SessionId,
    ) -> Result<lash_core::store::SessionLookup, StoreError> {
        let lookup = self.inner.lookup_session(session).await?;
        if self.gate.close_reached.load(Ordering::SeqCst) {
            self.gate.closing_reads.fetch_add(1, Ordering::SeqCst);
        }
        Ok(lookup)
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session: &lash::SessionId,
    ) -> Result<Vec<lash_core::TurnCancelClosureAuthorization>, StoreError> {
        let pins = self.inner.pending_turn_cancel_closure_pins(session).await?;
        if !pins.is_empty() {
            self.gate.pin_reads.fetch_add(1, Ordering::SeqCst);
        }
        Ok(pins)
    }
}

impl DeploymentStoreDecorator for GatedCatalog {}

struct GatedCloseLedger {
    inner: Arc<dyn ObligationLedger>,
    gate: Arc<LifecycleGate>,
}

#[async_trait::async_trait]
impl ObligationLedger for GatedCloseLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(&self, key: &ObligationKey, now: u64) -> Result<Option<ObligationId>, StoreError> {
        self.inner.arm(key, now).await
    }

    async fn claim_due(
        &self,
        now: u64,
        ttl: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        if self.gate.close.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        self.inner.claim_due(now, ttl, limit).await
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now: u64,
        ttl: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let open = self.gate.close_open.notified();
        if self.gate.close.load(Ordering::SeqCst) {
            self.gate.close_reached.store(true, Ordering::SeqCst);
            open.await;
        }
        self.inner.claim(id, token, now, ttl).await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now: u64,
    ) -> Result<SettleOutcome, StoreError> {
        self.inner.settle(id, token, settlement, now).await
    }

    async fn rearm(&self, id: &ObligationId, now: u64) -> Result<bool, StoreError> {
        self.inner.rearm(id, now).await
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

pub(super) fn gated_backend(
    backend: lash_core::Backend,
    gate: Arc<LifecycleGate>,
) -> lash_core::Backend {
    lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .map_session_store_factory({
            let gate = Arc::clone(&gate);
            move |inner| Arc::new(GatedCatalog { inner, gate })
        })
        .map_obligation_ledgers(move |kind, inner| {
            if kind == ObligationKind::ScopeClose {
                Arc::new(GatedCloseLedger {
                    inner,
                    gate: Arc::clone(&gate),
                })
            } else {
                inner
            }
        })
        .into_backend()
}

fn deletion_attempts(world: &World) -> usize {
    world
        .backend
        .server()
        .invocations()
        .iter()
        .filter(|run| run.target.starts_with("LashTestHandlerHost/"))
        .count()
}

fn print_journals(world: &World, phase: &str) {
    let server = world.backend.server();
    eprintln!("delete phase {phase}; store clock {}", server.now_ms());
    for run in server.invocations() {
        let steps: Vec<_> = server
            .journal(&run.id)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, entry.ty, entry.name.clone()))
            .collect();
        eprintln!("{run:?} {steps:?}");
    }
}

async fn delete_after_answer(stores: Stores, replay: bool, pinned: bool) {
    let gate = Arc::new(LifecycleGate::default());
    let Some(world) = world_over_gated(
        ServerConfig::default().always_replay(replay),
        stores,
        Some(Arc::clone(&gate)),
    )
    .await
    else {
        skipped_without_postgres();
        return;
    };
    let handle = world
        .session
        .send(lash::TurnInput::text("answered before deletion"))
        .await
        .expect("accept");
    wait_until("the model runs", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    if !pinned {
        gate.close.store(true, Ordering::SeqCst);
    }
    world.barrier.release.notify_one();
    let answer = handle.outcome().await.expect("answer");
    assert_eq!(answer.status(), lash::TurnStatus::Answered);
    let run = answer.run().cloned().expect("answered run");
    let scope_close = lash_core::store::ObligationKey::ScopeClose {
        session_id: SESSION.into(),
        run: run.clone(),
    }
    .id();
    if pinned {
        // The previous answer is durable. A subsequent turn's exact closure
        // authorization legitimately pins the same session until its commit.
        gate.pin.store(true, Ordering::SeqCst);
        let _next = world
            .session
            .send(lash::TurnInput::text("closure still finishing"))
            .await
            .expect("accept successor");
        world.barrier.release.notify_one();
        wait_until("the successor pins its closure", || {
            gate.pin_reached.load(Ordering::SeqCst)
        })
        .await;
    } else {
        run_execution_completed(&world, &run).await;
        wait_until("the answered run owes its close", || {
            gate.close_reached.load(Ordering::SeqCst)
        })
        .await;
    }
    let deleting = tokio::spawn({
        let core = world.core.clone();
        let backend = world.backend.clone();
        async move {
            delete_session(&core, SESSION, |attempt| {
                backend.run_in_handler(lash_core::AdmittedScope::session_delete(SESSION), attempt)
            })
            .await;
        }
    });
    if pinned {
        wait_until("the waiter reads the retained pin", || {
            gate.pin_reads.load(Ordering::SeqCst) >= 5
        })
        .await;
        print_journals(&world, "pinned");
        assert_eq!(
            deletion_attempts(&world),
            1,
            "a retained pin is awaited without repeating delete"
        );
        gate.release_pin();
    } else {
        wait_until("the delete closes the session", || {
            world.backend.server().invocations().iter().any(|executed| {
                executed.target.starts_with("LashTestHandlerHost/")
                    && executed.status == "completed"
            })
        })
        .await;
        let reads = gate.closing_reads.load(Ordering::SeqCst);
        wait_until("the waiter observes closing or returns", || {
            deleting.is_finished() || gate.closing_reads.load(Ordering::SeqCst) >= reads + 5
        })
        .await;
        print_journals(&world, "closing");
        assert!(
            !deleting.is_finished(),
            "Closing is awaited through the finalizer, not treated as completed deletion"
        );
        assert_eq!(
            world
                .backend
                .lash_backend()
                .obligation_ledger(ObligationKind::ScopeClose)
                .state(&scope_close)
                .await
                .expect("scope-close state"),
            Some(ObligationState::Due)
        );
        gate.release_close();
    }
    finish_cleanup(&world, &scope_close).await;
    tokio::time::timeout(Duration::from_secs(60), deleting)
        .await
        .expect("the state-driven deletion completes")
        .expect("delete joined");
    assert!(
        world
            .core
            .session(SESSION)
            .durable()
            .await
            .expect("durable handle")
            .was_deleted()
            .await
            .expect("deletion tombstone")
    );
    print_journals(&world, "deleted");
}

macro_rules! laws {
    ($($(#[$attr:meta])* $name:ident, $store:ident, $replay:expr, $pinned:expr;)*) => {$ (
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() { delete_after_answer(Stores::$store, $replay, $pinned).await; }
    )*};
}

laws! {
    a_pinned_delete_waits_for_its_closure_on_sqlite_memory, SqliteMemory, false, true;
    a_pinned_delete_waits_for_its_closure_on_sqlite_memory_replaying, SqliteMemory, true, true;
    a_pinned_delete_waits_for_its_closure_on_sqlite_file, SqliteFile, false, true;
    a_pinned_delete_waits_for_its_closure_on_sqlite_file_replaying, SqliteFile, true, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    a_pinned_delete_waits_for_its_closure_on_postgres, Postgres, false, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    a_pinned_delete_waits_for_its_closure_on_postgres_replaying, Postgres, true, true;
    a_closing_delete_waits_for_its_scope_close_on_sqlite_memory, SqliteMemory, false, false;
    a_closing_delete_waits_for_its_scope_close_on_sqlite_memory_replaying, SqliteMemory, true, false;
    a_closing_delete_waits_for_its_scope_close_on_sqlite_file, SqliteFile, false, false;
    a_closing_delete_waits_for_its_scope_close_on_sqlite_file_replaying, SqliteFile, true, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    a_closing_delete_waits_for_its_scope_close_on_postgres, Postgres, false, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    a_closing_delete_waits_for_its_scope_close_on_postgres_replaying, Postgres, true, false;
}

/// The next finalizer delivery after cleanup settles. No repeated close, and
/// no elapsed-time oracle: one due pass discharges the accepted deletion.
async fn finish_cleanup(world: &World, scope_close: &ObligationId) {
    finish_session_cleanup(world, SESSION).await;
    let state = world
        .backend
        .lash_backend()
        .obligation_ledger(ObligationKind::ScopeClose)
        .state(scope_close)
        .await
        .expect("scope close");
    assert!(
        matches!(state, Some(ObligationState::Delivered) | None),
        "the scope-close obligation discharged: {state:?}"
    );
}

pub(super) async fn finish_session_cleanup(world: &World, session_id: &str) {
    let session = lash::SessionId::fixture(session_id);
    let backend = world.backend.lash_backend();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if backend
                .session_delete_ledger()
                .undelivered_cleanup(&session)
                .await
                .expect("cleanup state")
                .is_settled()
                && (backend
                    .session_delete_ledger()
                    .delete_obligation(&session)
                    .await
                    .expect("delete state")
                    .is_some()
                    || world
                        .core
                        .session(lash_core::SessionId::fixture(session_id))
                        .durable()
                        .await
                        .expect("handle")
                        .was_deleted()
                        .await
                        .expect("tombstone"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("run cleanup discharges when its closure ends");
    let relay = lash_core::session_delete::SessionDeleteRelay::new(
        world.core.session_administration().await,
    );
    let clock = lash_core::testing::TestClock::new(world.backend.server().now_ms() + 2_000);
    let pass = lash_core::shift::relay::relay_due(&relay, &clock, NonZeroUsize::MIN)
        .await
        .expect("next due finalizer pass");
    eprintln!("finalizer after cleanup {pass:?}");
}

async fn live_delete_after_answer(pinned: bool) {
    let gate = Arc::new(LifecycleGate::default());
    let world = live_world_gated("delete-after-answer", Some(Arc::clone(&gate))).await;
    let handle = world
        ._session
        .send(lash::TurnInput::text("answered before deletion"))
        .await
        .expect("accept");
    wait_until("the model runs", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    if !pinned {
        gate.close.store(true, Ordering::SeqCst);
    }
    world.barrier.release.notify_one();
    assert_eq!(
        handle.outcome().await.expect("answer").status(),
        lash::TurnStatus::Answered
    );
    if pinned {
        gate.pin.store(true, Ordering::SeqCst);
        let _next = world
            ._session
            .send(lash::TurnInput::text("closure still finishing"))
            .await
            .expect("accept successor");
        world.barrier.release.notify_one();
        wait_until("the closure is pinned", || {
            gate.pin_reached.load(Ordering::SeqCst)
        })
        .await;
    } else {
        wait_until("the run owes its close", || {
            gate.close_reached.load(Ordering::SeqCst)
        })
        .await;
    }
    let deleting = tokio::spawn({
        let core = world._core.clone();
        let backend = world.backend.clone();
        let key = world.key.clone();
        async move {
            delete_session(&core, &key, |attempt| {
                backend.run_in_handler(
                    lash_core::AdmittedScope::session_delete(lash::SessionId::fixture(key.clone())),
                    attempt,
                )
            })
            .await;
        }
    });
    if pinned {
        wait_until("the waiter observes retained closure", || {
            gate.pin_reads.load(Ordering::SeqCst) >= 5
        })
        .await;
        let attempts = world
            .backend
            .invocations()
            .await
            .expect("invocations")
            .iter()
            .filter(|run| {
                run.target.starts_with("LashTestHandlerHost/") && run.target.contains(&world.key)
            })
            .count();
        assert_eq!(attempts, 1, "wait for the pin without repeating delete");
        gate.release_pin();
    } else {
        let reads = gate.closing_reads.load(Ordering::SeqCst);
        wait_until("the waiter observes retained close", || {
            gate.closing_reads.load(Ordering::SeqCst) >= reads + 5 || deleting.is_finished()
        })
        .await;
        assert!(
            !deleting.is_finished(),
            "Closing is awaited through its physical delete"
        );
        gate.release_close();
    }
    tokio::time::timeout(Duration::from_secs(60), deleting)
        .await
        .expect("deletion completes after cleanup")
        .expect("joined");
    assert!(
        world
            ._core
            .session(lash_core::SessionId::fixture(world.key.as_str()))
            .durable()
            .await
            .expect("handle")
            .was_deleted()
            .await
            .expect("tombstone")
    );
    for run in world.backend.invocations().await.expect("invocations") {
        eprintln!("{run:?}");
        if run.target.starts_with("LashTestHandlerHost/") || run.target.starts_with("LashTurn/") {
            eprintln!(
                "journal {:?}",
                world.backend.journal(&run.id).await.expect("journal")
            );
        }
    }
    world.backend.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait suite runs it"]
async fn live_restate_a_pinned_delete_waits_for_its_closure() {
    live_delete_after_answer(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the host-send-wait suite runs it"]
async fn live_restate_a_closing_delete_waits_for_its_scope_close() {
    live_delete_after_answer(false).await;
}

async fn deletion_wait_reports_state_and_stalls(stores: Stores, replay: bool) {
    let gate = Arc::new(LifecycleGate::default());
    let Some(world) = world_over_gated(
        ServerConfig::default().always_replay(replay),
        stores,
        Some(Arc::clone(&gate)),
    )
    .await
    else {
        skipped_without_postgres();
        return;
    };
    let session = lash::SessionId::from(SESSION);
    assert_eq!(
        world
            .core
            .await_session_deletion(&"never-created".into())
            .await
            .expect("absent"),
        lash::SessionDeleteCompletion::Absent
    );
    assert_eq!(
        world
            .core
            .await_session_deletion(&session)
            .await
            .expect("open"),
        lash::SessionDeleteCompletion::NotClosing
    );
    gate.close.store(true, Ordering::SeqCst);
    let handle = world
        .session
        .send(lash::TurnInput::text("stalled cleanup"))
        .await
        .expect("accept");
    wait_until("the model runs", || {
        world.barrier.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    world.barrier.release.notify_one();
    let run = handle
        .outcome()
        .await
        .expect("answer")
        .run()
        .cloned()
        .expect("root");
    run_execution_completed(&world, &run).await;
    wait_until("the run close is owed", || {
        gate.close_reached.load(Ordering::SeqCst)
    })
    .await;
    let (attempt, answer) = deletion(&world.core, SESSION).await;
    world
        .backend
        .run_in_handler(lash_core::AdmittedScope::session_delete(SESSION), attempt)
        .await
        .expect("delete handler");
    assert!(matches!(
        answer.lock().expect("answer").take(),
        Some(Ok(lash::SessionDeletion::Closing(_)))
    ));
    let id = lash_core::store::ObligationKey::ScopeClose {
        session_id: session.clone(),
        run: run.clone(),
    }
    .id();
    let ledger = world
        .backend
        .lash_backend()
        .obligation_ledger(ObligationKind::ScopeClose);
    let claim = ledger
        .claim(
            &id,
            &ClaimToken::mint(),
            world.backend.server().now_ms(),
            60_000,
        )
        .await
        .expect("claim")
        .expect("due");
    assert_eq!(
        ledger
            .settle(
                &id,
                &claim.token,
                ObligationSettlement::Stall {
                    reason: lash_core::store::StallReason::Refused,
                    error: lash_core::store::DeliveryError::new(
                        lash_core::RuntimeErrorCode::EngineControlRequest,
                        "scope close needs operator repair"
                    ),
                },
                world.backend.server().now_ms()
            )
            .await
            .expect("stall"),
        SettleOutcome::Applied
    );
    let result = world
        .core
        .await_session_deletion(&session)
        .await
        .expect("typed stalled dependency");
    let lash::SessionDeleteCompletion::Stalled(stalled) = result else {
        panic!("the waiter returns a stalled cleanup: {result:?}");
    };
    assert_eq!(stalled.kind, ObligationKind::ScopeClose);
    assert_eq!(stalled.id, id);
    assert_eq!(
        stalled
            .last_error
            .as_ref()
            .map(|error| error.message.as_str()),
        Some("scope close needs operator repair")
    );
    assert!(
        world
            .core
            .rearm_obligation(ObligationKind::ScopeClose, &id)
            .await
            .expect("explicit re-arm")
    );
    gate.release_close();
    finish_cleanup(&world, &id).await;
    assert_eq!(
        world
            .core
            .await_session_deletion(&session)
            .await
            .expect("deleted"),
        lash::SessionDeleteCompletion::Deleted
    );
}

macro_rules! completion_laws {
    ($($(#[$attr:meta])* $name:ident, $store:ident, $replay:expr;)*) => {$ (
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() { deletion_wait_reports_state_and_stalls(Stores::$store, $replay).await; }
    )*};
}
completion_laws! {
    deletion_wait_reports_state_and_stalls_on_sqlite_memory, SqliteMemory, false;
    deletion_wait_reports_state_and_stalls_on_sqlite_memory_replaying, SqliteMemory, true;
    deletion_wait_reports_state_and_stalls_on_sqlite_file, SqliteFile, false;
    deletion_wait_reports_state_and_stalls_on_sqlite_file_replaying, SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    deletion_wait_reports_state_and_stalls_on_postgres, Postgres, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    deletion_wait_reports_state_and_stalls_on_postgres_replaying, Postgres, true;
}

async fn run_execution_completed(world: &World, run: &lash::TurnId) {
    wait_until(
        "the run's execution has completed beside its held close",
        || {
            world
                .backend
                .server()
                .turn_invocations(&SESSION.into(), run)
                .iter()
                .any(|executed| executed.status == "completed")
        },
    )
    .await;
}
