//! The session-ingress store laws (ADR 0101 §16) on SQLite.

use lash_core::store::{ObligationKind, TurnInputAdmission};
use lash_core::{SessionCatalogStore as _, SessionCommitStore as _, TurnInputStore as _};
use lash_core_execution::StoreSet as _;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::session_ingress_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let runtime = backend.store().await;
    runtime
        .admit_session(&lash_conformance::session_ingress_session_request())
        .await
        .expect("create the SQLite session-ingress session");
    let ingress = runtime.clone();
    (
        backend,
        lash_conformance::SessionIngressHandles { runtime, ingress },
    )
});

/// FIG-3975: a SQLite admission folds the producer's whole store round into
/// its own commit — the version gate, the enqueue, the claim of each
/// admitted row's still-due ingress obligation, and the head read — and
/// answers `Fused`. The claims it took are the ones the obligation rows
/// still carry: a relay-side claim right after finds nothing due.
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
        .admit_pending_turn_inputs(batch, 60_000)
        .await
        .expect("admit the batch");
    let TurnInputAdmission::Fused {
        rows,
        ingress_claims,
        committed_head,
    } = admission
    else {
        panic!("the SQLite backend must fold the admission into one commit");
    };
    assert_eq!(rows.len(), 2, "the batch admitted both inputs");
    assert_eq!(
        ingress_claims.len(),
        2,
        "each admitted row's still-due obligation was claimed in the commit"
    );
    assert_ne!(
        ingress_claims[0].id, ingress_claims[1].id,
        "each row carries its own obligation"
    );

    // The claims the commit took still hold: a second claimant finds the
    // obligations claimed, not due.
    let ledger = backend.obligation_ledger(ObligationKind::Ingress);
    for claimed in &ingress_claims {
        assert!(
            ledger
                .claim(&claimed.id, 0, 60_000)
                .await
                .expect("reclaim the obligation")
                .is_none(),
            "the admission's claim must still hold"
        );
    }

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
