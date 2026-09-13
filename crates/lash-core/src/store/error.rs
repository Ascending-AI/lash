use crate::ProcessId;
use crate::SessionId;
/// The returned renewal field that made a resident lease unsafe to replace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionExecutionLeaseRenewalInstallMismatch {
    Session,
    OwnerIncarnation,
    Executor,
    LeaseToken,
    FencingToken,
    ExpiryRegressed,
}

impl SessionExecutionLeaseRenewalInstallMismatch {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::OwnerIncarnation => "owner_incarnation",
            Self::Executor => "executor",
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
    /// The turn's finalized outcome could not be materialized into resident
    /// state, so the commit aborted before any durable write. The typed runtime
    /// refusal travels unchanged: the turn's caller classifies it by its own
    /// code, never by this wrapper. Boxed so the carried error does not grow
    /// every `Result` that returns a store error.
    #[error("turn outcome materialization refused: {error}")]
    TurnOutcomeMaterializationRefused { error: Box<crate::RuntimeError> },
    /// The backend could not acquire its transactional write authority because
    /// another writer currently holds it. Retry the identical commit unchanged;
    /// do not reload, rebase, or alter its semantic content.
    #[error("store commit is contended; retry the identical commit unchanged")]
    Contended,
    #[error(
        "runtime commit records {node_count} rows for this attempt, exceeding the configured {max_nodes}-row node budget; the node budget covers rows written as recorded by this attempt, including attachment-intent adoption; a same-turn-id replay may stamp prior-attempt rows beyond the count"
    )]
    CommitNodeBudgetExceeded { node_count: usize, max_nodes: usize },
    #[error(
        "runtime commit carries {total_bytes} budgeted payload bytes, exceeding the {max_bytes}-byte transaction budget (session config: {session_config_bytes}, graph delta: {graph_delta_bytes}, checkpoint: {checkpoint_bytes}, attachment manifest: {attachment_manifest_bytes}, queue batches: {queue_batch_bytes}, agent frame: {agent_frame_bytes}, usage deltas: {usage_delta_bytes}, durable turn result: {turn_result_bytes})"
    )]
    CommitByteBudgetExceeded {
        session_config_bytes: usize,
        graph_delta_bytes: usize,
        checkpoint_bytes: usize,
        attachment_manifest_bytes: usize,
        queue_batch_bytes: usize,
        agent_frame_bytes: usize,
        usage_delta_bytes: usize,
        turn_result_bytes: usize,
        total_bytes: usize,
        max_bytes: usize,
    },
    #[error(
        "queued-work action reserve {action_token_reserve} exhausts model context window {max_context_tokens}"
    )]
    QueuedWorkActionReserveExhaustsContext {
        max_context_tokens: usize,
        action_token_reserve: usize,
    },
    #[error(
        "queued-work row `{batch_id}` at enqueue sequence {batch_enqueue_seq} renders to at least {rendered_tokens} tokens, exceeding model context window {max_context_tokens}; the row remains pending for host review"
    )]
    QueuedWorkRowExceedsContextWindow {
        batch_id: String,
        batch_enqueue_seq: u64,
        rendered_tokens: usize,
        max_context_tokens: usize,
    },
    #[error(
        "store is already bound to session `{bound_session_id}` and cannot be reused for `{attempted_session_id}`"
    )]
    SessionBindingMismatch {
        bound_session_id: SessionId,
        attempted_session_id: SessionId,
    },
    /// A session-scoped operation was attempted on a store handle that is not bound to a session.
    #[error("store handle is not bound to a session")]
    SessionNotBound,
    /// An unbound read found multiple candidate sessions and cannot choose one safely.
    #[error(
        "unbound store cannot resolve one session from {session_count} candidates; bind an explicit session"
    )]
    SessionResolutionAmbiguous { session_count: u64 },
    #[error("session `{session_id}` was admitted without durable session metadata")]
    SessionBindingNotMaterialized { session_id: SessionId },
    #[error(
        "session state version {found} is newer than this runtime's version {current}; upgrade the runtime before opening this session"
    )]
    SessionStateVersionNewerThanRuntime { found: u32, current: u32 },
    #[error(
        "session state version {found} has no conversion chain to {current}; drain sessions and recreate the store with this version"
    )]
    SessionStateVersionUnsupported { found: u32, current: u32 },
    #[error("invalid session id: {reason}")]
    InvalidSessionId { reason: &'static str },
    #[error(
        "session `{session_id}` was used and deleted; session ids cannot be reused in this store"
    )]
    SessionDeleted { session_id: SessionId },
    #[error("store does not support `{operation}`")]
    UnsupportedStoreOperation { operation: &'static str },
    /// A required-constraint inspection reached SQL outside the deliberately
    /// small grammar it can compare without guessing at semantic equivalence.
    #[error(
        "{backend} required-constraint inspection is inconclusive for `{table}.{constraint}`: {detail}"
    )]
    RequiredConstraintInspectionInconclusive {
        backend: &'static str,
        table: String,
        constraint: String,
        detail: String,
    },
    /// A persisted queued-work row carries only half of the predecessor claim
    /// correlation that an abandon would have to restore.
    #[error(
        "stored queued-work predecessor claim is corrupt: claim id present={claim_id_present}, claim token present={claim_token_present}"
    )]
    QueuedWorkPredecessorClaimCorrupt {
        claim_id_present: bool,
        claim_token_present: bool,
    },
    #[error("store head revision conflict: expected {expected}, actual {actual}")]
    HeadRevisionConflict { expected: u64, actual: u64 },
    /// Cancellation intent changed after the runtime observed it and before
    /// the same transaction could publish cancellation-dependent effects.
    #[error(
        "turn cancellation intent changed for session `{session_id}` turn `{turn_id}`; refresh cancellation authority and retry"
    )]
    TurnCancelIntentChanged {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// Session reopen selected a different cancellation authority than the one
    /// durably admitted before work began.
    #[error(
        "turn cancellation authority mismatch for session `{session_id}`: expected `{expected}`, got `{presented}`"
    )]
    TurnCancelBindingMismatch {
        session_id: SessionId,
        expected: String,
        presented: String,
    },
    /// A different exact closure operation already occupies this turn's
    /// non-overwritable authorization slot.
    #[error(
        "turn cancellation closure authorization conflicts for turn `{turn_id}` in session `{session_id}`"
    )]
    TurnCancelClosureConflict {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// A commit or repair attempted to consume an absent or different closure
    /// authorization.
    #[error(
        "turn cancellation closure authorization is missing or changed for turn `{turn_id}` in session `{session_id}`"
    )]
    TurnCancelClosureAuthorizationMismatch {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// Destructive lifecycle cleanup was attempted while exact closure work is
    /// still pinned. An execution-lane activation must drain it first.
    #[error(
        "session `{session_id}` has {pending_count} pending turn cancellation closure pin(s); activate and drain the session before deletion or scope retirement"
    )]
    TurnCancelClosureLifecyclePinned {
        session_id: SessionId,
        pending_count: usize,
    },
    /// The non-session physical owner was retired before this closure could be
    /// admitted. The retirement tombstone is permanent for that scope.
    #[error("turn cancellation closure scope `{scope_id}` is retired")]
    TurnCancelClosureScopeRetired { scope_id: String },
    /// Stored-reference adoption found the durable byte-absence fact left by a
    /// completed attachment GC delete. The boundary commit publishes nothing;
    /// the caller may re-put the digest and retry.
    #[error(
        "attachment `{digest}` bytes were reclaimed; re-put the attachment before retrying the commit"
    )]
    AttachmentBytesReclaimed { digest: crate::AttachmentId },
    #[error(
        "runtime operation `{operation_key}` for session `{session_id}` was retried with different commit content; reuse an operation identity only for the same logical operation"
    )]
    RuntimeTurnCommitConflict {
        session_id: SessionId,
        /// The commit operation identity, not a turn identity: every caller
        /// passes the operation storage key the conflicting retry reused.
        operation_key: String,
    },
    #[error(
        "runtime commit for session `{session_id}` cannot both borrow and release the session execution lease"
    )]
    RuntimeCommitLeaseAuthorityConflict { session_id: SessionId },
    /// One append operation id was reused for different semantic request content.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this distinction so runtimes never absorb a caller identity bug as
    /// an idempotent append replay.
    #[error(
        "append operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    AppendOperationIdentityConflict {
        session_id: SessionId,
        operation_key: String,
    },
    /// One semantic-boundary operation id was reused for different canonical
    /// request content (FIG-2480).
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this refusal so a non-retry with a differing canonical encoding
    /// is never silently deduplicated into a receipt replay.
    #[error(
        "semantic-boundary operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    SemanticBoundaryIdentityConflict {
        session_id: SessionId,
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
        session_id: SessionId,
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
    #[error("runtime commit node id must not be empty")]
    InvalidGraphNodeId { node_id: String },
    #[error("runtime commit generation {generation} already exists for session `{session_id}`")]
    GraphGenerationCollision {
        session_id: SessionId,
        generation: u64,
    },
    #[error("runtime commit leaf {leaf_node_id:?} does not resolve to a live graph node")]
    InvalidGraphLeaf { leaf_node_id: Option<String> },
    #[error("node `{node_id}` has no retained continuation anchor")]
    ForkPointNotRetained { node_id: String },
    #[error("fork target session `{session_id}` already exists")]
    ForkSessionAlreadyExists { session_id: SessionId },
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
        session_id: SessionId,
        claim_id: String,
        row_id: Option<Box<str>>,
        superseding_claim_id: Option<Box<str>>,
        superseding_session_lease_generation: Option<Box<u64>>,
    },
    #[doc(hidden)]
    #[error(
        "selected queued work intersects an interrupted claim and requires its full composition: {required_batch_ids:?}"
    )]
    SelectedQueuedWorkRequiresInterruptedComposition { required_batch_ids: Vec<String> },
    #[error(
        "turn input claim `{claim_id}` for session `{session_id}` is superseded at row {row_id:?} by claim {superseding_claim_id:?} in session-lease generation {superseding_session_lease_generation:?}"
    )]
    TurnInputClaimSuperseded {
        session_id: SessionId,
        claim_id: String,
        row_id: Option<Box<str>>,
        superseding_claim_id: Option<Box<str>>,
        superseding_session_lease_generation: Option<Box<u64>>,
    },
    /// An unclaimed turn-input settlement lost the head CAS.
    ///
    /// The settling turn accepted `input_id` itself and drove it without the
    /// session-execution lane ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)
    /// §5). Between acceptance and commit the row stopped being unclaimed and
    /// unsettled — a recovery claim took it, a cancel withdrew it, or another
    /// driver already settled it — so this commit affected zero rows and is
    /// refused whole. Unlike [`Self::TurnInputClaimSuperseded`] this settlement
    /// carries no lease generation, so it is never dropped and retried: the
    /// losing driver retires at its first commit attempt.
    #[error(
        "unclaimed turn-input settlement for session `{session_id}` lost the head CAS at row `{input_id}`: the row is {observed_state:?} and held by claim {superseding_claim_id:?}"
    )]
    UnclaimedTurnInputSettlementSuperseded {
        session_id: SessionId,
        input_id: String,
        observed_state: Option<Box<str>>,
        superseding_claim_id: Option<Box<str>>,
    },
    #[error(
        "runtime commit for session `{session_id}` includes queued-work-derived content without settling claim `{claim_id}`"
    )]
    UnsettledQueuedWorkClaim {
        session_id: SessionId,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` includes turn-input-derived content without settling claim `{claim_id}`"
    )]
    UnsettledTurnInputClaim {
        session_id: SessionId,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` attempts to settle foreign queued-work claim `{claim_id}`"
    )]
    ForeignQueuedWorkCompletion {
        session_id: SessionId,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` attempts to settle foreign turn-input claim `{claim_id}`"
    )]
    ForeignTurnInputCompletion {
        session_id: SessionId,
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
        session_id: SessionId,
        source_key: String,
        existing_input_id: String,
    },
    #[error(
        "process wake `{process_id}` sequence {sequence} for session `{session_id}` has no live receiver row and is at or below the receiver allocation floor {allocation_floor}; the sender store may have been restored or rewound"
    )]
    ProcessWakeSequenceRewound {
        session_id: SessionId,
        process_id: ProcessId,
        sequence: u64,
        allocation_floor: u64,
    },
    #[error("session execution lease for session `{session_id}` is missing or expired")]
    SessionExecutionLeaseExpired { session_id: SessionId },
    #[error(
        "session execution lease renewal for session `{session_id}` was refused because owner or lease token is no longer current"
    )]
    SessionExecutionLeaseRenewalRefused { session_id: SessionId },
    #[error(
        "session execution lease renewal response for session `{session_id}` was refused because its {mismatch} field did not preserve the presented lease"
    )]
    SessionExecutionLeaseRenewalInstallRefused {
        session_id: SessionId,
        mismatch: SessionExecutionLeaseRenewalInstallMismatch,
    },
    #[error(
        "session execution lease release for session `{session_id}` was refused because owner or lease token is no longer current"
    )]
    SessionExecutionLeaseReleaseRefused { session_id: SessionId },
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
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this when a complete checkpoint manifest names a component blob
    /// the backend cannot resolve. They must fail the load or commit rather than
    /// silently treating the key as deleted.
    #[error(
        "checkpoint component `{key}` references `{blob_ref}`, which is not present in the store"
    )]
    CheckpointComponentMissing {
        /// Stable logical key from the checkpoint root's complete component listing.
        key: String,
        /// Content address named by the manifest but absent from the backend.
        blob_ref: crate::BlobRef,
    },
    /// A checkpoint root observed before publication disappeared while its
    /// transaction waited to acquire the root's blob-row lock.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this instead of surfacing a backend foreign-key failure when
    /// concurrent reclaim wins publication of an already-persisted root.
    #[error("checkpoint root `{blob_ref}` is not present in the store")]
    CheckpointRootMissing {
        /// Content address of the checkpoint root removed before publication.
        blob_ref: crate::BlobRef,
    },
    /// A component's persisted codec is not the codec implemented by this build.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this before publishing or exposing a component with an unknown
    /// encoding; they must not reinterpret its bytes with the current codec.
    #[error(
        "checkpoint component `{key}` uses encoding version {actual}, but this build requires \
         version {expected}; remedy: drain affected sessions and recreate the store with this \
         Lash version"
    )]
    CheckpointComponentEncodingVersionMismatch {
        /// Stable logical key naming the incompatible component.
        key: String,
        /// Encoding version carried by the submitted or durable descriptor.
        actual: u32,
        /// Sole encoding version implemented by this Lash build.
        expected: u32,
    },
    /// A runtime commit was assembled from a projection that cannot prove it
    /// contains the checkpoint root's complete keyed component listing.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// preserve this refusal: only a set derived from a full hydrated manifest
    /// is `Complete`, and only then may absent keys be interpreted as deletion.
    /// A commit built from an `Unproven` set must fail before any backend write.
    #[error(
        "runtime checkpoint component set is incomplete; hydrate the durable checkpoint before committing"
    )]
    IncompleteCheckpointComponentSet,
    /// A durable value failed to encode before its bytes were written.
    #[error("failed to encode {record_kind}: {message}")]
    RecordEncodingFailed {
        record_kind: String,
        message: String,
    },
    /// The resident execution-state bodies were released after their commit
    /// and no accepted snapshot is retained in process, so this resident state
    /// cannot supply the execution a same-frame restore rebuilds from. Hydrate
    /// the durable checkpoint instead: reading the released root as "no
    /// execution" would rebuild an empty session over committed globals
    /// (FIG-2521).
    #[error(
        "execution-state bodies were released after their commit and no accepted snapshot is retained; hydrate the durable checkpoint before restoring"
    )]
    ExecutionStateBodiesReleased,
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
            Self::TurnOutcomeMaterializationRefused { .. } => "TurnOutcomeMaterializationRefused",
            Self::Contended => "Contended",
            Self::CommitNodeBudgetExceeded { .. } => "CommitNodeBudgetExceeded",
            Self::CommitByteBudgetExceeded { .. } => "CommitByteBudgetExceeded",
            Self::QueuedWorkActionReserveExhaustsContext { .. } => {
                "QueuedWorkActionReserveExhaustsContext"
            }
            Self::QueuedWorkRowExceedsContextWindow { .. } => "QueuedWorkRowExceedsContextWindow",
            Self::SessionBindingMismatch { .. } => "SessionBindingMismatch",
            Self::SessionNotBound => "SessionNotBound",
            Self::SessionResolutionAmbiguous { .. } => "SessionResolutionAmbiguous",
            Self::SessionBindingNotMaterialized { .. } => "SessionBindingNotMaterialized",
            Self::SessionStateVersionUnsupported { .. } => "SessionStateVersionUnsupported",
            Self::SessionStateVersionNewerThanRuntime { .. } => {
                "SessionStateVersionNewerThanRuntime"
            }
            Self::InvalidSessionId { .. } => "InvalidSessionId",
            Self::SessionDeleted { .. } => "SessionDeleted",
            Self::UnsupportedStoreOperation { .. } => "UnsupportedStoreOperation",
            Self::RequiredConstraintInspectionInconclusive { .. } => {
                "RequiredConstraintInspectionInconclusive"
            }
            Self::QueuedWorkPredecessorClaimCorrupt { .. } => "QueuedWorkPredecessorClaimCorrupt",
            Self::HeadRevisionConflict { .. } => "HeadRevisionConflict",
            Self::TurnCancelIntentChanged { .. } => "TurnCancelIntentChanged",
            Self::TurnCancelBindingMismatch { .. } => "TurnCancelBindingMismatch",
            Self::TurnCancelClosureConflict { .. } => "TurnCancelClosureConflict",
            Self::TurnCancelClosureAuthorizationMismatch { .. } => {
                "TurnCancelClosureAuthorizationMismatch"
            }
            Self::TurnCancelClosureLifecyclePinned { .. } => "TurnCancelClosureLifecyclePinned",
            Self::TurnCancelClosureScopeRetired { .. } => "TurnCancelClosureScopeRetired",
            Self::AttachmentBytesReclaimed { .. } => "AttachmentBytesReclaimed",
            Self::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict",
            Self::RuntimeCommitLeaseAuthorityConflict { .. } => {
                "RuntimeCommitLeaseAuthorityConflict"
            }
            Self::AppendOperationIdentityConflict { .. } => "AppendOperationIdentityConflict",
            Self::SemanticBoundaryIdentityConflict { .. } => "SemanticBoundaryIdentityConflict",
            Self::AppendReceiptRequestedNodeCountCorrupt { .. } => {
                "AppendReceiptRequestedNodeCountCorrupt"
            }
            Self::TokenUsageAccountingOverflow { .. } => "TokenUsageAccountingOverflow",
            Self::CheckpointTurnIndexOutOfRange { .. } => "CheckpointTurnIndexOutOfRange",
            Self::CheckpointTokenUsageOutOfRange { .. } => "CheckpointTokenUsageOutOfRange",
            Self::AppendAncestorNotActive { .. } => "AppendAncestorNotActive",
            Self::NodeIdDerivationMismatch { .. } => "NodeIdDerivationMismatch",
            Self::NodeIdCollision { .. } => "NodeIdCollision",
            Self::InvalidGraphNodeId { .. } => "InvalidGraphNodeId",
            Self::GraphGenerationCollision { .. } => "GraphGenerationCollision",
            Self::InvalidGraphLeaf { .. } => "InvalidGraphLeaf",
            Self::ForkPointNotRetained { .. } => "ForkPointNotRetained",
            Self::ForkSessionAlreadyExists { .. } => "ForkSessionAlreadyExists",
            Self::InvalidGraphParent { .. } => "InvalidGraphParent",
            Self::MissingFrameOpenAncestor { .. } => "MissingFrameOpenAncestor",
            Self::CurrentFrameNodeMismatch { .. } => "CurrentFrameNodeMismatch",
            Self::QueuedWorkClaimSuperseded { .. } => "QueuedWorkClaimSuperseded",
            Self::SelectedQueuedWorkRequiresInterruptedComposition { .. } => {
                "SelectedQueuedWorkRequiresInterruptedComposition"
            }
            Self::TurnInputClaimSuperseded { .. } => "TurnInputClaimSuperseded",
            Self::UnclaimedTurnInputSettlementSuperseded { .. } => {
                "UnclaimedTurnInputSettlementSuperseded"
            }
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
            Self::CheckpointRootMissing { .. } => "CheckpointRootMissing",
            Self::CheckpointComponentEncodingVersionMismatch { .. } => {
                "CheckpointComponentEncodingVersionMismatch"
            }
            Self::IncompleteCheckpointComponentSet => "IncompleteCheckpointComponentSet",
            Self::RecordEncodingFailed { .. } => "RecordEncodingFailed",
            Self::ExecutionStateBodiesReleased => "ExecutionStateBodiesReleased",
            Self::StoredDataCorrupt { .. } => "StoredDataCorrupt",
            Self::StorageFailure { .. } => "StorageFailure",
            Self::Backend(_) => "Backend",
        }
    }
}
