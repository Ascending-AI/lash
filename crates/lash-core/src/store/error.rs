#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The backend could not acquire its transactional write authority because
    /// another writer currently holds it. Retry the identical commit unchanged;
    /// do not reload, rebase, or alter its semantic content.
    #[error("store commit is contended; retry the identical commit unchanged")]
    Contended,
    #[error(
        "runtime commit has {node_count} graph nodes, exceeding the {max_nodes}-node transaction budget"
    )]
    CommitNodeBudgetExceeded { node_count: usize, max_nodes: usize },
    #[error(
        "runtime commit carries {total_bytes} budgeted payload bytes, exceeding the {max_bytes}-byte transaction budget (graph delta: {graph_delta_bytes}, checkpoint: {checkpoint_bytes}, attachment manifest: {attachment_manifest_bytes})"
    )]
    CommitByteBudgetExceeded {
        graph_delta_bytes: usize,
        checkpoint_bytes: usize,
        attachment_manifest_bytes: usize,
        total_bytes: usize,
        max_bytes: usize,
    },
    #[error(
        "store is already bound to session `{bound_session_id}` and cannot be reused for `{attempted_session_id}`"
    )]
    SessionBindingMismatch {
        bound_session_id: String,
        attempted_session_id: String,
    },
    #[error(
        "session `{session_id}` was used and deleted; session ids cannot be reused in this store"
    )]
    SessionDeleted { session_id: String },
    #[error("store does not support `{operation}`")]
    UnsupportedStoreOperation { operation: &'static str },
    #[error("store head revision conflict: expected {expected}, actual {actual}")]
    HeadRevisionConflict { expected: u64, actual: u64 },
    #[error(
        "runtime operation `{turn_id}` for session `{session_id}` was retried with different commit content; reuse an operation identity only for the same logical operation"
    )]
    RuntimeTurnCommitConflict { session_id: String, turn_id: String },
    #[error("runtime commit node `{node_id}` does not match derived node id `{expected_node_id}`")]
    NodeIdDerivationMismatch {
        node_id: String,
        expected_node_id: String,
    },
    #[error("runtime commit node id `{node_id}` already exists in durable session history")]
    NodeIdCollision { node_id: String },
    #[error("runtime commit leaf {leaf_node_id:?} does not resolve to a live graph node")]
    InvalidGraphLeaf { leaf_node_id: Option<String> },
    #[error("node `{node_id}` has no retained continuation anchor")]
    ForkPointNotRetained { node_id: String },
    #[error("fork target session `{session_id}` already exists")]
    ForkSessionAlreadyExists { session_id: String },
    #[error("runtime commit node `{node_id}` has invalid parent {actual:?}; expected {expected:?}")]
    InvalidGraphParent {
        node_id: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error(
        "session leaf `{leaf_node_id}` has no FrameOpen ancestor; every root graph must begin with a frame"
    )]
    MissingFrameOpenAncestor { leaf_node_id: String },
    #[error(
        "queued work claim `{claim_id}` for session `{session_id}` is superseded by a newer session-lease generation"
    )]
    QueuedWorkClaimSuperseded {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "turn input claim `{claim_id}` for session `{session_id}` is superseded by a newer session-lease generation"
    )]
    TurnInputClaimSuperseded {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` includes queued-work-derived content without settling claim `{claim_id}`"
    )]
    UnsettledQueuedWorkClaim {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` includes turn-input-derived content without settling claim `{claim_id}`"
    )]
    UnsettledTurnInputClaim {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "pending turn input source_key `{source_key}` for session `{session_id}` is already bound to input `{existing_input_id}` with different submitted content"
    )]
    PendingTurnInputSourceKeyConflict {
        session_id: String,
        source_key: String,
        existing_input_id: String,
    },
    #[error("session execution lease for session `{session_id}` is missing or expired")]
    SessionExecutionLeaseExpired { session_id: String },
    #[error(
        "{record_kind} schema_version {actual} is not supported by this binary (expected {expected})"
    )]
    UnsupportedRecordSchemaVersion {
        record_kind: &'static str,
        actual: u32,
        expected: u32,
    },
    #[error(
        "{record_kind} is missing schema_version and was written by unsupported pre-versioned state (expected {expected})"
    )]
    MissingRecordSchemaVersion {
        record_kind: &'static str,
        expected: u32,
    },
    #[error("{record_kind} schema_version {actual} is invalid (expected integer {expected})")]
    InvalidRecordSchemaVersion {
        record_kind: &'static str,
        actual: String,
        expected: u32,
    },
    #[error("checkpoint {component} component `{blob_ref}` is not present in the store")]
    CheckpointComponentMissing {
        component: &'static str,
        blob_ref: crate::BlobRef,
    },
    #[error("store backend error: {0}")]
    Backend(String),
}
