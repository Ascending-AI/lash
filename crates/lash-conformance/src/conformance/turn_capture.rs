//! Durable capture laws shared by SQLite and PostgreSQL.

use super::*;
use crate::store::{
    CaptureAttemptReset, CaptureBaseAdvance, CaptureBatch, CaptureFrame, CaptureInvocationKey,
    OpenCaptureWriter, SealTurnCapture, SealedCapture, StoppedPartialCommit, StoppedPartialRead,
    StoppedPartialReadRequest,
};
use lash_core::SessionStoreFactory;
use lash_core::facade_support::TurnAddress;
use lash_sansio::llm::types::StreamBlockIdentity;
use lash_sansio::{CaptureBase, PartialItem, SessionId, StopReason};

async fn fixture(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) -> (Arc<dyn RuntimePersistence>, TurnAddress) {
    let session = SessionId::from(format!("capture-{label}"));
    let request = lash_core::testing::store_fixtures::session_store_request(
        &session,
        "capture-model",
        SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create capture session");
    (store, TurnAddress::new(session, format!("turn-{label}")))
}

async fn writer(
    store: &dyn RuntimePersistence,
    turn: &TurnAddress,
) -> crate::store::CaptureWriterLease {
    store
        .open_capture_writer(&OpenCaptureWriter {
            turn: turn.clone(),
            root: turn.turn_id.clone(),
            invocation: CaptureInvocationKey("llm".into()),
        })
        .await
        .expect("open writer")
}

fn text_frames() -> Vec<CaptureFrame> {
    let block = StreamBlockIdentity::new("message", 0);
    vec![
        CaptureFrame::TextStart {
            block: block.clone(),
        },
        CaptureFrame::TextDelta {
            block: block.clone(),
            text: "partial".into(),
        },
        CaptureFrame::TextEnd {
            block,
            text: "partial".into(),
        },
    ]
}

async fn append(
    store: &dyn RuntimePersistence,
    writer: &crate::store::CaptureWriterLease,
    ordinal: u64,
    frames: Vec<CaptureFrame>,
) -> crate::store::CaptureAck {
    store
        .append_capture_batch(&CaptureBatch {
            lease: writer.lease_ref(),
            batch_ordinal: ordinal,
            frames,
        })
        .await
        .expect("append capture")
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_batch_replay_and_conflict(factory: Arc<dyn SessionStoreFactory>, label: &str) {
    let (store, turn) = fixture(factory, label).await;
    let lease = writer(store.as_ref(), &turn).await;
    let batch = CaptureBatch {
        lease: lease.lease_ref(),
        batch_ordinal: 0,
        frames: text_frames(),
    };
    let first = store
        .append_capture_batch(&batch)
        .await
        .expect("first append");
    assert_eq!((first.first_sequence, first.last_sequence), (1, 3));
    assert_eq!(
        store.append_capture_batch(&batch).await.expect("retry"),
        first
    );
    let conflict = CaptureBatch {
        frames: vec![CaptureFrame::TextStart {
            block: StreamBlockIdentity::new("different", 0),
        }],
        ..batch
    };
    assert!(matches!(
        store.append_capture_batch(&conflict).await,
        Err(StoreError::CaptureBatchConflict { .. })
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_reset_fences_old_epoch(factory: Arc<dyn SessionStoreFactory>, label: &str) {
    let (store, turn) = fixture(factory, label).await;
    let first = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &first, 0, text_frames()).await;
    let next = store
        .persist_attempt_reset(&CaptureAttemptReset {
            lease: first.lease_ref(),
        })
        .await
        .expect("reset");
    assert_eq!(next.attempt_epoch, first.attempt_epoch + 1);
    assert!(next.inherited.is_empty());
    assert_eq!(
        store
            .persist_attempt_reset(&CaptureAttemptReset {
                lease: first.lease_ref()
            })
            .await
            .expect("reset replay")
            .attempt_epoch,
        next.attempt_epoch
    );
    assert!(matches!(
        store
            .append_capture_batch(&CaptureBatch {
                lease: first.lease_ref(),
                batch_ordinal: 1,
                frames: text_frames()
            })
            .await,
        Err(StoreError::CaptureWriterFenced { .. })
    ));
    append(store.as_ref(), &next, 0, text_frames()).await;
    let sealed = store
        .seal_turn_capture(&SealTurnCapture {
            turn,
            root: first.turn.turn_id,
            reason: StopReason::UserCancel,
            recorded_watermark: None,
        })
        .await
        .expect("seal");
    let partial = sealed.partial();
    assert_eq!(partial.items.len(), 1);
    assert!(matches!(&partial.items[0], PartialItem::Text { text, .. } if text == "partial"));
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_successor_resets_inherited_epoch(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) {
    let (store, turn) = fixture(factory, label).await;
    let first = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &first, 0, text_frames()).await;
    let successor = writer(store.as_ref(), &turn).await;
    assert_eq!(successor.attempt_epoch, first.attempt_epoch + 1);
    assert_eq!(successor.inherited.len(), 3);
    let reset = CaptureAttemptReset {
        lease: first.lease_ref(),
    };
    let resumed = store
        .persist_attempt_reset(&reset)
        .await
        .expect("reset inherited epoch");
    assert_eq!(resumed.attempt_epoch, successor.attempt_epoch);
    assert!(resumed.inherited.is_empty());
    assert_eq!(
        store
            .persist_attempt_reset(&reset)
            .await
            .expect("reset replay"),
        resumed
    );
    let mut stale = first.lease_ref();
    stale.attempt_epoch = successor.attempt_epoch + 1;
    assert!(matches!(
        store
            .persist_attempt_reset(&CaptureAttemptReset { lease: stale })
            .await,
        Err(StoreError::CaptureWriterFenced { .. })
    ));
    append(store.as_ref(), &resumed, 0, text_frames()).await;
    let partial = store
        .seal_turn_capture(&SealTurnCapture {
            turn: turn.clone(),
            root: turn.turn_id,
            reason: StopReason::UserCancel,
            recorded_watermark: None,
        })
        .await
        .expect("seal")
        .into_partial();
    assert_eq!(partial.items.len(), 1);
    assert!(matches!(&partial.items[0], PartialItem::Text { text, .. } if text == "partial"));
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_base_advance_removes_old_tail(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) {
    let (store, turn) = fixture(factory, label).await;
    let first = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &first, 0, text_frames()).await;
    let advance = CaptureBaseAdvance {
        turn: turn.clone(),
        to: CaptureBase(1),
    };
    store.advance_capture_base(&advance).await.expect("advance");
    store
        .advance_capture_base(&advance)
        .await
        .expect("advance replay");
    let stale = CaptureBaseAdvance {
        turn: turn.clone(),
        to: CaptureBase(3),
    };
    assert!(matches!(
        store.advance_capture_base(&stale).await,
        Err(StoreError::CaptureBaseStale { .. })
    ));
    let next = writer(store.as_ref(), &turn).await;
    assert_eq!(next.base, CaptureBase(1));
    assert!(next.inherited.is_empty());
    append(store.as_ref(), &next, 0, text_frames()).await;
    let partial = store
        .seal_turn_capture(&SealTurnCapture {
            turn: turn.clone(),
            root: turn.turn_id.clone(),
            reason: StopReason::UserCancel,
            recorded_watermark: None,
        })
        .await
        .expect("seal")
        .into_partial();
    assert_eq!(partial.id.base, CaptureBase(1));
    assert_eq!(partial.items.len(), 1);
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_seal_is_first_writer_wins(factory: Arc<dyn SessionStoreFactory>, label: &str) {
    let (store, turn) = fixture(factory, label).await;
    let lease = writer(store.as_ref(), &turn).await;
    let ack = append(store.as_ref(), &lease, 0, text_frames()).await;
    let request = SealTurnCapture {
        turn: turn.clone(),
        root: turn.turn_id.clone(),
        reason: StopReason::UserCancel,
        recorded_watermark: Some(ack.last_sequence),
    };
    let sealed = store
        .seal_turn_capture(&request)
        .await
        .expect("seal")
        .into_partial();
    assert_eq!(sealed.id.sealed_through, ack.last_sequence);
    assert_eq!(
        store
            .seal_turn_capture(&SealTurnCapture {
                recorded_watermark: Some(u64::MAX),
                ..request
            })
            .await
            .expect("reseal")
            .into_partial(),
        sealed
    );
    assert!(matches!(
        store
            .append_capture_batch(&CaptureBatch {
                lease: lease.lease_ref(),
                batch_ordinal: 1,
                frames: text_frames()
            })
            .await,
        Err(StoreError::CaptureSealed { .. })
    ));
    assert_eq!(
        store
            .read_stopped_partial(&StoppedPartialReadRequest {
                session_id: turn.session_id,
                turn: turn.turn_id
            })
            .await
            .expect("read"),
        StoppedPartialRead::Pending
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_commit_publishes_exact_partial(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) {
    let (store, turn) = fixture(factory, label).await;
    let lease = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &lease, 0, text_frames()).await;
    let partial = store
        .seal_turn_capture(&SealTurnCapture {
            turn: turn.clone(),
            root: turn.turn_id.clone(),
            reason: StopReason::UserCancel,
            recorded_watermark: None,
        })
        .await
        .expect("seal")
        .into_partial();
    let request = lash_core::testing::store_fixtures::session_store_request(
        &turn.session_id,
        "capture-model",
        SessionRelation::Root,
    );
    let mut state = RuntimeSessionState {
        session_id: turn.session_id.clone(),
        ..RuntimeSessionState::new(request.policy)
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::store::OperationId::new(
        lash_core::ExecutionScope::turn(turn.session_id.clone(), turn.turn_id.clone()),
        "commit",
    );
    let mut commit =
        crate::RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    commit.stopped_partial = Some(StoppedPartialCommit::of(&partial));
    store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit");
    let read = store
        .read_stopped_partial(&StoppedPartialReadRequest {
            session_id: turn.session_id.clone(),
            turn: turn.turn_id.clone(),
        })
        .await
        .expect("read");
    assert_eq!(read, StoppedPartialRead::Available(partial.clone()));
    assert_eq!(
        store
            .seal_turn_capture(&SealTurnCapture {
                turn,
                root: partial.id.root.clone(),
                reason: StopReason::ProcessLoss,
                recorded_watermark: None
            })
            .await
            .expect("seal replay"),
        SealedCapture::Committed(partial)
    );
    assert!(
        store
            .commit_runtime_state(commit)
            .await
            .expect("commit replay")
            .receipt_replayed
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_retention_waits_for_deletion(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) {
    let (store, turn) = fixture(Arc::clone(&factory), label).await;
    let lease = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &lease, 0, text_frames()).await;
    let partial = store
        .seal_turn_capture(&SealTurnCapture {
            turn: turn.clone(),
            root: turn.turn_id.clone(),
            reason: StopReason::UserCancel,
            recorded_watermark: None,
        })
        .await
        .expect("seal")
        .into_partial();
    let request = lash_core::testing::store_fixtures::session_store_request(
        &turn.session_id,
        "capture-model",
        SessionRelation::Root,
    );
    let mut state = RuntimeSessionState {
        session_id: turn.session_id.clone(),
        ..RuntimeSessionState::new(request.policy)
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::store::OperationId::new(
        lash_core::ExecutionScope::turn(turn.session_id.clone(), turn.turn_id.clone()),
        "commit",
    );
    let mut commit =
        crate::RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    commit.stopped_partial = Some(StoppedPartialCommit::of(&partial));
    store.commit_runtime_state(commit).await.expect("commit");

    let all = crate::RetentionBound {
        committed_before_epoch_ms: u64::MAX,
    };
    assert_eq!(
        factory
            .reclaim_retained_evidence(all)
            .await
            .expect("live sweep")
            .removed_stopped_partial_count,
        0
    );
    factory
        .delete_session(&turn.session_id)
        .await
        .expect("delete session");
    assert!(matches!(
        store
            .read_stopped_partial(&StoppedPartialReadRequest {
                session_id: turn.session_id.clone(),
                turn: turn.turn_id,
            })
            .await,
        Err(StoreError::SessionDeleted { .. })
    ));
    let report = factory
        .reclaim_retained_evidence(all)
        .await
        .expect("deleted sweep");
    assert_eq!(report.removed_stopped_partial_count, 1);
    assert_eq!(
        factory
            .reclaim_retained_evidence(all)
            .await
            .expect("sweep replay")
            .removed_stopped_partial_count,
        0
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance assertions require fixture setup"
)]
pub async fn capture_deletion_reclaims_staging_frames(
    factory: Arc<dyn SessionStoreFactory>,
    label: &str,
) {
    let (store, turn) = fixture(Arc::clone(&factory), label).await;
    let lease = writer(store.as_ref(), &turn).await;
    append(store.as_ref(), &lease, 0, text_frames()).await;
    let report = factory
        .delete_session(&turn.session_id)
        .await
        .expect("delete session");
    assert_eq!(report.removed_capture_frame_count, 3);
    assert!(matches!(
        store
            .read_stopped_partial(&StoppedPartialReadRequest {
                session_id: turn.session_id,
                turn: turn.turn_id,
            })
            .await,
        Err(StoreError::SessionDeleted { .. })
    ));
}
