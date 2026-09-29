//! Durable terminal classifications shared by both SQL stores.

use crate::{RuntimeCommit, RuntimeSessionState, SessionRelation, SessionStoreFactory};
use lash_core::store::{
    CommitBudget, FleetFormat, OperationId, TurnCommitFailureCause, TurnCommitOutcome,
};
use lash_sansio::SessionId;
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "conformance fixture must stop on failed setup"
)]
async fn law(factory: Arc<dyn SessionStoreFactory>, expected: TurnCommitOutcome) {
    let session_id = SessionId::from(format!("outcome-{}", expected.as_str()));
    let request = lash_core::testing::store_fixtures::session_store_request(
        &session_id,
        "outcome-model",
        SessionRelation::Root,
    );
    let writer = factory.create_store(&request).await.expect("create store");
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(request.policy)
    };
    let operation = OperationId::turn(session_id.as_str(), "outcome-turn", "final");
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
    let first = writer
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit turn");
    assert_eq!(first.outcome, Some(expected.clone()));
    let replay = writer
        .commit_runtime_state(commit)
        .await
        .expect("replay turn");
    assert_eq!(replay.outcome, Some(expected.clone()));
    let view = factory
        .read_session(&session_id)
        .await
        .expect("read committed session")
        .expect("session exists");
    assert_eq!(view.turn_commits().len(), 1);
    assert_eq!(view.turn_commits()[0].outcome, expected);
}

pub async fn completed(factory: Arc<dyn SessionStoreFactory>) {
    law(factory, TurnCommitOutcome::Completed).await;
}

pub async fn frame_switch(factory: Arc<dyn SessionStoreFactory>) {
    law(factory, TurnCommitOutcome::FrameSwitch).await;
}

pub async fn cancelled(factory: Arc<dyn SessionStoreFactory>) {
    law(factory, TurnCommitOutcome::Cancelled).await;
}

pub async fn failed(factory: Arc<dyn SessionStoreFactory>) {
    law(
        factory,
        TurnCommitOutcome::Failed(TurnCommitFailureCause::ProviderError),
    )
    .await;
}
