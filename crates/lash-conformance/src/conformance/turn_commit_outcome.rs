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
    let session_id = SessionId::fixture(format!("outcome-{}", expected.as_str()));
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
    let operation = OperationId::turn(&session_id, turn_id.clone(), "final");
    let (mut commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
        &mut state,
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

/// An unattended terminal must remain readable until the projector acknowledges it.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture stops on failed setup"
)]
pub async fn unread_terminals_survive_retention(store: Arc<dyn ConformanceDeployment>) {
    let session_id = SessionId::fixture("unread-turn-terminal");
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
    let (mut commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
        &mut state,
        OperationId::turn(&session_id, TurnId::from("unattended"), "final"),
        CommitBudget::bounded(1024 * 1024, 512),
        FleetFormat::current(),
    )
    .expect("build turn commit");
    commit.outcome = Some(TurnCommitOutcome::Failed(
        TurnCommitFailureCause::ProviderError,
    ));
    store
        .commit_runtime_state(commit)
        .await
        .expect("unattended failure");
    store
        .delete_session(&session_id)
        .await
        .expect("delete session");
    let report = store
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: lash_core::store::TurnProjectionWatermark::UpTo(
                lash_core::store::TurnChangeCursor::initial(),
            ),
        })
        .await
        .expect("reclaim before acknowledgement");
    assert_eq!(
        report.removed_receipt_count, 0,
        "prune dropped an unread typed terminal"
    );
    assert_eq!(report.removed_session_terminal_count, 0);
    let one = std::num::NonZeroUsize::new(1).expect("one");
    let initial = lash_core::store::TurnChangeCursor::initial();
    let first = store
        .turns_changed_since(initial, one)
        .await
        .expect("read missed failure");
    assert_eq!(first.changes.len(), 1);
    assert!(matches!(
        first.changes[0].kind,
        lash_core::store::TurnChangeKind::Committed {
            outcome: TurnCommitOutcome::Failed(TurnCommitFailureCause::ProviderError),
            ..
        }
    ));
    assert_eq!(first.changes[0].session_id, session_id);
    let delete = store
        .turns_changed_since(first.next, one)
        .await
        .expect("read session terminal");
    assert_eq!(delete.changes.len(), 1);
    assert_eq!(
        delete.changes[0].kind,
        lash_core::store::TurnChangeKind::SessionDeleted
    );
    let report = store
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: lash_core::store::TurnProjectionWatermark::UpTo(first.next),
        })
        .await
        .expect("acknowledged turn reclamation");
    assert_eq!(report.removed_receipt_count, 1);
    assert_eq!(report.removed_session_terminal_count, 0);
    assert!(matches!(store.turns_changed_since(initial, one).await,
        Err(crate::StoreError::TurnChangeCursorPruned { horizon }) if horizon == first.next));
    let still = store
        .turns_changed_since(first.next, one)
        .await
        .expect("unread deletion survives");
    assert_eq!(still.changes, delete.changes);
    assert_eq!(still.retained_after, first.next);
    let report = store
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: lash_core::store::TurnProjectionWatermark::UpTo(delete.next),
        })
        .await
        .expect("acknowledged deletion reclamation");
    assert_eq!(report.removed_session_terminal_count, 1);
    let end = store
        .turns_changed_since(delete.next, one)
        .await
        .expect("read at horizon");
    assert!(end.changes.is_empty());
    assert_eq!(end.next, delete.next);
    assert_eq!(end.retained_after, delete.next);
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture stops on failed setup"
)]
pub async fn terminal_feed_is_ordered_and_replay_stable(store: Arc<dyn ConformanceDeployment>) {
    use lash_core::store::{TurnChangeCursor, TurnChangeKind};
    let one = std::num::NonZeroUsize::new(1).expect("one");
    let initial = TurnChangeCursor::initial();
    assert!(
        store
            .turns_changed_since(initial, one)
            .await
            .expect("empty feed")
            .changes
            .is_empty()
    );
    let mut expected = Vec::new();
    for (i, outcome) in [
        TurnCommitOutcome::Completed,
        TurnCommitOutcome::Cancelled,
        TurnCommitOutcome::Failed(TurnCommitFailureCause::ContextOverflow),
    ]
    .into_iter()
    .enumerate()
    {
        let session = SessionId::fixture(format!("feed-{i}"));
        let request = crate::testing::store_fixtures::session_store_request(
            &session,
            "outcome-model",
            crate::SessionRelation::Root,
        );
        store.admit_session(&request).await.expect("admit session");
        let mut state = RuntimeSessionState {
            session_id: session.clone(),
            ..RuntimeSessionState::new(request.config.session_policy())
        };
        let (mut commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
            &mut state,
            OperationId::turn(&session, TurnId::from("feed-turn"), "final"),
            CommitBudget::bounded(1024 * 1024, 512),
            FleetFormat::current(),
        )
        .expect("build commit");
        commit.outcome = Some(outcome.clone());
        let operation = commit.turn_commit.operation.clone();
        store
            .commit_runtime_state(commit.clone())
            .await
            .expect("commit");
        let replay = store
            .commit_runtime_state(commit)
            .await
            .expect("receipt replay");
        assert!(replay.receipt_replayed);
        expected.push((session, TurnChangeKind::Committed { operation, outcome }));
    }
    let session = expected[0].0.clone();
    let record = |message: &str| {
        lash_core::store::SessionFaultRecord::new(
            lash_core::store::SessionFaultOrigin::DriveAdmission,
            &crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: message.to_owned(),
            }
            .runtime_error(),
        )
    };
    let first = record("first fault");
    let second = record("second fault");
    store
        .record_session_fault(&session, &first, 7)
        .await
        .expect("record first fault");
    store
        .record_session_fault(&session, &second, 9)
        .await
        .expect("first fault wins");
    store
        .clear_session_fault(&session)
        .await
        .expect("operator clears");
    store
        .record_session_fault(&session, &second, 11)
        .await
        .expect("new fault episode");
    expected.push((
        session.clone(),
        TurnChangeKind::SessionFault {
            record: Box::new(first),
        },
    ));
    expected.push((
        session,
        TurnChangeKind::SessionFault {
            record: Box::new(second),
        },
    ));
    let mut cursor = initial;
    for (session, kind) in expected {
        let page = store
            .turns_changed_since(cursor, one)
            .await
            .expect("bounded page");
        assert_eq!(page.changes.len(), 1);
        assert_eq!(page.changes[0].session_id, session);
        assert_eq!(page.changes[0].kind, kind);
        assert!(page.next.store_sequence() > cursor.store_sequence());
        assert_eq!(page.next, page.changes[0].cursor);
        cursor = page.next;
    }
    assert!(
        store
            .turns_changed_since(cursor, one)
            .await
            .expect("end")
            .changes
            .is_empty()
    );
    assert!(matches!(
        store
            .turns_changed_since(TurnChangeCursor::from_store_sequence(u64::MAX), one)
            .await,
        Err(crate::StoreError::TurnChangeCursorAhead { .. })
    ));
    assert!(
        store
            .reclaim_retained_evidence(crate::RetentionBound {
                committed_before_epoch_ms: u64::MAX,
                turn_watermark: lash_core::store::TurnProjectionWatermark::UpTo(
                    TurnChangeCursor::from_store_sequence(u64::MAX)
                ),
            })
            .await
            .is_err(),
        "a future acknowledgement must refuse"
    );
    assert_eq!(
        store
            .turns_changed_since(initial, std::num::NonZeroUsize::MAX)
            .await
            .expect("invalid prune left evidence")
            .changes
            .len(),
        5
    );
}
