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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
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
        self.inner.claim(id, token, now_ms, claim_ttl_ms).await
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
                        })
                    } else {
                        ledger
                    }
                })
                .into_store_set()
        },
    )
    .await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .recovery_pass_budget(lash_core::engine::RecoveryPassBudget {
        attempt: std::time::Duration::from_millis(BUDGET_MS),
        tick_wait: std::time::Duration::from_secs(1),
    })
    .provider(mock_provider())
    .model(mock_model_spec())
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
