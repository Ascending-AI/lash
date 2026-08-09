/// The returned renewal field that made a resident lease unsafe to replace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionExecutionLeaseRenewalInstallMismatch {
    Session,
    OwnerIncarnation,
    LeaseToken,
    FencingToken,
    ExpiryRegressed,
}

impl SessionExecutionLeaseRenewalInstallMismatch {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::OwnerIncarnation => "owner_incarnation",
            Self::LeaseToken => "lease_token",
            Self::FencingToken => "fencing_token",
            Self::ExpiryRegressed => "expiry",
        }
    }
}

impl std::fmt::Display for SessionExecutionLeaseRenewalInstallMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Capturing dirty executor state failed before any store commit was
    /// attempted. The current execution must abort, but no publication is
    /// ambiguous and all live lease/claim ownership can be handed back.
    #[error("failed to snapshot dirty execution state: {message}")]
    ExecutionStateCaptureFailed { message: String },
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
    #[error("session `{session_id}` was admitted without durable session metadata")]
    SessionBindingNotMaterialized { session_id: String },
    #[error("invalid session id: {reason}")]
    InvalidSessionId { reason: &'static str },
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
    #[error(
        "runtime commit for session `{session_id}` cannot both borrow and release the session execution lease"
    )]
    RuntimeCommitLeaseAuthorityConflict { session_id: String },
    /// One append operation id was reused for different semantic request content.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this distinction so runtimes never absorb a caller identity bug as
    /// an idempotent append replay.
    #[error(
        "append operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    AppendOperationIdentityConflict {
        session_id: String,
        operation_key: String,
    },
    /// A matching append receipt carries contradictory requested-node counts.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// treat this as durable receipt corruption and must never replay it.
    #[error(
        "append receipt `{operation_key}` for session `{session_id}` has contradictory requested-node counts (stored {stored:?}, attempted {attempted:?})"
    )]
    AppendReceiptRequestedNodeCountCorrupt {
        /// Session whose receipt failed its contracted count cross-check.
        session_id: String,
        /// Canonical operation storage key of the corrupt receipt.
        operation_key: String,
        /// Count stored with the first attempt, or `None` when corruptly absent.
        stored: Option<u64>,
        /// Count carried by the retry, or `None` when corruptly absent.
        attempted: Option<u64>,
    },
    /// Token usage counters overflowed while durable usage rows were
    /// accumulated for staging, commit, or load.
    #[error(
        "token usage counter `{counter}` overflowed while accumulating ({usage_source}, {model})"
    )]
    TokenUsageAccountingOverflow {
        /// Caller-defined usage source whose accumulated counter overflowed.
        usage_source: String,
        /// Model identifier for the overflowing ledger row.
        model: String,
        /// Name of the overflowing [`crate::TokenUsage`] counter.
        counter: &'static str,
    },
    /// A checkpoint carried a turn index too close to the platform limit for
    /// the runtime's bare next-turn increments to remain safe after restore.
    ///
    /// Integrator class (ADR 0051): **runtime embedders** surface this restore
    /// failure and reconstruct or repair the affected session; store
    /// implementors preserve the checkpoint value without reinterpreting it.
    #[error(
        "checkpoint turn index {turn_index} is outside the restorable range (must be below {max_exclusive})"
    )]
    CheckpointTurnIndexOutOfRange {
        /// Turn index decoded from the durable checkpoint.
        turn_index: usize,
        /// First turn index excluded by the runtime's restore invariant.
        max_exclusive: usize,
    },
    /// A checkpoint carried session token usage whose aggregations do not fit
    /// `i64`, so every restored consumer of a bare sum would be poisoned.
    ///
    /// Integrator class (ADR 0051): **runtime embedders** surface this restore
    /// failure and reconstruct or repair the affected session; store
    /// implementors preserve the checkpoint counters without reinterpreting
    /// them.
    #[error("checkpoint token usage counter `{counter}` is outside the restorable range")]
    CheckpointTokenUsageOutOfRange {
        /// Name of the aggregation that overflowed on the decoded checkpoint.
        counter: &'static str,
    },
    /// A fresh append named an ancestor outside the durable active path.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// enforce this after receipt lookup so an already-committed retry wins,
    /// while runtimes translate a fresh rejection to `StaleBranch`.
    #[error("append requires inactive ancestor `{required_node_id}`")]
    AppendAncestorNotActive { required_node_id: String },
    #[error("runtime commit node `{node_id}` does not match derived node id `{expected_node_id}`")]
    NodeIdDerivationMismatch {
        node_id: String,
        expected_node_id: String,
    },
    #[error("runtime commit node id `{node_id}` already exists in durable session history")]
    NodeIdCollision { node_id: String },
    #[error("runtime commit generation {generation} already exists for session `{session_id}`")]
    GraphGenerationCollision { session_id: String, generation: u64 },
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
    /// A commit's claimed current frame disagrees with its graph-derived frame.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this typed corruption fence after deriving the nearest live
    /// `FrameOpen` ancestor from the post-commit graph.
    #[error(
        "runtime commit current frame {claimed:?} does not match nearest FrameOpen ancestor {derived:?}"
    )]
    CurrentFrameNodeMismatch {
        /// Frame node id supplied by the runtime commit.
        claimed: Option<String>,
        /// Nearest `FrameOpen` node id derived by the store.
        derived: Option<String>,
    },
    #[error(
        "queued work claim `{claim_id}` for session `{session_id}` is superseded at row {row_id:?} by claim {superseding_claim_id:?} in session-lease generation {superseding_session_lease_generation:?}"
    )]
    QueuedWorkClaimSuperseded {
        session_id: String,
        claim_id: String,
        row_id: Option<Box<str>>,
        superseding_claim_id: Option<Box<str>>,
        superseding_session_lease_generation: Option<Box<u64>>,
    },
    #[error(
        "turn input claim `{claim_id}` for session `{session_id}` is superseded at row {row_id:?} by claim {superseding_claim_id:?} in session-lease generation {superseding_session_lease_generation:?}"
    )]
    TurnInputClaimSuperseded {
        session_id: String,
        claim_id: String,
        row_id: Option<Box<str>>,
        superseding_claim_id: Option<Box<str>>,
        superseding_session_lease_generation: Option<Box<u64>>,
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
        "runtime commit for session `{session_id}` attempts to settle foreign queued-work claim `{claim_id}`"
    )]
    ForeignQueuedWorkCompletion {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` attempts to settle foreign turn-input claim `{claim_id}`"
    )]
    ForeignTurnInputCompletion {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit has {completed_count} {claim_kind} completions for {originating_count} originating claims"
    )]
    ClaimSettlementCountMismatch {
        claim_kind: &'static str,
        originating_count: usize,
        completed_count: usize,
    },
    #[error(
        "store confirmed {confirmed_count} usage identities, but only {staged_count} were staged"
    )]
    UnstagedUsageConfirmation {
        confirmed_count: usize,
        staged_count: usize,
    },
    #[error("monotonic counter `{counter}` cannot advance past {current}")]
    MonotonicCounterOverflow { counter: &'static str, current: u64 },
    #[error(
        "pending turn input source_key `{source_key}` for session `{session_id}` is already bound to input `{existing_input_id}` with different submitted content"
    )]
    PendingTurnInputSourceKeyConflict {
        session_id: String,
        source_key: String,
        existing_input_id: String,
    },
    #[error(
        "process wake `{process_id}` sequence {sequence} for session `{session_id}` has no live receiver row and is at or below the receiver allocation floor {allocation_floor}; the sender store may have been restored or rewound"
    )]
    ProcessWakeSequenceRewound {
        session_id: String,
        process_id: String,
        sequence: u64,
        allocation_floor: u64,
    },
    #[error("session execution lease for session `{session_id}` is missing or expired")]
    SessionExecutionLeaseExpired { session_id: String },
    #[error(
        "session execution lease renewal for session `{session_id}` was refused because owner or lease token is no longer current"
    )]
    SessionExecutionLeaseRenewalRefused { session_id: String },
    #[error(
        "session execution lease renewal response for session `{session_id}` was refused because its {mismatch} field did not preserve the presented lease"
    )]
    SessionExecutionLeaseRenewalInstallRefused {
        session_id: String,
        mismatch: SessionExecutionLeaseRenewalInstallMismatch,
    },
    #[error(
        "session execution lease release for session `{session_id}` was refused because owner or lease token is no longer current"
    )]
    SessionExecutionLeaseReleaseRefused { session_id: String },
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
    /// Checkpoint-specialized dangling-pointer failure; other unreadable durable
    /// records use [`Self::StoredDataCorrupt`].
    #[error("checkpoint {component} component `{blob_ref}` is not present in the store")]
    CheckpointComponentMissing {
        component: &'static str,
        blob_ref: crate::BlobRef,
    },
    /// A durable row was present but unreadable; checkpoint dangling pointers
    /// use the specialized [`Self::CheckpointComponentMissing`] variant.
    #[error("stored {record_kind} data is corrupt: {message}")]
    StoredDataCorrupt {
        /// Stable name of the durable record whose payload was unreadable.
        record_kind: &'static str,
        /// Backend codec diagnostic describing the malformed payload.
        message: String,
    },
    /// The storage substrate failed an operation before a trustworthy value
    /// could be returned.
    #[error("{backend} storage failure: {message}")]
    StorageFailure {
        /// Stable backend name, such as `sqlite` or `postgres`.
        backend: &'static str,
        /// Backend diagnostic for the failed storage operation.
        message: String,
    },
    #[error("store backend error: {0}")]
    Backend(String),
}

impl StoreError {
    /// Advances a fence, generation, sequence, or revision without allowing
    /// wraparound or a silent no-op at the numeric ceiling.
    #[doc(hidden)]
    pub fn checked_monotonic_increment(counter: &'static str, current: u64) -> Result<u64, Self> {
        if current >= i64::MAX as u64 {
            return Err(Self::MonotonicCounterOverflow { counter, current });
        }
        Ok(current + 1)
    }

    /// Stable name of this error's enum variant.
    ///
    /// The match is deliberately exhaustive inside `lash-core` so adding a
    /// variant cannot silently collapse distinct errors in external
    /// diagnostics that must use a wildcard for this non-exhaustive enum.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::ExecutionStateCaptureFailed { .. } => "ExecutionStateCaptureFailed",
            Self::Contended => "Contended",
            Self::CommitNodeBudgetExceeded { .. } => "CommitNodeBudgetExceeded",
            Self::CommitByteBudgetExceeded { .. } => "CommitByteBudgetExceeded",
            Self::SessionBindingMismatch { .. } => "SessionBindingMismatch",
            Self::SessionBindingNotMaterialized { .. } => "SessionBindingNotMaterialized",
            Self::InvalidSessionId { .. } => "InvalidSessionId",
            Self::SessionDeleted { .. } => "SessionDeleted",
            Self::UnsupportedStoreOperation { .. } => "UnsupportedStoreOperation",
            Self::HeadRevisionConflict { .. } => "HeadRevisionConflict",
            Self::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict",
            Self::RuntimeCommitLeaseAuthorityConflict { .. } => {
                "RuntimeCommitLeaseAuthorityConflict"
            }
            Self::AppendOperationIdentityConflict { .. } => "AppendOperationIdentityConflict",
            Self::AppendReceiptRequestedNodeCountCorrupt { .. } => {
                "AppendReceiptRequestedNodeCountCorrupt"
            }
            Self::TokenUsageAccountingOverflow { .. } => "TokenUsageAccountingOverflow",
            Self::CheckpointTurnIndexOutOfRange { .. } => "CheckpointTurnIndexOutOfRange",
            Self::CheckpointTokenUsageOutOfRange { .. } => "CheckpointTokenUsageOutOfRange",
            Self::AppendAncestorNotActive { .. } => "AppendAncestorNotActive",
            Self::NodeIdDerivationMismatch { .. } => "NodeIdDerivationMismatch",
            Self::NodeIdCollision { .. } => "NodeIdCollision",
            Self::GraphGenerationCollision { .. } => "GraphGenerationCollision",
            Self::InvalidGraphLeaf { .. } => "InvalidGraphLeaf",
            Self::ForkPointNotRetained { .. } => "ForkPointNotRetained",
            Self::ForkSessionAlreadyExists { .. } => "ForkSessionAlreadyExists",
            Self::InvalidGraphParent { .. } => "InvalidGraphParent",
            Self::MissingFrameOpenAncestor { .. } => "MissingFrameOpenAncestor",
            Self::CurrentFrameNodeMismatch { .. } => "CurrentFrameNodeMismatch",
            Self::QueuedWorkClaimSuperseded { .. } => "QueuedWorkClaimSuperseded",
            Self::TurnInputClaimSuperseded { .. } => "TurnInputClaimSuperseded",
            Self::UnsettledQueuedWorkClaim { .. } => "UnsettledQueuedWorkClaim",
            Self::UnsettledTurnInputClaim { .. } => "UnsettledTurnInputClaim",
            Self::ForeignQueuedWorkCompletion { .. } => "ForeignQueuedWorkCompletion",
            Self::ForeignTurnInputCompletion { .. } => "ForeignTurnInputCompletion",
            Self::ClaimSettlementCountMismatch { .. } => "ClaimSettlementCountMismatch",
            Self::UnstagedUsageConfirmation { .. } => "UnstagedUsageConfirmation",
            Self::MonotonicCounterOverflow { .. } => "MonotonicCounterOverflow",
            Self::PendingTurnInputSourceKeyConflict { .. } => "PendingTurnInputSourceKeyConflict",
            Self::ProcessWakeSequenceRewound { .. } => "ProcessWakeSequenceRewound",
            Self::SessionExecutionLeaseExpired { .. } => "SessionExecutionLeaseExpired",
            Self::SessionExecutionLeaseRenewalRefused { .. } => {
                "SessionExecutionLeaseRenewalRefused"
            }
            Self::SessionExecutionLeaseRenewalInstallRefused { .. } => {
                "SessionExecutionLeaseRenewalInstallRefused"
            }
            Self::SessionExecutionLeaseReleaseRefused { .. } => {
                "SessionExecutionLeaseReleaseRefused"
            }
            Self::UnsupportedRecordSchemaVersion { .. } => "UnsupportedRecordSchemaVersion",
            Self::MissingRecordSchemaVersion { .. } => "MissingRecordSchemaVersion",
            Self::InvalidRecordSchemaVersion { .. } => "InvalidRecordSchemaVersion",
            Self::CheckpointComponentMissing { .. } => "CheckpointComponentMissing",
            Self::StoredDataCorrupt { .. } => "StoredDataCorrupt",
            Self::StorageFailure { .. } => "StorageFailure",
            Self::Backend(_) => "Backend",
        }
    }
}
