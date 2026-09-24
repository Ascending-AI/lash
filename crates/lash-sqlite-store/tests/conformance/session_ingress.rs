//! The session-ingress store laws (ADR 0101 §16) on SQLite.

use lash_core::SessionStoreFactory as _;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::session_ingress_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let runtime = backend
        .session_store_factory()
        .create_store(&lash_conformance::session_ingress_session_request())
        .await
        .expect("create the SQLite session-ingress session");
    let ingress = backend.store().await;
    (
        backend,
        lash_conformance::SessionIngressHandles { runtime, ingress },
    )
});
