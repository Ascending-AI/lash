//! The direct-turn acceptance laws (ADR 0069) under the Restate effect
//! engine's in-process face. The laws drive `stream_turn` themselves, which a
//! handler-bound controller cannot serve from a test task, so the fixture's
//! host is the Restate controller over its recording context — the same
//! journaled-run and scope-fence path a handler execution takes — over a
//! SQLite memory store set.

use std::sync::Arc;

use super::*;

lash_conformance::direct_turn_acceptance_tests!({
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the direct-turn acceptance store set"),
    );
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(
        Arc::new(RecordingContext::default()),
    ));
    let backend = lash_conformance::backend_over(Arc::clone(&stores), host);
    let store = stores
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the direct-turn acceptance session store");
    (stores, "restate-double", backend, store)
});
