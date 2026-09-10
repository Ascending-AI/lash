//! FIG-2502 / FIG-853: terminal-gated horizons and documented retry outcomes.
use super::session_store_factory::session_store_request;
use super::*;
use pretty_assertions::assert_eq;

/// Certify the explicit factory retention lever on one fresh backend.
pub async fn retention_conformance(factory: Arc<dyn crate::SessionStoreFactory>) {
    let request = session_store_request(
        "retention-terminal",
        "retention-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.unwrap();
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let usage = crate::TokenLedgerEntry {
        source: "retention".into(),
        model: "retention-model".into(),
        usage: crate::TokenUsage {
            input_tokens: 7,
            output_tokens: 3,
            ..Default::default()
        },
    };
    let commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[usage]);
    let receipt = store.commit_runtime_state(commit.clone()).await.unwrap();
    assert!(!receipt.receipt_replayed);
    let committed_at_ms = factory
        .list_sessions(&crate::SessionListFilter::default())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.session_id == request.session_id)
        .unwrap()
        .last_commit_at_ms
        .unwrap();
    let all = crate::RetentionBound {
        committed_before_epoch_ms: u64::MAX,
    };
    // FIG-853's regression: even the largest bound cannot erase a live receipt
    // and turn its retry into a terminal "someone else committed" conflict.
    assert_eq!(
        factory.reclaim_retained_evidence(all).await.unwrap(),
        crate::RetentionReport::default()
    );
    let replay = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("pre-terminal prune must never turn replay into conflict");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, receipt.head_revision);
    assert_eq!(
        store
            .load_session()
            .await
            .unwrap()
            .unwrap()
            .token_ledger
            .len(),
        1
    );

    // Another live scope is a negative control for the factory-wide sweep.
    let live_request = session_store_request(
        "retention-live",
        "retention-model",
        crate::SessionRelation::Root,
    );
    let live = factory.create_store(&live_request).await.unwrap();
    let mut live_state = crate::RuntimeSessionState {
        session_id: live_request.session_id.clone(),
        ..crate::RuntimeSessionState::new(live_request.policy.clone())
    };
    live_state.ensure_agent_frame_initialized();
    let live_commit = crate::RuntimeCommit::persisted_state_for_test(&live_state, &[]);
    live.commit_runtime_state(live_commit.clone())
        .await
        .unwrap();

    factory.delete_session(&request.session_id).await.unwrap();
    // Exclusive cutoff: the receipt exactly at the horizon is still retained.
    let at_horizon = crate::RetentionBound {
        committed_before_epoch_ms: committed_at_ms,
    };
    assert_eq!(
        factory.reclaim_retained_evidence(at_horizon).await.unwrap(),
        crate::RetentionReport::default()
    );
    assert!(
        matches!(
            store.commit_runtime_state(commit.clone()).await,
            Err(crate::StoreError::SessionDeleted { .. })
        ),
        "terminal retry already refuses before pruning"
    );
    let past_horizon = crate::RetentionBound {
        committed_before_epoch_ms: committed_at_ms + 1,
    };
    let report = factory
        .reclaim_retained_evidence(past_horizon)
        .await
        .unwrap();
    assert_eq!(report.removed_receipt_count, 1);
    assert_eq!(report.removed_usage_delta_count, 1);
    assert_eq!(report.removed_attachment_root_count, 0);
    assert_eq!(crate::MaintenanceReport::reclaimed_count(&report), 2);
    assert!(
        matches!(
            store.commit_runtime_state(commit).await,
            Err(crate::StoreError::SessionDeleted { .. })
        ),
        "post-horizon retry is SessionDeleted, never a commit conflict or a new run"
    );
    assert_eq!(
        factory.reclaim_retained_evidence(all).await.unwrap(),
        crate::RetentionReport::default()
    );
    assert!(
        factory
            .session_was_deleted(&request.session_id)
            .await
            .unwrap(),
        "FIG-754 / FIG-748: permanent identity evidence survives every bound"
    );
    assert!(
        live.commit_runtime_state(live_commit)
            .await
            .unwrap()
            .receipt_replayed,
        "a sibling live receipt survives global reclamation"
    );
}
