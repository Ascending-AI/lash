//! The fork observer-intent settlement law under the Restate engine's backend
//! shape: the law exercises the session catalog and process registry only, so
//! the fixture pairs a SQLite memory store set with the Restate controller's
//! in-process face, as the rest of this suite's storage-side laws do.

use std::sync::Arc;

use super::*;

lash_conformance::observer_intent_tests!({
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the observer-intent store set"),
    );
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(
        Arc::new(RecordingContext::default()),
    ));
    let backend = lash_conformance::backend_over(Arc::clone(&stores), host);
    (stores, backend)
});
