//! The session-ingress store laws (ADR 0101 §16) on PostgreSQL.

use std::sync::Arc;

use lash_core_execution::SessionStoreFactory as _;

use super::{reset, storage};

lash_conformance::session_ingress_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres session-ingress conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let runtime = storage
        .session_store_factory()
        .create_store(&lash_conformance::session_ingress_session_request())
        .await
        .expect("create the Postgres session-ingress session");
    let ingress = Arc::new(storage.session_store(lash_conformance::SESSION_INGRESS_SESSION_ID));
    (
        database_lock,
        lash_conformance::SessionIngressHandles { runtime, ingress },
    )
});
