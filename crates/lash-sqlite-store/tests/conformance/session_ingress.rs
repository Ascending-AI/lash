//! The session-ingress store laws (ADR 0101 §16) on SQLite.

use lash_core::store::TurnInputAdmission;
use lash_core::{SessionCatalogStore as _, TurnInputStore as _};

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::session_ingress_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let runtime = backend.store().await;
    runtime
        .admit_session(&lash_conformance::session_ingress_session_request())
        .await
        .expect("create the SQLite session-ingress session");
    let observed = backend.clone();
    let admission_snapshot: lash_conformance::IngressAdmissionProbe =
        std::sync::Arc::new(move || {
            let backend = observed.clone();
            Box::pin(async move {
                backend
                    .raw()
                    .query_row(
                        "SELECT (SELECT count(*) FROM pending_turn_inputs),
                        (SELECT count(*) FROM queued_work_batches),
                        (SELECT count(*) FROM session_run_specs),
                        (SELECT coalesce(max(enqueue_seq), 0) FROM session_ingress_sequence)",
                        [],
                        |row| {
                            Ok(lash_conformance::IngressAdmissionSnapshot {
                                inputs: row.get(0)?,
                                batches: row.get(1)?,
                                run_specs: row.get(2)?,
                                sequence: row.get(3)?,
                            })
                        },
                    )
                    .expect("observe ingress allocations")
            })
        });
    (
        backend,
        lash_conformance::SessionIngressHandles {
            runtime,
            admission_snapshot,
        },
    )
});

/// FIG-3975: a SQLite admission folds the producer's whole store round into
/// its own commit — the version gate, the enqueue and the head read — and
/// answers `Fused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_folds_the_producer_round_into_one_commit() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let runtime = backend.store().await;
    runtime
        .admit_session(&lash_conformance::session_ingress_session_request())
        .await
        .expect("create the session");
    let session_id = lash_sansio::SessionId::from(lash_conformance::SESSION_INGRESS_SESSION_ID);
    let batch = lash_core::PendingTurnInputBatch::new(
        session_id.clone(),
        vec![
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("first fused input"),
            ),
            lash_core::PendingTurnInputDraft::new(
                session_id,
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("second fused input"),
            ),
        ],
    )
    .expect("build the batch");

    let admission = runtime
        .admit_pending_turn_inputs(batch)
        .await
        .expect("admit the batch");
    let TurnInputAdmission::Fused {
        rows,
        committed_head,
    } = admission
    else {
        panic!("the SQLite backend must fold the admission into one commit");
    };
    assert_eq!(rows.len(), 2, "the batch admitted both inputs");
    let reread = runtime
        .load_session_head_meta(&lash_sansio::SessionId::from(
            lash_conformance::SESSION_INGRESS_SESSION_ID,
        ))
        .await
        .expect("read the head");
    assert_eq!(
        committed_head
            .as_ref()
            .map(|head| (head.head_revision, head.checkpoint_ref.is_some())),
        reread
            .as_ref()
            .map(|head| (head.head_revision, head.checkpoint_ref.is_some())),
        "the committed-head read rides the same transaction"
    );
}
