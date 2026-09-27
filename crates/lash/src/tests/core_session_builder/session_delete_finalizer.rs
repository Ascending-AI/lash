//! A session's two-phase delete through the facade (ADR 0109 §4): the close
//! commits and the session refuses sends, the physical delete is an
//! obligation that waits for the close's cleanup, and a stalled delete is
//! surfaced until an operator re-arms it.

use super::*;

use lash_core::drive::relay::{RelayPolicy, relay_due};
use lash_core::session_delete::SessionDeleteRelay;
use lash_core::store::{ObligationKey, ObligationKind, ObligationSettlement, StallReason};

const SESSION: &str = "delete-finalizer";
const ROOT: &str = "delete-finalizer-root";

fn page() -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(8).expect("non-zero page")
}

fn now_ms() -> u64 {
    lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock)
}

/// A core whose session `SESSION` ran root `ROOT` and whose root still owes
/// its scope close: what S8-S's terminal transaction arms, armed here by
/// the ledger's repair arm.
async fn closing_fixture() -> Result<(LashCore, lash_core::store::ObligationId)> {
    closing_fixture_over(memory_backend().await).await
}

async fn closing_fixture_over(
    backend: Arc<lash_sqlite_store::SqliteBackend>,
) -> Result<(LashCore, lash_core::store::ObligationId)> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).open().await?;
    session
        .send(TurnInput::text("a root that ends before the delete"))
        .id(ROOT)
        .output()
        .await?;
    drop(session);
    let scope_close = core
        .backend
        .obligation_ledger(ObligationKind::ScopeClose)
        .arm(
            &ObligationKey::ScopeClose {
                session_id: SESSION.into(),
                root: ROOT.into(),
            },
            now_ms(),
        )
        .await?
        .expect("the root's row owes nothing yet");
    Ok((core, scope_close))
}

async fn deliver_by_hand(
    core: &LashCore,
    kind: ObligationKind,
    id: &lash_core::store::ObligationId,
) {
    let ledger = core.backend.obligation_ledger(kind);
    let claimed = ledger
        .claim(id, now_ms(), 60_000)
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
    let open = core.session(SESSION).open().await?;
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
    use lash_core::SessionDriver as _;

    let clock = Arc::new(lash_core::testing::TestClock::new(now_ms()));
    let (core, scope_close) =
        closing_fixture_over(memory_backend_with_clock(clock.clone()).await).await?;
    assert!(matches!(
        delete_bound_session_outcome(&core, SESSION).await?,
        crate::SessionDeletion::Closing(_)
    ));
    deliver_by_hand(&core, ObligationKind::ScopeClose, &scope_close).await;
    let driver = crate::core::queued_work::native_queued_work_handle_for_tests(
        &core,
        Arc::clone(&core.store_factory),
    );
    driver
        .reconcile(
            &lash_core::engine::ReconcileCursor::default(),
            page(),
            "held-delete",
        )
        .await?;
    assert!(
        !was_deleted(&core).await?,
        "a tick before the retry's backoff leaves the delete due"
    );
    clock.advance(2_000);
    driver
        .reconcile(
            &lash_core::engine::ReconcileCursor::default(),
            page(),
            "held-delete",
        )
        .await?;
    assert!(
        was_deleted(&core).await?,
        "the tick delivered the due delete"
    );
    Ok(())
}
