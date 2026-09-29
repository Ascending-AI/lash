//! A session's two-phase delete (ADR 0109 §4) compared across the three
//! backends: the close's acknowledgement arms the `SessionDelete` obligation
//! on the session's catalog row, and the session-delete ledger counts exactly
//! the session's undelivered cleanup — scope closes on its roots and
//! parent-end plans of the scopes it owns — and nothing another session owes;
//! and a closed session counts as closing until its physical delete.
//!
//! The Postgres database is shared with every other case and every earlier
//! run, so each backend's sessions carry the run nonce; ids are compared by
//! presence and state, never by value.

use lash_core::store::session_delete::SessionCleanup;
use lash_core::store::{IntentApplication, ObligationKey, ObligationKind, ObligationSettlement};
use lash_core::{ScopeId, StoreSet, TurnId};

use super::*;

const T0: u64 = 2_000_000;

type Transcript = Vec<String>;

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open or answer panics the harness with its name by design"
)]
async fn session_delete_transcript(stores: &dyn StoreSet, prefix: &str) -> Transcript {
    let mut out = Transcript::new();
    let factory = stores.session_store_factory();
    let registry = stores.process_registry();
    let ledger = stores.session_delete_ledger();
    let root = TurnId::from("delete-root");
    let mut sessions = Vec::new();
    for alias in ["own", "own:x"] {
        let session_id = SessionId::from(format!("{prefix}-delete-{alias}"));
        let store = admit_test_session(
            factory.clone(),
            &SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            },
        )
        .await
        .expect("create the session");
        store
            .bind_root_inputs(&session_id, &root, &[])
            .await
            .expect("record the session's root");
        sessions.push(session_id);
    }
    let (own, other) = (&sessions[0], &sessions[1]);

    let before = ledger
        .delete_obligation(own)
        .await
        .expect("read the delete obligation");
    out.push(format!(
        "delete before close -> {:?}",
        before.map(|o| o.state)
    ));
    // The Postgres database is shared, so the closing count is read as a
    // difference across this case's own close.
    let closing_before = ledger.count_closing().await.expect("count closing");
    let intent = factory
        .begin_session_close(own, T0)
        .await
        .expect("begin the close")
        .expect("the session exists");
    out.push(format!(
        "a close counts its session closing -> {}",
        ledger.count_closing().await.expect("count closing") - closing_before
    ));
    let pending = ledger
        .delete_obligation(own)
        .await
        .expect("read the delete obligation");
    out.push(format!(
        "delete after an unacknowledged close -> {:?}",
        pending.map(|o| o.state)
    ));
    let application = factory
        .claim_intent_application(intent.id, T0 + 1)
        .await
        .expect("claim the close's application");
    out.push(format!(
        "close application applies -> {}",
        matches!(application, IntentApplication::Apply(_))
    ));
    // The acknowledgement is claim-fenced: the relay's claim on the close's
    // `ControlIntent` obligation.
    let claim = stores
        .obligation_ledger(ObligationKind::ControlIntent)
        .claim(
            intent
                .obligation
                .as_ref()
                .expect("the close armed its obligation"),
            T0 + 1,
            60_000,
        )
        .await
        .expect("claim the close's obligation")
        .expect("the close's obligation is due")
        .token;
    factory
        .acknowledge_intent(intent.id, &claim, T0 + 2)
        .await
        .expect("acknowledge the close");
    let armed = ledger
        .delete_obligation(own)
        .await
        .expect("read the delete obligation")
        .expect("the acknowledgement armed the delete");
    out.push(format!(
        "delete after the acknowledgement -> {:?}",
        armed.state
    ));
    factory
        .acknowledge_intent(intent.id, &claim, T0 + 3)
        .await
        .expect("acknowledge again");
    let kept = ledger
        .delete_obligation(own)
        .await
        .expect("read the delete obligation")
        .expect("still armed");
    out.push(format!(
        "a repeated acknowledgement keeps the obligation -> {}",
        kept == armed
    ));
    out.push(format!(
        "an acknowledged close keeps its session closing -> {}",
        ledger.count_closing().await.expect("count closing") - closing_before
    ));

    let cleanup = |label: &'static str, cleanup: SessionCleanup| {
        format!(
            "{label} -> scope_close={} parent_end={}",
            cleanup.scope_close, cleanup.parent_end
        )
    };
    out.push(cleanup(
        "cleanup of an unarmed session",
        ledger.undelivered_cleanup(own).await.expect("cleanup"),
    ));
    // The close ended `own`'s open root, and that terminal write armed its
    // scope close (ADR 0109 §3); the other session's root is still open, so
    // the ledger's repair arm arms it.
    let mut scope_closes = Vec::new();
    for session in [own, other] {
        let armed = stores
            .obligation_ledger(ObligationKind::ScopeClose)
            .arm(
                &ObligationKey::ScopeClose {
                    session_id: session.clone(),
                    root: root.clone(),
                },
                T0,
            )
            .await
            .expect("arm the scope close");
        out.push(format!(
            "the repair arm arms the root of {} -> {}",
            if session == own { "own" } else { "other" },
            armed.is_some()
        ));
        scope_closes.push(
            armed.unwrap_or_else(|| lash_core::store::scope_close_obligation_id(session, &root)),
        );
    }
    let plans = [
        ScopeId::session(own.clone()),
        ScopeId::turn(own.clone(), root.clone()),
        ScopeId::queue_drain(own.clone(), "delete-drain"),
        ScopeId::session(other.clone()),
        ScopeId::turn(other.clone(), root.clone()),
    ];
    let mut plan_ids = Vec::new();
    for scope in &plans {
        registry
            .record_parent_end(scope)
            .await
            .expect("record the plan");
        // The record armed the plan's `ParentEnd` obligation (ADR 0109 §3).
        let plan = registry
            .get_parent_end_plan(scope)
            .await
            .expect("read the plan")
            .expect("the plan is recorded");
        out.push(format!(
            "recording a plan arms its obligation -> {:?}",
            plan.obligation_state
        ));
        plan_ids.push(plan.obligation_id.expect("the record armed the plan"));
    }
    out.push(cleanup(
        "cleanup with every obligation due",
        ledger.undelivered_cleanup(own).await.expect("cleanup"),
    ));
    let plan_ledger = stores.obligation_ledger(ObligationKind::ParentEnd);
    let claimed = plan_ledger
        .claim(&plan_ids[0], T0, 60_000)
        .await
        .expect("claim")
        .expect("due");
    plan_ledger
        .settle(
            &plan_ids[0],
            &claimed.token,
            ObligationSettlement::Delivered,
            T0,
        )
        .await
        .expect("deliver the session scope's plan");
    let scope_ledger = stores.obligation_ledger(ObligationKind::ScopeClose);
    let claimed = scope_ledger
        .claim(&scope_closes[0], T0, 60_000)
        .await
        .expect("claim")
        .expect("due");
    scope_ledger
        .settle(
            &scope_closes[0],
            &claimed.token,
            ObligationSettlement::Delivered,
            T0,
        )
        .await
        .expect("deliver the root's scope close");
    out.push(cleanup(
        "cleanup after two deliveries",
        ledger.undelivered_cleanup(own).await.expect("cleanup"),
    ));
    out.push(cleanup(
        "the other session's cleanup",
        ledger.undelivered_cleanup(other).await.expect("cleanup"),
    ));
    out
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open panics the harness with its name by design"
)]
pub(super) async fn compare_session_deletes(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    nonce: &str,
) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the SQLite memory session-delete store set");
    let file = lash_sqlite_store::SqliteStoreSet::open(sqlite_root.join("session-deletes"))
        .await
        .expect("open the SQLite file session-delete store set");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let postgres_stores = lash_postgres_store::PostgresStoreSet::new(
        postgres,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    );
    let backends: [(&str, &dyn StoreSet); 3] = [
        ("sqlite-memory", &memory),
        ("sqlite", &file),
        ("postgres", &postgres_stores),
    ];
    let mut observations = Vec::new();
    for (name, stores) in backends {
        let prefix = format!("fig-3855-{nonce}-{name}");
        observations.push((name, session_delete_transcript(stores, &prefix).await));
    }
    for pair in observations.windows(2) {
        let ((left, left_transcript), (right, right_transcript)) = (&pair[0], &pair[1]);
        assert_eq!(
            left_transcript, right_transcript,
            "session-delete answers differ between {left} and {right}"
        );
    }
    assert_eq!(
        observations[0].1.last().map(String::as_str),
        Some("the other session's cleanup -> scope_close=1 parent_end=2"),
        "{:#?}",
        observations[0].1
    );
    eprintln!(
        "PASS session_delete_ledger: backends=3 steps={}",
        observations[0].1.len()
    );
}
