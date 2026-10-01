//! The session-ingress store laws (ADR 0101 §16) on PostgreSQL.

use std::sync::Arc;

use lash_core_execution::SessionCatalogStore as _;

use super::{reset, storage};

lash_conformance::session_ingress_tests!({
    let Some((database_fixture, storage)) = storage().await else {
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
    let pool = storage.pool().clone();
    let admission_snapshot: lash_conformance::IngressAdmissionProbe = Arc::new(move || {
        let pool = pool.clone();
        Box::pin(async move {
            let (inputs, batches, run_specs, obligations, sequence): (i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM lash_pending_turn_inputs),
                        (SELECT count(*) FROM lash_queued_work_batches),
                        (SELECT count(*) FROM lash_session_run_specs),
                        (SELECT count(*) FROM lash_pending_turn_inputs WHERE obligation_id IS NOT NULL)
                          + (SELECT count(*) FROM lash_queued_work_batches WHERE obligation_id IS NOT NULL),
                        (SELECT coalesce(max(enqueue_seq), 0) FROM lash_session_ingress_sequence)",
            ).fetch_one(&pool).await.expect("observe ingress allocations");
            lash_conformance::IngressAdmissionSnapshot {
                inputs,
                batches,
                run_specs,
                obligations,
                sequence,
            }
        })
    });
    (
        database_fixture,
        lash_conformance::SessionIngressHandles {
            runtime,
            ingress,
            admission_snapshot,
        },
    )
});
