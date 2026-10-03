//! One value of every [`StoreError`] variant, for the laws that must hold of
//! each of them.

use super::{AnchorUnavailable, StoreError, WindowAnchorViolation};
use crate::{BatchId, InputId, NodeId, SessionId, TurnId};

/// `store_error_samples!` names every `StoreError` variant with one value of
/// it. Its one input generates the list the laws iterate and an exhaustive
/// `match`, so a variant missing from it fails to compile and the iterated
/// set cannot outlive the enum.
macro_rules! store_error_samples {
    ($( $variant:ident $(($($tuple:tt)*))? $({$($fields:tt)*})? => $sample:expr, )*) => {
        impl StoreError {
            /// One value of every variant.
            pub fn samples_for_testing() -> Vec<Self> {
                let samples = vec![$( $sample, )*];
                for sample in &samples {
                    match sample {
                        $( Self::$variant $(($($tuple)*))? $({$($fields)*})? => {} )*
                    }
                }
                samples
            }
        }
    };
}

fn session() -> SessionId {
    SessionId::from("sampled-session")
}

fn turn() -> TurnId {
    TurnId::from("sampled-turn")
}

fn node() -> NodeId {
    NodeId::from("sampled-node")
}

fn referrer() -> crate::artifact_referrer::ArtifactReferrer {
    crate::artifact_referrer::ArtifactReferrer::Session(session())
}

fn attachment() -> crate::AttachmentId {
    crate::AttachmentId::parse("sampled-attachment").expect("a well-formed attachment id")
}

store_error_samples! {
    UnfinishedRunConflict { .. } => StoreError::UnfinishedRunConflict {
        session_id: session(),
        run: turn(),
    },
    RunHeldByAnotherExecutor { .. } => StoreError::RunHeldByAnotherExecutor {
        session_id: session(),
        run: turn(),
        recorded: Box::new(crate::store::RunExecutor::run(&crate::store::AdmissionId::new("fixture#0"))),
        admitting: Box::new(crate::store::RunExecutor::run(&crate::store::AdmissionId::new("fixture#0"))),
    },
    FollowOnPending { .. } => StoreError::FollowOnPending {
        session_id: session(),
        follow_on_turn_id: turn(),
        attempts: 1,
    },
    FollowOnFrameNotCurrent { .. } => StoreError::FollowOnFrameNotCurrent {
        session_id: session(),
        follow_on_frame_id: "frame".to_string(),
        current_frame_node_id: None,
    },
    FollowOnHeadInvariant { .. } => StoreError::FollowOnHeadInvariant {
        session_id: session(),
        reason: "sampled".to_string(),
    },
    FollowOnNotPending { .. } => StoreError::FollowOnNotPending {
        session_id: session(),
        follow_on_turn_id: turn(),
    },
    ExecutionStateCaptureFailed { .. } => StoreError::ExecutionStateCaptureFailed {
        message: "sampled".to_string(),
    },
    TurnOutcomeMaterializationRefused { .. } => StoreError::TurnOutcomeMaterializationRefused {
        error: Box::new(crate::RuntimeError::new(
            crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
            "sampled",
        )),
    },
    WaitReceiptConflict { .. } => StoreError::WaitReceiptConflict { wait_id: "wait".into() },
    ToolRequestConflict { .. } => StoreError::ToolRequestConflict { owner: lash_trace::TraceToolOwner::Turn { session_id: session(), turn_id: "turn".into() }, request_key: "tool-request".into() },
    PreparedProcessRegistrationStale { .. } => StoreError::PreparedProcessRegistrationStale { process_id: crate::process_id_for_test("prepared-process") },
    PreparedRunAdmissionStale { .. } => StoreError::PreparedRunAdmissionStale { session_id: session(), run: crate::TurnId::from("stale-run") },
    Contended => StoreError::Contended,
    CommitNodeBudgetExceeded { .. } => StoreError::CommitNodeBudgetExceeded {
        node_count: 2,
        max_nodes: 1,
    },
    CommitByteBudgetExceeded { .. } => StoreError::CommitByteBudgetExceeded {
        session_config_bytes: 0,
        graph_delta_bytes: 2,
        checkpoint_bytes: 0,
        attachment_referrer_bytes: 0,
        follow_on_bytes: 0,
        turn_result_bytes: 0,
        total_bytes: 2,
        max_bytes: 1,
    },
    QueuedWorkActionReserveExhaustsContext { .. } => {
        StoreError::QueuedWorkActionReserveExhaustsContext {
            max_context_tokens: 1,
            action_token_reserve: 2,
        }
    },
    QueuedWorkRowExceedsContextWindow { .. } => StoreError::QueuedWorkRowExceedsContextWindow {
        batch_id: BatchId::from("sampled-batch"),
        batch_enqueue_seq: 1,
        rendered_tokens: 2,
        max_context_tokens: 1,
    },
    SessionRelationMismatch { .. } => StoreError::SessionRelationMismatch {
        session_id: session(),
        recorded: Box::new(crate::SessionLineage::Root),
        requested: Box::new(crate::SessionLineage::Child {
            parent_session_id: SessionId::from("sampled-parent"),
        }),
    },
    SessionNotFound { .. } => StoreError::SessionNotFound { session_id: session() },
    ForeignSessionRequest { .. } => StoreError::ForeignSessionRequest {
        view_session_id: session(),
        request_session_id: SessionId::from("sampled-other"),
    },
    InvalidWindowAnchor { .. } => StoreError::InvalidWindowAnchor {
        frame_node_id: node(),
        violation: WindowAnchorViolation::BaseNotFrameOpen,
    },
    HistoryAnchorUnavailable { .. } => StoreError::HistoryAnchorUnavailable {
        session_id: session(),
        node_id: node(),
        reason: AnchorUnavailable::Tombstoned,
    },
    HistoryNodeTooLarge { .. } => StoreError::HistoryNodeTooLarge {
        node_id: node(),
        required_bytes: 2,
        max_bytes: 1,
    },
    CursorForeignSession { .. } => StoreError::CursorForeignSession {
        cursor_session_id: SessionId::from("sampled-other"),
        session_id: session(),
    },
    HistoryCursorLineageChanged { .. } => StoreError::HistoryCursorLineageChanged {
        session_id: session(),
    },
    SessionBindingNotMaterialized { .. } => StoreError::SessionBindingNotMaterialized {
        session_id: session(),
    },
    StoreSessionMismatch { .. } => StoreError::StoreSessionMismatch {
        loaded: SessionId::from("sampled-other"),
        requested: session(),
    },
    SessionStateVersionNewerThanRuntime { .. } => StoreError::SessionStateVersionNewerThanRuntime {
        found: 2,
        current: 1,
    },
    SessionStateVersionUnsupported { .. } => StoreError::SessionStateVersionUnsupported {
        found: 1,
        current: 2,
    },
    Incompatible { .. } => StoreError::Incompatible {
        refusal: crate::compat::CompatRefusal::Unstamped {
            component: "sampled".to_string(),
            writing_release: None,
        },
    },
    WriterFenced { .. } => StoreError::WriterFenced {
        recorded: 2,
        writable: crate::compat::VersionRange::exactly(1),
    },
    InvalidSessionId { .. } => StoreError::InvalidSessionId { reason: "sampled" },
    BlankIdentity(_) => StoreError::BlankIdentity(
        crate::SessionId::parse("").expect_err("an empty string is no session id"),
    ),
    SessionDeleted { .. } => StoreError::SessionDeleted { session_id: session() },
    UnsupportedStoreOperation { .. } => StoreError::UnsupportedStoreOperation {
        operation: "sampled",
    },
    HeadRevisionConflict { .. } => StoreError::HeadRevisionConflict { expected: 1, actual: 2 },
    TurnCancelIntentChanged { .. } => StoreError::TurnCancelIntentChanged {
        session_id: session(),
        turn_id: turn(),
    },
    TurnCancelBindingMismatch { .. } => StoreError::TurnCancelBindingMismatch {
        session_id: session(),
        expected: "admitted".to_string(),
        presented: "other".to_string(),
    },
    TurnCancelClosureConflict { .. } => StoreError::TurnCancelClosureConflict {
        session_id: session(),
        turn_id: turn(),
    },
    TurnCancelClosureAuthorizationMismatch { .. } => {
        StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: session(),
            turn_id: turn(),
        }
    },
    TurnCancelClosureLifecyclePinned { .. } => StoreError::TurnCancelClosureLifecyclePinned {
        session_id: session(),
        pending_count: 1,
    },
    TurnCancelClosureScopeRetired { .. } => StoreError::TurnCancelClosureScopeRetired {
        scope_id: "sampled-scope".to_string(),
    },
    TurnCancelClosureOwnerReleased { .. } => StoreError::TurnCancelClosureOwnerReleased {
        participant_id: "sampled-owner".to_string(),
    },
    UnknownAttachment { .. } => StoreError::UnknownAttachment { digest: attachment() },
    IncompleteEnumeration { .. } => StoreError::IncompleteEnumeration {
        scope: "sampled",
        unfinished: "sampled".to_string(),
    },
    ReferrerKindRefused { .. } => StoreError::ReferrerKindRefused {
        kind: crate::artifact_referrer::ArtifactReferrerKind::Session,
        store: crate::artifact_referrer::ReferrerStore::Attachment,
    },
    StaleWritePermit { .. } => StoreError::StaleWritePermit { digest: attachment() },
    RuntimeTurnCommitConflict { .. } => StoreError::RuntimeTurnCommitConflict {
        session_id: session(),
        operation_key: "sampled-operation".to_string(),
    },
    AppendOperationIdentityConflict { .. } => StoreError::AppendOperationIdentityConflict {
        session_id: session(),
        operation_key: "sampled-operation".to_string(),
    },
    SemanticBoundaryIdentityConflict { .. } => StoreError::SemanticBoundaryIdentityConflict {
        session_id: session(),
        operation_key: "sampled-operation".to_string(),
    },
    AppendReceiptRequestedNodeCountCorrupt { .. } => {
        StoreError::AppendReceiptRequestedNodeCountCorrupt {
            session_id: session(),
            operation_key: "sampled-operation".to_string(),
            stored: 1,
            attempted: 2,
        }
    },
    TokenUsageAccountingOverflow { .. } => StoreError::TokenUsageAccountingOverflow {
        usage_source: "sampled".to_string(),
        model: "sampled".to_string(),
        counter: "input_tokens",
    },
    CheckpointTurnIndexOutOfRange { .. } => StoreError::CheckpointTurnIndexOutOfRange {
        turn_index: 2,
        max_exclusive: 1,
    },
    CheckpointTokenUsageOutOfRange { .. } => StoreError::CheckpointTokenUsageOutOfRange {
        counter: "input_tokens",
    },
    AppendAncestorNotActive { .. } => StoreError::AppendAncestorNotActive {
        required_node_id: node(),
    },
    NodeIdDerivationMismatch { .. } => StoreError::NodeIdDerivationMismatch {
        node_id: node(),
        expected_node_id: NodeId::from("sampled-derived"),
    },
    NodeIdCollision { .. } => StoreError::NodeIdCollision { node_id: node() },
    GraphGenerationCollision { .. } => StoreError::GraphGenerationCollision {
        session_id: session(),
        generation: 1,
    },
    InvalidGraphLeaf { .. } => StoreError::InvalidGraphLeaf { leaf_node_id: None },
    ForkTargetPending { .. } => StoreError::ForkTargetPending {
        session_id: session(),
        target: crate::session_store_factory_types::Target::Revision(1),
    },
    ForkTargetUnavailable { .. } => StoreError::ForkTargetUnavailable {
        session_id: session(),
        target: crate::session_store_factory_types::Target::Revision(1),
    },
    ForkTargetPruned { .. } => StoreError::ForkTargetPruned {
        session_id: session(),
        target: crate::session_store_factory_types::Target::Revision(1),
    },
    TurnBaseNotRetained { .. } => StoreError::TurnBaseNotRetained { revision: 1 },
    ForkSessionAlreadyExists { .. } => StoreError::ForkSessionAlreadyExists {
        session_id: session(),
    },
    InvalidGraphParent { .. } => StoreError::InvalidGraphParent {
        node_id: node(),
        expected: None,
        actual: Some(NodeId::from("sampled-parent")),
    },
    MissingFrameOpenAncestor { .. } => StoreError::MissingFrameOpenAncestor {
        leaf_node_id: node(),
    },
    IngressRowNotAdmitted { .. } => StoreError::IngressRowNotAdmitted {
        session_id: session(),
        run: turn(),
        row: Box::new(super::IngressRowId::Input(InputId::from("ti:sampled"))),
        admitted_run: None,
    },
    IngressSettlementDuplicate { .. } => StoreError::IngressSettlementDuplicate {
        session_id: session(),
        run: turn(),
        row: Box::new(super::IngressRowId::Input(InputId::from("ti:sampled"))),
    },
    IngressSettlementUnfenced { .. } => StoreError::IngressSettlementUnfenced {
        session_id: session(),
    },
    IngressAndSessionCommandRun { .. } => StoreError::IngressAndSessionCommandRun {
        session_id: session(),
        run: turn(),
    },
    SessionCommandWithdrawn { .. } => StoreError::SessionCommandWithdrawn {
        session_id: session(),
        batch_id: BatchId::from("sampled-batch"),
    },
    SessionHeadOwned { .. } => StoreError::SessionHeadOwned {
        session_id: session(),
        owner: super::SessionHeadOwner::Run { run: turn() },
    },
    StaleShiftFence { .. } => StoreError::StaleShiftFence {
        session_id: session(),
        fence_epoch: 1,
        current_epoch: 2,
    },
    RunAlreadyTerminal { .. } => StoreError::RunAlreadyTerminal {
        session_id: session(),
        run: turn(),
        by: Box::new(super::RunTerminalCause::OperatorCancelled {
            intent: super::ControlIntentId::from_sequence(1),
        }),
    },
    RunInputWithdrawn { .. } => StoreError::RunInputWithdrawn {
        session_id: session(),
        run: turn(),
    },
    SessionClosing { .. } => StoreError::SessionClosing {
        session_id: session(),
        intent: super::ControlIntentId::from_sequence(1),
    },
    ControlIntentUnknown { .. } => StoreError::ControlIntentUnknown {
        intent: super::ControlIntentId::from_sequence(1),
    },
    ShiftEpochUnavailable { .. } => StoreError::ShiftEpochUnavailable { session_id: session() },
    ShiftFenceSessionMismatch { .. } => StoreError::ShiftFenceSessionMismatch {
        session_id: session(),
        fence_session_id: SessionId::from("sampled-other"),
    },
    IngressTurnAddressUnknown { .. } => StoreError::IngressTurnAddressUnknown {
        session_id: session(),
        turn_id: turn(),
    },
    IngressReservedSourceKey { .. } => StoreError::IngressReservedSourceKey {
        session_id: session(),
        kind: "turn_input",
        source_key: "reserved:sampled".to_string(),
    },
    MonotonicCounterOverflow { .. } => StoreError::MonotonicCounterOverflow {
        counter: "sampled",
        current: i64::MAX as u64,
    },
    PendingTurnInputSourceKeyConflict { .. } => StoreError::PendingTurnInputSourceKeyConflict {
        session_id: session(),
        source_key: "host:sampled".to_string(),
        existing_input_id: InputId::from("ti:sampled"),
    },
    QueuedWorkSourceKeyConflict { .. } => StoreError::QueuedWorkSourceKeyConflict {
        session_id: session(),
        source_key: "host:sampled".to_string(),
        existing_batch_id: BatchId::from("sampled-batch"),
    },
    PendingTurnInputIdConflict { .. } => StoreError::PendingTurnInputIdConflict {
        session_id: session(),
        input_id: InputId::from("ti:sampled"),
    },
    PendingTurnInputBatchDuplicate { .. } => StoreError::PendingTurnInputBatchDuplicate {
        session_id: session(),
        name: "sampled".to_string(),
    },
    PendingTurnInputBatchForeignSession { .. } => {
        StoreError::PendingTurnInputBatchForeignSession {
            session_id: session(),
            draft_session_id: SessionId::from("sampled-other"),
        }
    },
    RunSpecHashCollision { .. } => StoreError::RunSpecHashCollision {
        session_id: session(),
        hash: "sampled-hash".to_string(),
    },
    PendingTurnInputRunSpecMismatch { .. } => StoreError::PendingTurnInputRunSpecMismatch {
        session_id: session(),
        turn_id: turn(),
    },
    RunSpecMissing { .. } => StoreError::RunSpecMissing {
        session_id: session(),
        hash: "sampled-hash".to_string(),
    },
    ProcessWakeSequenceRewound { .. } => StoreError::ProcessWakeSequenceRewound {
        session_id: session(),
        process_id: crate::process_id_for_test("sampled-process"),
        sequence: 1,
        allocation_floor: 2,
    },
    SessionExecutionLeaseExpired { .. } => StoreError::SessionExecutionLeaseExpired {
        session_id: session(),
    },
    UnfencedHeadPublication { .. } => StoreError::UnfencedHeadPublication {
        session_id: session(),
        backend: "sampled",
    },
    UnsupportedRecordSchemaVersion { .. } => StoreError::UnsupportedRecordSchemaVersion {
        record_kind: "Sampled",
        actual: 2,
        expected: 1,
    },
    MissingRecordSchemaVersion { .. } => StoreError::MissingRecordSchemaVersion {
        record_kind: "Sampled",
        expected: 1,
    },
    InvalidRecordSchemaVersion { .. } => StoreError::InvalidRecordSchemaVersion {
        record_kind: "Sampled",
        actual: "one".to_string(),
        expected: 1,
    },
    CheckpointComponentMissing { .. } => StoreError::CheckpointComponentMissing {
        key: "sampled".to_string(),
        blob_ref: crate::BlobRef::for_content(b"sampled"),
    },
    CheckpointRootMissing { .. } => StoreError::CheckpointRootMissing {
        blob_ref: crate::BlobRef::for_content(b"sampled"),
    },
    CheckpointComponentEncodingVersionMismatch { .. } => {
        StoreError::CheckpointComponentEncodingVersionMismatch {
            key: "sampled".to_string(),
            actual: 2,
            expected: 1,
        }
    },
    IncompleteCheckpointComponentSet => StoreError::IncompleteCheckpointComponentSet,
    RecordEncodingFailed { .. } => StoreError::RecordEncodingFailed {
        record_kind: "Sampled".to_string(),
        message: "sampled".to_string(),
    },
    ExecutionStateBodiesReleased => StoreError::ExecutionStateBodiesReleased,
    StoredDataCorrupt { .. } => StoreError::StoredDataCorrupt {
        record_kind: "Sampled",
        message: "sampled".to_string(),
    },
    ArtifactReferrerEnded { .. } => StoreError::ArtifactReferrerEnded { referrer: referrer() },
    ArtifactMissing { .. } => StoreError::ArtifactMissing {
        artifact_ref: "sampled-artifact".to_string(),
    },
    ArtifactCarryMissing { .. } => StoreError::ArtifactCarryMissing {
        artifact_ref: "sampled-artifact".to_string(),
        to: referrer(),
    },
    TurnChangeCursorPruned { .. } => StoreError::TurnChangeCursorPruned { horizon: super::TurnChangeCursor::initial() },
    TurnChangeCursorAhead { .. } => StoreError::TurnChangeCursorAhead { current: super::TurnChangeCursor::initial() },
    ParkFeedCursorCompacted { .. } => StoreError::ParkFeedCursorCompacted {
        horizon: super::ParkFeedCursor::initial(),
    },
    MigrationOpenElsewhere { .. } => StoreError::MigrationOpenElsewhere {
        database: "sampled".to_string(),
        location: std::path::PathBuf::from("sampled"),
    },
    StorageFailure { .. } => StoreError::StorageFailure {
        backend: "sampled",
        message: "sampled".to_string(),
    },
    Backend(_) => StoreError::Backend("sampled".to_string()),
}
