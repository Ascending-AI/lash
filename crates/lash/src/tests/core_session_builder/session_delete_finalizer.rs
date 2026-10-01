//! A session's two-phase delete through the facade (ADR 0109 §4): the close
//! commits and the session refuses sends, the physical delete is an
//! obligation that waits for the close's cleanup, and a stalled delete is
//! surfaced until an operator re-arms it.

use super::*;

use lash_core::drive::relay::{RelayPolicy, relay_due};
use lash_core::session_delete::SessionDeleteRelay;
use lash_core::store::{ObligationKind, ObligationSettlement, StallReason};

const SESSION: &str = "delete-finalizer";
const ROOT: &str = "delete-finalizer-root";

fn page() -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(8).expect("non-zero page")
}

fn now_ms() -> u64 {
    lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock)
}

/// A core whose session `SESSION` ran root `ROOT` and whose root still owes
/// its scope close: the root's terminal transaction armed it (ADR 0109 §3)
/// and the close step's immediate delivery failed — the registry refused the
/// root's parent-end record once — so the obligation is due for a retry.
///
/// The core runs on a Restate double whose virtual clock starts at the wall
/// clock, so the relay passes below, timed off the wall clock, see the
/// obligations the double's stores scheduled.
async fn closing_fixture() -> Result<(LashCore, lash_core::store::ObligationId)> {
    closing_fixture_under(lash_restate_test::TimeMode::auto()).await
}

async fn closing_fixture_under(
    time: lash_restate_test::TimeMode,
) -> Result<(LashCore, lash_core::store::ObligationId)> {
    let root_scope = lash_core::ScopeId::turn(SESSION, ROOT);
    let backend = double_backend_over_explicit_reconcile(
        lash_restate_test::ServerConfig {
            start_time_ms: now_ms(),
            time,
            ..lash_restate_test::ServerConfig::default()
        },
        move |stores| {
            lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_process_registry(|registry| {
                    lash_core::fail_parent_end_once(registry, root_scope)
                })
                .into_store_set()
        },
    )
    .await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).created().await.open().await?;
    session
        .send(TurnInput::text("a root that ends before the delete"))
        .id(ROOT)
        .output()
        .await?;
    drop(session);
    // The handle answered at the root's final commit; its close step runs
    // after (FIG-3979).
    settle_session_drive(&core, SESSION).await;
    let scope_close = lash_core::store::scope_close_obligation_id(&SESSION.into(), &ROOT.into());
    assert_eq!(
        core.backend
            .obligation_ledger(ObligationKind::ScopeClose)
            .state(&scope_close)
            .await?,
        Some(lash_core::store::ObligationState::Due),
        "the root's failed close left its scope close owed"
    );
    Ok((core, scope_close))
}

async fn deliver_by_hand(
    core: &LashCore,
    kind: ObligationKind,
    id: &lash_core::store::ObligationId,
) {
    let ledger = core.backend.obligation_ledger(kind);
    let claimed = ledger
        .claim(id, &lash_core::store::ClaimToken::mint(), now_ms(), 60_000)
        .await
        .expect("claim")
        .expect("due");
    ledger
        .settle(
            id,
            &claimed.token,
            ObligationSettlement::Delivered,
            now_ms(),
        )
        .await
        .expect("settle");
}

async fn was_deleted(core: &LashCore) -> Result<bool> {
    core.session(SESSION).durable().await?.was_deleted().await
}

/// The finalizer rule: the delete closes the session at once — it refuses
/// sends typed — but its physical delete waits, retried by the relay, until
/// the scope close its root still owes is delivered; then the next attempt
/// deletes it.
#[tokio::test]
async fn the_physical_delete_waits_for_the_closes_cleanup() -> Result<()> {
    let (core, scope_close) = closing_fixture().await?;

    let deletion = delete_bound_session_outcome(&core, SESSION).await?;
    let crate::SessionDeletion::Closing(closing) = deletion else {
        panic!("an undelivered scope close holds the delete, got {deletion:?}");
    };
    assert!(
        matches!(
            closing.waiting,
            crate::SessionDeleteWait::Cleanup(lash_core::store::session_delete::SessionCleanup {
                scope_close: 1,
                parent_end: 0,
            })
        ),
        "{:?}",
        closing.waiting
    );
    assert!(closing.obligation.is_some(), "the close armed the delete");
    assert!(!was_deleted(&core).await?);
    let open = core.session(SESSION).created().await.open().await?;
    let refused = open
        .send(TurnInput::text("sent while closing"))
        .output()
        .await;
    assert!(
        matches!(
            &refused,
            Err(EmbedError::Runtime(error))
                if error.code == lash_core::RuntimeErrorCode::SessionDeleted
                    && error.message.contains("is closing")
        ),
        "a closing session refuses a send typed: {:?}",
        refused.as_ref().err()
    );
    drop(open);

    let relay = SessionDeleteRelay::new(core.session_administration().await);
    let held = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now_ms() + 2_000),
        page(),
    )
    .await?;
    assert_eq!((held.claimed, held.retried), (1, 1), "{held:?}");
    assert!(!was_deleted(&core).await?);

    deliver_by_hand(&core, ObligationKind::ScopeClose, &scope_close).await;
    let pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now_ms() + 10_000),
        page(),
    )
    .await?;
    assert_eq!((pass.claimed, pass.claim_lost), (1, 1), "{pass:?}");
    assert!(was_deleted(&core).await?);
    Ok(())
}

/// A delete whose cleanup never settles stalls at the attempt ceiling and is
/// surfaced — in the drain status and the stalled listing — never dropped and
/// never retried until re-armed; re-armed after its cleanup settles, it
/// deletes the session.
#[tokio::test]
async fn a_stalled_delete_is_surfaced_until_rearmed() -> Result<()> {
    let (core, scope_close) = closing_fixture().await?;
    let crate::SessionDeletion::Closing(closing) =
        delete_bound_session_outcome(&core, SESSION).await?
    else {
        panic!("an undelivered scope close holds the delete");
    };
    let delete = closing.obligation.expect("the close armed the delete");

    let policy = RelayPolicy {
        attempt_ceiling: std::num::NonZeroU32::new(2).expect("non-zero ceiling"),
        ..RelayPolicy::default()
    };
    let relay = SessionDeleteRelay::with_policy(core.session_administration().await, policy);
    let pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now_ms() + 2_000),
        page(),
    )
    .await?;
    assert_eq!((pass.claimed, pass.stalled), (1, 1), "{pass:?}");
    let later = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now_ms() + 3_600_000),
        page(),
    )
    .await?;
    assert_eq!(later.claimed, 0, "a stalled delete is never retried");

    let status = core.drain_status(false).await?;
    assert_eq!(
        status.stalled_obligations[&ObligationKind::SessionDelete],
        1
    );
    assert!(!status.drained(), "a stalled delete holds the drain");
    let stalled = core
        .stalled_obligations(ObligationKind::SessionDelete, None, page())
        .await?;
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].id, delete);
    assert_eq!(stalled[0].reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled[0].attempts, 2);
    assert!(
        stalled[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("waits on its cleanup")),
        "{:?}",
        stalled[0].last_error
    );
    assert!(!was_deleted(&core).await?);

    deliver_by_hand(&core, ObligationKind::ScopeClose, &scope_close).await;
    assert!(
        core.rearm_obligation(ObligationKind::SessionDelete, &delete)
            .await?
    );
    let pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now_ms() + 1),
        page(),
    )
    .await?;
    assert_eq!((pass.claimed, pass.claim_lost), (1, 1), "{pass:?}");
    assert!(was_deleted(&core).await?);
    Ok(())
}

/// The deployment's reconcile tick carries the session-delete relay: a
/// delete held by its cleanup is finished by the tick after the cleanup is
/// delivered, with no call from the host.
#[tokio::test]
async fn the_reconcile_tick_finishes_a_held_delete() -> Result<()> {
    let (core, scope_close) = closing_fixture_under(lash_restate_test::TimeMode::Manual).await?;
    let double = held_double(&core).expect("the fixture runs on a held double");
    assert!(matches!(
        delete_bound_session_outcome(&core, SESSION).await?,
        crate::SessionDeletion::Closing(_)
    ));
    deliver_by_hand(&core, ObligationKind::ScopeClose, &scope_close).await;
    let driver = Arc::clone(&core._session_driver);
    driver
        .reconcile(&lash_core::engine::ReconcileCursor::default(), page())
        .await?;
    assert!(
        !was_deleted(&core).await?,
        "a tick before the retry's backoff leaves the delete due"
    );
    double
        .server()
        .advance(std::time::Duration::from_millis(2_000));
    driver
        .reconcile(&lash_core::engine::ReconcileCursor::default(), page())
        .await?;
    assert!(
        was_deleted(&core).await?,
        "the tick delivered the due delete"
    );
    Ok(())
}

/// An obligation ledger that forwards every call and records each
/// settlement it is asked to write: a retry's error names the budget the
/// abandoned attempt ran past, which `ObligationStanding` does not carry
/// (FIG-4246).
struct RecordingLedger {
    inner: Arc<dyn lash_core::store::ObligationLedger>,
    settlements: Arc<std::sync::Mutex<Vec<(lash_core::store::ObligationId, ObligationSettlement)>>>,
    pause: Option<Arc<DeleteDeliveryPause>>,
}

type Settlements =
    Arc<std::sync::Mutex<Vec<(lash_core::store::ObligationId, ObligationSettlement)>>>;

#[async_trait::async_trait]
impl lash_core::store::ObligationLedger for RecordingLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &lash_core::store::ObligationKey,
        now_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ObligationId>, lash_core::StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ClaimedObligation>, lash_core::StoreError> {
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &lash_core::store::ObligationId,
        token: &lash_core::store::ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ClaimedObligation>, lash_core::StoreError>
    {
        let claimed = self.inner.claim(id, token, now_ms, claim_ttl_ms).await?;
        if claimed.is_some()
            && let Some(pause) = &self.pause
            && pause.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let server = pause
                .server
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("the double is built");
            let hold = server.hold("LashDurableWaitIndex", SESSION).await;
            *pause
                .hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hold);
        }
        Ok(claimed)
    }

    async fn settle(
        &self,
        id: &lash_core::store::ObligationId,
        token: &lash_core::store::ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> std::result::Result<lash_core::store::SettleOutcome, lash_core::StoreError> {
        self.settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id.clone(), settlement.clone()));
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(
        &self,
        id: &lash_core::store::ObligationId,
        now_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&lash_core::store::ObligationId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::StalledObligation>, lash_core::StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> std::result::Result<u64, lash_core::StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(
        &self,
        id: &lash_core::store::ObligationId,
    ) -> std::result::Result<Option<lash_core::store::ObligationStanding>, lash_core::StoreError>
    {
        self.inner.standing(id).await
    }
}

/// The host's configured attempt budget bounds an immediate `deliver_now`
/// too (FIG-4246): a delete's `CloseSession` intent, delivered at once
/// through the control-intent relay, is abandoned when its attempt outlives
/// the configured budget, and the retry it settles names that budget — not
/// the `RelayPolicy` default's 30 s.
#[tokio::test]
async fn an_immediate_delivery_runs_under_the_configured_attempt_budget() -> Result<()> {
    const BUDGET_MS: u64 = 150;
    let session_scope = lash_core::ScopeId::session(SESSION);
    let settlements: Settlements = Arc::default();
    let layer_settlements = Arc::clone(&settlements);
    let backend = double_backend_over_explicit_reconcile(
        lash_restate_test::ServerConfig {
            start_time_ms: now_ms(),
            time: lash_restate_test::TimeMode::auto(),
            ..lash_restate_test::ServerConfig::default()
        },
        move |stores| {
            lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_process_registry(move |registry| {
                    let faults = lash_core::testing::ProcessRegistryFaults::new(registry);
                    faults.hold_parent_end(session_scope);
                    Arc::new(faults)
                })
                .map_obligation_ledgers(move |kind, ledger| {
                    if kind == ObligationKind::ControlIntent {
                        Arc::new(RecordingLedger {
                            inner: ledger,
                            settlements: Arc::clone(&layer_settlements),
                            pause: None,
                        })
                    } else {
                        ledger
                    }
                })
                .into_store_set()
        },
    )
    .await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .recovery_pass_budget(lash_core::engine::RecoveryPassBudget {
            attempt: std::time::Duration::from_millis(BUDGET_MS),
            tick_wait: std::time::Duration::from_secs(1),
        })
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).created().await.open().await?;
    session
        .send(TurnInput::text("a root that ends before the delete"))
        .id(ROOT)
        .output()
        .await?;
    drop(session);
    settle_session_drive(&core, SESSION).await;

    // The close intent's engine half ends the session's scope, whose
    // parent-end record the registry never answers: the immediate delivery
    // runs until its attempt is abandoned at the host's configured budget.
    // The timeout only bounds a run still waiting on the 30 s default.
    let deletion = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        delete_bound_session_outcome(&core, SESSION),
    )
    .await
    .expect("the delete returns once the held close outlives the configured budget")?;
    let crate::SessionDeletion::Closing(closing) = deletion else {
        panic!("a held session scope close leaves the delete closing, got {deletion:?}");
    };
    assert!(
        matches!(
            closing.waiting,
            crate::SessionDeleteWait::CloseIntent(lash_core::store::ControlIntentState::Pending)
        ),
        "{:?}",
        closing.waiting
    );
    let retries: Vec<String> = settlements
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|(_, settlement)| match settlement {
            ObligationSettlement::Retry { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert!(
        retries
            .iter()
            .any(|error| error.contains(&format!("past its {BUDGET_MS} ms attempt budget"))),
        "the immediate attempt's retry names the configured {BUDGET_MS} ms budget: {retries:?}"
    );
    Ok(())
}

#[derive(Default)]
struct DeleteDeliveryPause {
    server: std::sync::Mutex<Option<lash_restate_test::RestateTestServer>>,
    armed: std::sync::atomic::AtomicBool,
    hold: std::sync::Mutex<Option<lash_restate_test::Hold>>,
}

/// The delete claims its real SQL obligation, then Restate's session wait
/// index stops before physical deletion's revoke_all can answer. Virtual
/// time controls retry eligibility; the held call exhausts the attempt budget.
#[allow(
    clippy::disallowed_methods,
    reason = "the test records runner load with its timing-sensitive regression"
)]
async fn delete_delivery_exhausts_its_budget(
    make_stores: impl AsyncFnOnce(Arc<dyn lash_core::Clock>) -> Arc<dyn lash_core::StoreSet>,
    stall: bool,
) -> Result<()> {
    const BUDGET_MS: u64 = 2_000;
    eprintln!(
        "runner load: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_else(|error| format!("unavailable: {error}"))
    );
    let pause = Arc::new(DeleteDeliveryPause::default());
    let settlements: Settlements = Arc::default();
    let layer_pause = Arc::clone(&pause);
    let layer_settlements = Arc::clone(&settlements);
    let double = lash_restate_test::backend_with_store_set(
        0x4342,
        lash_restate_test::ServerConfig {
            start_time_ms: now_ms(),
            time: lash_restate_test::TimeMode::Manual,
            ..Default::default()
        },
        Default::default(),
        async move |clock| {
            let stores = make_stores(clock).await;
            Ok(
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_obligation_ledgers(move |kind, inner| {
                        if kind == ObligationKind::SessionDelete {
                            Arc::new(RecordingLedger {
                                inner,
                                settlements: Arc::clone(&layer_settlements),
                                pause: Some(Arc::clone(&layer_pause)),
                            })
                        } else {
                            inner
                        }
                    })
                    .into_store_set(),
            )
        },
    )
    .await
    .expect("the Restate double over SQL stores");
    *pause
        .server
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(double.server().clone());
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(double.lash_backend())
        .with_session_work(double.explicit_reconcile_session_work())
        .into_backend();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .recovery_pass_budget(lash_core::engine::RecoveryPassBudget {
            attempt: std::time::Duration::from_millis(BUDGET_MS),
            tick_wait: std::time::Duration::from_secs(1),
        })
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    create_catalog_session(&core, SESSION).await?;
    let ledger = double
        .engine_stores()
        .obligation_ledger(ObligationKind::SessionDelete);
    super::super::scope_support::in_delete_handler(&double, &core, SESSION, async |context| {
        let closed = lash_core::session_close::close_session(&context)
            .await
            .expect("close the session")
            .expect("the session exists");
        assert!(
            matches!(
                closed.applied,
                lash_core::store::ControlIntentState::Acknowledged { .. }
            ),
            "the close must finish before the physical-delete fault: {closed:?}"
        );
        let obligation = double
            .engine_stores()
            .session_delete_ledger()
            .delete_obligation(&SessionId::from(SESSION))
            .await?
            .expect("close armed the delete");
        let now = double.server().now_ms();
        if stall {
            for _ in 1..RelayPolicy::default().attempt_ceiling.get() {
                let claim = ledger
                    .claim(
                        &obligation.id,
                        &lash_core::store::ClaimToken::mint(),
                        now,
                        60_000,
                    )
                    .await?
                    .expect("the previous retry is due");
                ledger
                    .settle(
                        &obligation.id,
                        &claim.token,
                        ObligationSettlement::Retry {
                            due_at_ms: now,
                            error: "earlier delivery exhausted its budget".into(),
                        },
                        now,
                    )
                    .await?;
            }
        }
        pause.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let deletion = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            LashCore::delete_session(context),
        )
        .await
        .expect("the budget ends the held delivery");
        let crate::SessionDeletion::Closing(closing) = deletion? else {
            panic!("a timed-out physical delete returns Closing");
        };
        assert_eq!(closing.obligation.as_ref(), Some(&obligation.id));
        let expected = if stall {
            crate::RelayVerdict::Stalled(StallReason::AttemptsExhausted)
        } else {
            crate::RelayVerdict::Retried {
                due_at_ms: now + RelayPolicy::default().base_backoff_ms,
            }
        };
        assert!(
            matches!(closing.waiting, crate::SessionDeleteWait::Delivery(ref verdict)
            if *verdict == expected),
            "the unrecorded attempt retains its typed verdict: {closing:?}"
        );
        let expected_state = if stall {
            lash_core::store::ObligationState::Stalled
        } else {
            lash_core::store::ObligationState::Due
        };
        assert_eq!(ledger.state(&obligation.id).await?, Some(expected_state));
        assert!(matches!(
            double
                .engine_stores()
                .session_store_factory()
                .lookup_session(&SessionId::from(SESSION))
                .await?,
            lash_core::store::SessionLookup::Live(_)
        ));
        assert!(
            settlements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|(_, settlement)| match settlement {
                    ObligationSettlement::Retry { error, .. }
                    | ObligationSettlement::Stall { error, .. } =>
                        error.contains(&format!("past its {BUDGET_MS} ms attempt budget")),
                    ObligationSettlement::Delivered | ObligationSettlement::Defer { .. } => false,
                })
        );
        Ok(())
    })
    .await?;
    pause
        .hold
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the delivery held the wait index")
        .release();
    if stall {
        let obligation = double
            .engine_stores()
            .session_delete_ledger()
            .delete_obligation(&SessionId::from(SESSION))
            .await?
            .expect("stalled delete retained");
        assert!(
            ledger
                .rearm(&obligation.id, double.server().now_ms())
                .await?
        );
    }
    double
        .server()
        .advance(std::time::Duration::from_millis(2_000));
    let relay = SessionDeleteRelay::new(core.session_administration().await);
    let pass = relay_due(&relay, double.test_clock().as_ref(), page()).await?;
    assert_eq!((pass.claimed, pass.claim_lost), (1, 1), "{pass:?}");
    assert!(matches!(
        double
            .engine_stores()
            .session_store_factory()
            .lookup_session(&SessionId::from(SESSION))
            .await?,
        lash_core::store::SessionLookup::Deleted
    ));
    eprintln!(
        "PASS delete delivery budget: stalled={stall}; recovery physically deleted the session"
    );
    Ok(())
}

async fn sqlite_delete_budget(file: bool, stall: bool) -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    Box::pin(delete_delivery_exhausts_its_budget(
        async |clock| {
            let stores = if file {
                lash_sqlite_store::SqliteStoreSet::open_with_clock(directory.path(), clock).await
            } else {
                lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock).await
            }
            .expect("open SQLite stores");
            Arc::new(stores) as Arc<dyn lash_core::StoreSet>
        },
        stall,
    ))
    .await
}

#[allow(
    clippy::disallowed_methods,
    reason = "service test reads its required PostgreSQL URL"
)]
async fn postgres_delete_budget(stall: bool) -> Result<()> {
    let url =
        std::env::var("LASH_POSTGRES_DATABASE_URL").expect("the PostgreSQL gate sets its URL");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url()).await?;
    let attachments = tempfile::tempdir().expect("attachment directory");
    Box::pin(delete_delivery_exhausts_its_budget(
        async |clock| {
            Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachments.path(),
                )),
                Default::default(),
                clock,
            )) as Arc<dyn lash_core::StoreSet>
        },
        stall,
    ))
    .await
}

#[tokio::test]
async fn an_unrecorded_delete_retry_returns_typed_on_sqlite_memory() -> Result<()> {
    Box::pin(sqlite_delete_budget(false, false)).await
}
#[tokio::test]
async fn an_unrecorded_delete_retry_returns_typed_on_sqlite_file() -> Result<()> {
    Box::pin(sqlite_delete_budget(true, false)).await
}
#[tokio::test]
async fn an_unrecorded_delete_stall_returns_typed_on_sqlite_memory() -> Result<()> {
    Box::pin(sqlite_delete_budget(false, true)).await
}
#[tokio::test]
async fn an_unrecorded_delete_stall_returns_typed_on_sqlite_file() -> Result<()> {
    Box::pin(sqlite_delete_budget(true, true)).await
}
#[tokio::test]
#[ignore = "requires the PostgreSQL service gate"]
async fn an_unrecorded_delete_retry_returns_typed_on_postgres() -> Result<()> {
    Box::pin(postgres_delete_budget(false)).await
}
#[tokio::test]
#[ignore = "requires the PostgreSQL service gate"]
async fn an_unrecorded_delete_stall_returns_typed_on_postgres() -> Result<()> {
    Box::pin(postgres_delete_budget(true)).await
}
