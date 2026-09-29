//! The session-ingress store laws (ADR 0101 §16) on PostgreSQL.

use std::sync::Arc;

use lash_core_execution::SessionCatalogStore as _;

use super::{reset, storage};

lash_conformance::session_ingress_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres session-ingress conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let runtime = Arc::new(storage.store());
    runtime
        .admit_session(&lash_conformance::session_ingress_session_request())
        .await
        .expect("admit the Postgres session-ingress session");
    let ingress = Arc::clone(&runtime);
    (
        database_lock,
        lash_conformance::SessionIngressHandles { runtime, ingress },
    )
});
