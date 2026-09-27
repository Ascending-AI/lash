//! L-D7 through L-D9: a session's two-phase delete (ADR 0109 §4). The
//! close's acknowledgement arms the session's `SessionDelete` obligation, the
//! obligation counts exactly the session's undelivered cleanup, and its
//! delivery — the physical delete — waits for that cleanup and then deletes
//! the session.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lash_core::drive::relay::{RelayVerdict, deliver_now, relay_due};
use lash_core::session_delete::SessionDeleteRelay;
use lash_core::store::session_delete::SessionCleanup;
use lash_core::store::{
    ObligationId, ObligationKey, ObligationKind, ObligationLedger, ObligationSettlement,
    ObligationState, StallReason,
};
use lash_core::{ScopeId, StoreSet, TurnId};

use super::session_close::{CloseSink, administration, close, session};

/// A claim on `id` settled as `settlement`: what a kind's relay does, done by
/// hand for a ledger whose producer slice is not the law's subject.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn settle_by_hand(
    ledger: &dyn ObligationLedger,
    id: &ObligationId,
    settlement: ObligationSettlement,
    now_ms: u64,
) {
    let claimed = ledger
        .claim(id, now_ms, 60_000)
        .await
        .expect("claim the obligation")
        .expect("the obligation is due");
    ledger
        .settle(id, &claimed.token, settlement, now_ms)
        .await
        .expect("settle the claim");
}

/// Arm `key`'s row, as its producer slice's transaction would.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn arm(stores: &Arc<dyn StoreSet>, key: ObligationKey, now_ms: u64) -> ObligationId {
    stores
        .obligation_ledger(key.kind())
        .arm(&key, now_ms)
        .await
        .expect("arm the row")
        .expect("the row exists and owes nothing")
}

/// L-D7: the close's acknowledgement arms the session's `SessionDelete`
/// obligation in its own transaction, due at once; an unacknowledged close
/// arms nothing, and a repeated close keeps the one obligation.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_close_acknowledgement_arms_the_session_delete_obligation(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let ledger = stores.session_delete_ledger();
    let factory = stores.session_store_factory();

    let (retained, _) = session(&stores, prefix, "delete-arm-retained").await;
    let failing = administration(
        Arc::clone(&host),
        &stores,
        CloseSink::new(Arc::clone(&factory), 1),
    );
    let intent = close(&failing, &retained, runner.as_ref()).await;
    assert!(
        ledger
            .delete_obligation(&retained)
            .await
            .expect("read the delete obligation")
            .is_none(),
        "a close whose engine half is retained owes no delete yet: {:?}",
        factory.load_intent(intent.id).await.expect("read intent")
    );

    let (id, _) = session(&stores, prefix, "delete-arm").await;
    let admin = administration(host, &stores, CloseSink::new(Arc::clone(&factory), 0));
    assert!(
        ledger
            .delete_obligation(&id)
            .await
            .expect("read the delete obligation")
            .is_none(),
        "an open session owes no delete"
    );
    close(&admin, &id, runner.as_ref()).await;
    let armed = ledger
        .delete_obligation(&id)
        .await
        .expect("read the delete obligation")
        .expect("the acknowledgement armed the delete");
    assert_eq!(armed.state, ObligationState::Due);
    assert_eq!(
        stores
            .obligation_ledger(ObligationKind::SessionDelete)
            .state(&armed.id)
            .await
            .expect("read the obligation state"),
        Some(ObligationState::Due)
    );
    close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        ledger
            .delete_obligation(&id)
            .await
            .expect("read the delete obligation"),
        Some(armed),
        "a repeated close keeps the one obligation"
    );
}

/// L-D8: a session's delete waits on exactly its own undelivered cleanup —
/// scope-close obligations on its roots, parent-end obligations on the plans
/// of its own scope, its turns' and its queue drains' — whether due, claimed
/// or stalled, and on nothing another session owes, even one whose id
/// extends its own.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_counts_only_the_sessions_undelivered_cleanup(
    prefix: &str,
    _host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    _runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let ledger = stores.session_delete_ledger();
    let registry = stores.process_registry();
    let now = stores.clock().timestamp_ms();
    let (id, store) = session(&stores, prefix, "delete-cleanup").await;
    let (other, other_store) = session(&stores, prefix, "delete-cleanup:x").await;
    let root = TurnId::from("cleanup-root");
    store
        .bind_root_inputs(&id, &root, &[])
        .await
        .expect("record the session's root");
    other_store
        .bind_root_inputs(&other, &root, &[])
        .await
        .expect("record the other session's root");
    assert_eq!(
        ledger
            .undelivered_cleanup(&id)
            .await
            .expect("read the cleanup"),
        SessionCleanup::default(),
        "rows that owe nothing are not cleanup"
    );

    let scope_close = arm(
        &stores,
        ObligationKey::ScopeClose {
            session_id: id.clone(),
            root: root.clone(),
        },
        now,
    )
    .await;
    arm(
        &stores,
        ObligationKey::ScopeClose {
            session_id: other.clone(),
            root: root.clone(),
        },
        now,
    )
    .await;
    let own = [
        ScopeId::session(id.clone()),
        ScopeId::turn(id.clone(), root.clone()),
        ScopeId::queue_drain(id.clone(), "cleanup-drain"),
    ];
    let foreign = [
        ScopeId::session(other.clone()),
        ScopeId::turn(other.clone(), root.clone()),
        ScopeId::queue_drain(other.clone(), "cleanup-drain"),
    ];
    // Recording a plan arms its `ParentEnd` obligation in the same
    // transaction (ADR 0109 §3).
    let mut plans = Vec::new();
    for scope in own.iter().chain(foreign.iter()) {
        registry
            .record_parent_end(scope)
            .await
            .expect("record the scope's plan");
        let plan = registry
            .get_parent_end_plan(scope)
            .await
            .expect("read the scope's plan")
            .expect("the plan is recorded");
        assert_eq!(plan.obligation_state, Some(ObligationState::Due));
        plans.push(plan.obligation_id.expect("the record armed the plan"));
    }
    assert_eq!(
        ledger
            .undelivered_cleanup(&id)
            .await
            .expect("read the cleanup"),
        SessionCleanup {
            scope_close: 1,
            parent_end: 3,
        }
    );

    let plan_ledger = stores.obligation_ledger(ObligationKind::ParentEnd);
    settle_by_hand(
        plan_ledger.as_ref(),
        &plans[0],
        ObligationSettlement::Delivered,
        now,
    )
    .await;
    settle_by_hand(
        plan_ledger.as_ref(),
        &plans[1],
        ObligationSettlement::Stall {
            reason: StallReason::Refused,
            error: "a child refused its cancel".into(),
        },
        now,
    )
    .await;
    plan_ledger
        .claim(&plans[2], now, 60_000)
        .await
        .expect("claim the drain's plan")
        .expect("the drain's plan is due");
    assert_eq!(
        ledger
            .undelivered_cleanup(&id)
            .await
            .expect("read the cleanup"),
        SessionCleanup {
            scope_close: 1,
            parent_end: 2,
        },
        "a stalled or claimed plan is still owed; a delivered one is not"
    );
    settle_by_hand(
        stores
            .obligation_ledger(ObligationKind::ScopeClose)
            .as_ref(),
        &scope_close,
        ObligationSettlement::Delivered,
        now,
    )
    .await;
    assert_eq!(
        ledger
            .undelivered_cleanup(&id)
            .await
            .expect("read the cleanup")
            .scope_close,
        0
    );
    assert_eq!(
        ledger
            .undelivered_cleanup(&other)
            .await
            .expect("read the other session's cleanup"),
        SessionCleanup {
            scope_close: 1,
            parent_end: 3,
        },
        "the other session's cleanup is its own"
    );
}

/// L-D9: the physical delete is the `SessionDelete` obligation's delivery.
/// While the close's cleanup is undelivered it refuses retryably and the
/// session stays closed and stored; once the cleanup is delivered, the next
/// attempt after the backoff deletes the session, and the row it lived on
/// goes with it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_physical_delete_waits_for_cleanup_then_deletes_the_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let factory = stores.session_store_factory();
    let clock = stores.clock();
    let (id, store) = session(&stores, prefix, "delete-finalizer").await;
    let root = TurnId::from("finalizer-root");
    store
        .bind_root_inputs(&id, &root, &[])
        .await
        .expect("record the session's root");
    let scope_close = arm(
        &stores,
        ObligationKey::ScopeClose {
            session_id: id.clone(),
            root,
        },
        clock.timestamp_ms(),
    )
    .await;
    let admin = administration(host, &stores, CloseSink::new(Arc::clone(&factory), 0));
    close(&admin, &id, runner.as_ref()).await;
    let delete = stores
        .session_delete_ledger()
        .delete_obligation(&id)
        .await
        .expect("read the delete obligation")
        .expect("the acknowledgement armed the delete");

    let relay = SessionDeleteRelay::new(admin);
    let first = deliver_now(&relay, &delete.id, clock.as_ref())
        .await
        .expect("attempt the delete");
    assert!(
        matches!(first, RelayVerdict::Retried { .. }),
        "an undelivered scope close holds the delete: {first:?}"
    );
    assert!(
        !factory
            .session_was_deleted(&id)
            .await
            .expect("read the tombstone"),
        "nothing was deleted"
    );
    let page = NonZeroUsize::new(8).expect("non-zero page");
    let now = clock.timestamp_ms();
    let before_backoff = relay_due(&relay, &lash_core::testing::TestClock::new(now), page)
        .await
        .expect("relay pass");
    assert_eq!(before_backoff.claimed, 0, "the retry waits out its backoff");

    settle_by_hand(
        stores
            .obligation_ledger(ObligationKind::ScopeClose)
            .as_ref(),
        &scope_close,
        ObligationSettlement::Delivered,
        now,
    )
    .await;
    let pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(now + 2_000),
        page,
    )
    .await
    .expect("relay pass");
    assert_eq!(pass.claimed, 1, "{pass:?}");
    assert_eq!(
        pass.claim_lost, 1,
        "the physical delete removed the row its obligation lived on: {pass:?}"
    );
    assert!(
        factory
            .session_was_deleted(&id)
            .await
            .expect("read the tombstone")
    );
    assert_eq!(
        stores
            .session_delete_ledger()
            .delete_obligation(&id)
            .await
            .expect("read the delete obligation"),
        None
    );
}
