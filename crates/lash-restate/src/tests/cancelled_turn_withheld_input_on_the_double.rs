//! The FIG-3531 cancelled-turn withheld-input law under the Restate effect
//! engine's in-process face. The law drives `stream_turn` itself, which a
//! handler-bound controller cannot serve from a test task, so the fixture is
//! the direct-turn one: the Restate controller over its recording context —
//! the same journaled-run and scope-fence path a handler execution takes —
//! over a SQLite memory store set.

use std::sync::Arc;

use super::*;

lash_conformance::cancelled_turn_withheld_input_tests!({
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the withheld-input store set"),
    );
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(
        Arc::new(RecordingContext::default()),
    ));
    let backend = lash_conformance::backend_over(Arc::clone(&stores), host);
    let store = stores
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(lash_conformance::CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the withheld-input session store");
    (stores, "restate-double", backend, store)
});
