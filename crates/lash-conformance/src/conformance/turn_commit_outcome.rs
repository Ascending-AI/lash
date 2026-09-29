//! Durable terminal classifications shared by both SQL stores.

use crate::store::ConformanceDeployment;
use crate::{RuntimeCommit, RuntimeSessionState};
use lash_core::store::{
    CommitBudget, FleetFormat, OperationId, TurnCommitFailureCause, TurnCommitOutcome,
    WindowSelector,
};
use lash_sansio::{SessionId, TurnId};
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "conformance fixture stops on failed setup"
)]
async fn law(store: Arc<dyn ConformanceDeployment>, expected: TurnCommitOutcome) {
    let session_id = SessionId::from(format!("outcome-{}", expected.as_str()));
    let request = crate::testing::store_fixtures::session_store_request(
        &session_id,
        "outcome-model",
        crate::SessionRelation::Root,
    );
    store.admit_session(&request).await.expect("admit session");
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(request.config.session_policy())
    };
    let turn_id = TurnId::from("outcome-turn");
    let operation = OperationId::turn(&session_id, turn_id.as_str(), "final");
    let (mut commit, _) =
        RuntimeCommit::persisted_state_with_operation_and_staged_usage_and_budget(
            &mut state,
            &[],
            operation,
            CommitBudget::bounded(1024 * 1024, 512),
            FleetFormat::current(),
        )
        .expect("build turn commit");
    commit.outcome = Some(expected.clone());
    let first = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit turn");
    assert_eq!(first.outcome, Some(expected.clone()));
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("replay turn");
    assert_eq!(replay.outcome, Some(expected));
    assert_eq!(replay.head_revision, first.head_revision);
    assert!(
        store
            .committed_turn_exists(&session_id, &turn_id)
            .await
            .expect("read committed turn identity")
    );
    let read = store
        .load_session_window(&session_id, WindowSelector::Current)
        .await
        .expect("read committed session")
        .expect("session has a head");
    assert_eq!(read.head_revision, first.head_revision);
}

pub async fn completed(store: Arc<dyn ConformanceDeployment>) {
    law(store, TurnCommitOutcome::Completed).await;
}

pub async fn frame_switch(store: Arc<dyn ConformanceDeployment>) {
    law(store, TurnCommitOutcome::FrameSwitch).await;
}

pub async fn cancelled(store: Arc<dyn ConformanceDeployment>) {
    law(store, TurnCommitOutcome::Cancelled).await;
}

pub async fn failed(store: Arc<dyn ConformanceDeployment>) {
    law(
        store,
        TurnCommitOutcome::Failed(TurnCommitFailureCause::ProviderError),
    )
    .await;
}
