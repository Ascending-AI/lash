use crate::ProcessId;
use crate::SessionId;
use crate::{BatchId, InputId, NodeId};
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("session {session_id} already has unfinished root {root}")]
    UnfinishedRootConflict {
        session_id: crate::SessionId,
        root: crate::TurnId,
    },
    /// A pending follow-on owns the session (ADR 0101 §3, FIG-3542): no other
    /// turn commits and no other head write changes the fact until the
    /// follow-on's own terminal commit.
    #[error(
        "session {session_id} owes follow-on turn `{follow_on_turn_id}` (recovered {attempts} \
         times); nothing else commits until it does"
    )]
    FollowOnPending {
        session_id: crate::SessionId,
        follow_on_turn_id: crate::TurnId,
        attempts: u32,
    },
    /// A head write would leave a pending follow-on whose frame is not the
    /// head's current frame.
    #[error(
        "session {session_id} pending follow-on targets frame `{follow_on_frame_id}` but the \
         head's current frame is {current_frame_node_id:?}"
    )]
    FollowOnFrameNotCurrent {
        session_id: crate::SessionId,
        follow_on_frame_id: String,
        current_frame_node_id: Option<String>,
    },
    /// A head write broke the pending follow-on's write rules.
    #[error("session {session_id} pending follow-on write refused: {reason}")]
    FollowOnHeadInvariant {
        session_id: crate::SessionId,
        reason: String,
    },
    /// A recovering drive named a follow-on the head no longer owes.
    #[error("session {session_id} no longer owes follow-on turn `{follow_on_turn_id}`")]
    FollowOnNotPending {
        session_id: crate::SessionId,
        follow_on_turn_id: crate::TurnId,
    },

    /// Capturing dirty executor state failed before any store commit was
    /// attempted. The current execution must abort, but no publication is
    /// ambiguous and all live lease/claim ownership can be handed back.
    #[error("failed to snapshot dirty execution state: {message}")]
    ExecutionStateCaptureFailed { message: String },
    /// The runtime refused the turn's final commit: its finalized outcome
    /// could not be materialized into resident state, or a replayed journaled
    /// drive set was superseded and the turn ceded (ADR 0069 §6). Nothing was
    /// written. The typed runtime refusal travels unchanged: the turn's caller
    /// classifies it by its own code, never by this wrapper. Boxed so the
    /// carried error does not grow every `Result` that returns a store error.
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
        "runtime commit carries {total_bytes} budgeted payload bytes, exceeding the {max_bytes}-byte transaction budget (session config: {session_config_bytes}, graph delta: {graph_delta_bytes}, checkpoint: {checkpoint_bytes}, attachment manifest: {attachment_manifest_bytes}, pending follow-on: {follow_on_bytes}, agent frame: {agent_frame_bytes}, usage deltas: {usage_delta_bytes}, durable turn result: {turn_result_bytes})"
    )]
    CommitByteBudgetExceeded {
        session_config_bytes: usize,
        graph_delta_bytes: usize,
        checkpoint_bytes: usize,
        attachment_manifest_bytes: usize,
        follow_on_bytes: usize,
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
        batch_id: BatchId,
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
    /// A rebind declared a lineage that disagrees with the one durably
    /// recorded for this session.
    ///
    /// Admission reads the recorded relation and refuses the conflict instead
    /// of absorbing it as a plain [`SessionAdmission::Rebound`](crate::SessionAdmission::Rebound):
    /// the relation is a durable fact, so a binding that renames a parent — or
    /// claims one for a session recorded as a root — is answered, never
    /// smoothed. A binding that declares no lineage still rebinds.
    #[error(
        "session `{session_id}` is durably recorded as {} and cannot be rebound as {}",
        .recorded.label(),
        .requested.label()
    )]
    SessionRelationMismatch {
        session_id: SessionId,
        recorded: Box<crate::SessionLineage>,
        requested: Box<crate::SessionLineage>,
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
    /// A store, an epoch or a stored label this build cannot admit
    /// (ADR 0115 §1.5). The refusal names its remedy.
    #[error("{refusal}")]
    Incompatible {
        refusal: crate::compat::CompatRefusal,
    },
    /// The writer fence read `F` outside this build's writable range inside a
    /// mutating transaction (ADR 0115 §2.4): a newer release finalized, and
    /// this deployment takes no more work. The transaction wrote nothing.
    #[error(
        "writer fenced: the fleet epoch is {recorded}, outside this build's writable range \
         {writable}; this deployment takes no more work. Roll forward to a build whose writable \
         range contains {recorded}; `lashctl version` prints a build's range"
    )]
    WriterFenced {
        recorded: u32,
        writable: crate::compat::VersionRange,
    },
    #[error("invalid session id: {reason}")]
    InvalidSessionId { reason: &'static str },
    #[error(
        "session `{session_id}` was used and deleted; session ids cannot be reused in this store"
    )]
    SessionDeleted { session_id: SessionId },
    #[error("store does not support `{operation}`")]
    UnsupportedStoreOperation { operation: &'static str },
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
    /// Stored-reference adoption found no upload evidence for the digest: no
    /// manifest row anywhere in this store records a completed write of these
    /// bytes, or a physical delete of them is in flight. The boundary commit
    /// publishes nothing; the caller puts the bytes and retries.
    #[error(
        "attachment `{digest}` has no completed upload in this store; put the bytes before committing a reference to them"
    )]
    UnknownAttachment { digest: crate::AttachmentId },
    /// An attachment write permit was settled after its attempt had been
    /// superseded by a newer `begin_attachment_write` for the same row. A stale
    /// attempt certifies no upload: nothing was stamped.
    #[error(
        "attachment write permit for `{digest}` is stale; a newer write attempt owns this manifest row"
    )]
    StaleWritePermit { digest: crate::AttachmentId },
    #[error(
        "runtime operation `{operation_key}` for session `{session_id}` was retried with different commit content; reuse an operation identity only for the same logical operation"
    )]
    RuntimeTurnCommitConflict {
        session_id: SessionId,
        /// The commit operation identity, not a turn identity: every caller
        /// passes the operation storage key the conflicting retry reused.
        operation_key: String,
    },
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
        "append receipt `{operation_key}` for session `{session_id}` has contradictory requested-node counts (stored {stored}, attempted {attempted})"
    )]
    AppendReceiptRequestedNodeCountCorrupt {
        /// Session whose receipt failed its contracted count cross-check.
        session_id: SessionId,
        /// Canonical operation storage key of the corrupt receipt.
        operation_key: String,
        stored: u64,
        /// Count carried by the retry.
        attempted: u64,
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
    AppendAncestorNotActive { required_node_id: NodeId },
    #[error("runtime commit node `{node_id}` does not match derived node id `{expected_node_id}`")]
    NodeIdDerivationMismatch {
        node_id: NodeId,
        expected_node_id: NodeId,
    },
    #[error("runtime commit node id `{node_id}` already exists in durable session history")]
    NodeIdCollision { node_id: NodeId },
    #[error("runtime commit node id must not be empty")]
    InvalidGraphNodeId { node_id: NodeId },
    #[error("runtime commit generation {generation} already exists for session `{session_id}`")]
    GraphGenerationCollision {
        session_id: SessionId,
        generation: u64,
    },
    #[error("runtime commit leaf {:?} does not resolve to a live graph node", .leaf_node_id.as_deref())]
    InvalidGraphLeaf { leaf_node_id: Option<NodeId> },
    #[error("node `{node_id}` has no retained continuation anchor")]
    ForkPointNotRetained { node_id: NodeId },
    /// A replay asked for the head its turn was admitted on, and the store no
    /// longer holds that head's checkpoint or graph leaf (FIG-3682).
    #[error(
        "the session no longer retains the head at revision {revision} that its turn was \
         admitted on"
    )]
    TurnBaseNotRetained { revision: u64 },
    #[error("fork target session `{session_id}` already exists")]
    ForkSessionAlreadyExists { session_id: SessionId },
    #[error(
        "runtime commit node `{node_id}` has invalid parent {:?}; expected {:?}",
        .actual.as_deref(),
        .expected.as_deref()
    )]
    InvalidGraphParent {
        node_id: NodeId,
        expected: Option<NodeId>,
        actual: Option<NodeId>,
    },
    #[error(
        "session leaf `{leaf_node_id}` has no FrameOpen ancestor; every root graph must begin with a frame"
    )]
    MissingFrameOpenAncestor { leaf_node_id: NodeId },
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
    /// A commit's ingress settlement named a row its root did not admit:
    /// the row is open, bound to another root, or gone (FIG-3927). Nothing
    /// was written; a row is only ever answered by the root that admitted
    /// it.
    #[error(
        "root `{root}` of session `{session_id}` did not admit {row}; the row is bound to {admitted_root:?}"
    )]
    IngressRowNotAdmitted {
        session_id: SessionId,
        root: crate::TurnId,
        row: Box<super::IngressRowId>,
        admitted_root: Option<crate::TurnId>,
    },
    /// A commit's ingress settlement named one row twice.
    #[error("root `{root}` of session `{session_id}` settles {row} twice in one commit")]
    IngressSettlementDuplicate {
        session_id: SessionId,
        root: crate::TurnId,
        row: Box<super::IngressRowId>,
    },
    /// A commit that settles admitted rows or applies session commands
    /// presented no drive fence: only a sealed drive's fenced commit may
    /// (FIG-3927).
    #[error("a commit of session `{session_id}` settles ingress rows without a drive fence")]
    IngressSettlementUnfenced { session_id: SessionId },
    /// The command lane's applying commit found one of its command rows
    /// withdrawn or admitted since it read them (design §2.7). Nothing was
    /// written; the lane reads the commands again.
    #[error("session command batch `{batch_id}` of session `{session_id}` is no longer open")]
    SessionCommandWithdrawn {
        session_id: SessionId,
        batch_id: BatchId,
    },
    /// A storage operation fenced by a drive presented a fence that is not
    /// the session's current drive epoch: a later admission superseded it
    /// (ADR 0105 §2). Nothing was written.
    #[error(
        "drive fence epoch {fence_epoch} for session `{session_id}` is stale; the session is at drive epoch {current_epoch}"
    )]
    StaleDriveFence {
        session_id: SessionId,
        fence_epoch: u64,
        current_epoch: u64,
    },
    /// A terminal write named a logical root that already has a different
    /// terminal. The stored terminal stands and nothing was written (ADR 0105
    /// law L-S6): a later execution adopts it instead of ending the root
    /// twice.
    #[error("root `{root}` of session `{session_id}` is already terminal ({by:?})")]
    RootAlreadyTerminal {
        session_id: SessionId,
        root: crate::TurnId,
        by: Box<super::RootTerminalCause>,
    },
    /// Withdrawal settled every input bound to this root before its stale
    /// execution tried to park. No park was written.
    #[error("root `{root}` of session `{session_id}` has only withdrawn inputs")]
    RootInputWithdrawn {
        session_id: SessionId,
        root: crate::TurnId,
    },
    /// The session is closing: its `CloseSession` intent committed, so it
    /// accepts no input and admits no root. Deletion only retries from here
    /// (FIG-3600 S7).
    #[error("session `{session_id}` is closing under control intent {intent}")]
    SessionClosing {
        session_id: SessionId,
        intent: super::ControlIntentId,
    },
    /// No control intent has this id.
    #[error("control intent {intent} is unknown")]
    ControlIntentUnknown { intent: super::ControlIntentId },
    /// A drive-fenced storage operation found no `session_meta` row for its
    /// session, so the session has no drive epoch to fence against (ADR 0105
    /// §2). Nothing was written.
    #[error("session `{session_id}` has no drive epoch: no session_meta row")]
    DriveEpochUnavailable { session_id: SessionId },
    /// A drive-fenced storage operation named a session other than the one
    /// its drive fence authorizes. Nothing was written.
    #[error("drive fence for session `{fence_session_id}` cannot act on session `{session_id}`")]
    DriveFenceSessionMismatch {
        session_id: SessionId,
        fence_session_id: SessionId,
    },
    /// A turn-addressed item named a turn that is neither the session's
    /// running turn nor one of its ended turns. Nothing was stored: no row,
    /// no tombstone and no sequence number (ADR 0101 §5.1).
    #[error("session `{session_id}` has no running or ended turn `{turn_id}` to address")]
    IngressTurnAddressUnknown {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// A submission used a source key reserved for another kind (ADR 0101
    /// §8). Nothing was stored.
    #[error(
        "session `{session_id}` refused {kind} source key `{source_key}`: the prefix is reserved for another kind"
    )]
    IngressReservedSourceKey {
        session_id: SessionId,
        kind: &'static str,
        source_key: String,
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
        existing_input_id: InputId,
    },
    /// A draft named an `input_id` a stored pending-input row already carries,
    /// with different submitted content or from another session.
    ///
    /// Input ids are unique across the whole store, not within one session, so
    /// the refusing row may belong to a different session than the draft's. An
    /// identical same-session re-submission is not refused: it is the same
    /// admission re-run (a journaled turn acceptance provisions its id before
    /// its body runs, ADR 0069 §6) and returns the stored row.
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// return this so a reused input identity is never silently double-filed.
    #[error(
        "pending turn input id `{input_id}` is already bound to a stored input; session `{session_id}` cannot file another row under it"
    )]
    PendingTurnInputIdConflict {
        session_id: SessionId,
        input_id: InputId,
    },
    /// A turn-input batch named one input twice, by source key or input id
    /// (FIG-3842). The whole request was refused; nothing was stored.
    #[error("turn-input batch of session `{session_id}` names {name} more than once")]
    PendingTurnInputBatchDuplicate { session_id: SessionId, name: String },
    /// A turn-input batch for session `session_id` carried a draft for
    /// `draft_session_id` (FIG-3842). Nothing was stored.
    #[error(
        "turn-input batch of session `{session_id}` carries a draft for session `{draft_session_id}`"
    )]
    PendingTurnInputBatchForeignSession {
        session_id: SessionId,
        draft_session_id: SessionId,
    },
    /// Different run spec bytes arrived under a hash the session already
    /// interned (FIG-3838). Nothing was stored.
    #[error("run spec `{hash}` of session `{session_id}` is already interned with different bytes")]
    RunSpecHashCollision { session_id: SessionId, hash: String },
    /// An input addressed to running turn `turn_id` carried an explicit run
    /// spec that differs from the one that turn's root runs under
    /// (FIG-3838). Steering joins the running root's shape: omit the spec to
    /// inherit it. Nothing was stored.
    #[error(
        "input addressed to running turn `{turn_id}` of session `{session_id}` carries a run spec that differs from the turn's; omit the spec to inherit the running root's shape"
    )]
    PendingTurnInputRunSpecMismatch {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// A root named run spec `hash`, which its session does not hold.
    #[error("run spec `{hash}` of session `{session_id}` is not interned")]
    RunSpecMissing { session_id: SessionId, hash: String },
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
        "session head publication for session `{session_id}` on backend `{backend}` read its head revision outside the backend's single-writer transaction"
    )]
    UnfencedHeadPublication {
        session_id: SessionId,
        backend: &'static str,
    },
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
    /// An artifact publish or acquire named a referrer that has a fence
    /// (ADR 0113 §2.7). Carried typed so the plugin boundary classifies the
    /// refusal by code rather than by message text.
    #[error("artifact referrer `{referrer}` has ended")]
    ArtifactReferrerEnded {
        referrer: crate::artifact_referrer::ArtifactReferrer,
    },
    /// An artifact acquire named bytes that are not stored.
    #[error("artifact `{artifact_ref}` is not stored")]
    ArtifactMissing { artifact_ref: String },
    /// A cleanup's carry found its bytes gone: an invariant of ADR 0113 §3
    /// was broken, and the cleanup stalls rather than papering over it.
    #[error("artifact `{artifact_ref}` carried to `{to}` is not stored")]
    ArtifactCarryMissing {
        artifact_ref: String,
        to: crate::artifact_referrer::ArtifactReferrer,
    },
    /// A turn park feed cursor predates history `compact_turn_park_feed`
    /// removed. The consumer must perform a full relist before resuming from
    /// the reported horizon.
    #[error(
        "turn park feed cursor is below the compaction horizon {horizon:?}; a full relist is required"
    )]
    ParkFeedCursorCompacted {
        /// The lowest feed position the store still serves.
        horizon: crate::store::ParkFeedCursor,
    },
    /// A capture append or reset named an attempt epoch a later writer of
    /// the same invocation has fenced (ADR 0114 §3.2).
    #[error(
        "capture writer {invocation}#{attempt_epoch} of turn `{turn_id}` in session `{session_id}` is fenced by epoch {current_epoch}"
    )]
    CaptureWriterFenced {
        session_id: SessionId,
        turn_id: crate::TurnId,
        invocation: String,
        attempt_epoch: u32,
        current_epoch: u32,
    },
    /// A capture write arrived after the turn's capture was sealed.
    #[error(
        "capture of turn `{turn_id}` in session `{session_id}` is sealed through sequence {sealed_through}"
    )]
    CaptureSealed {
        session_id: SessionId,
        turn_id: crate::TurnId,
        sealed_through: u64,
    },
    /// A capture batch passed the frame or byte bound. Nothing was stored.
    #[error(
        "capture batch of {frames} frames and {bytes} bytes exceeds {max_frames} frames or {max_bytes} bytes"
    )]
    CaptureBatchTooLarge {
        frames: usize,
        bytes: u64,
        max_frames: usize,
        max_bytes: u64,
    },
    /// A capture batch held no frames. A batch is never empty: an empty
    /// ordinal could not replay idempotently from the frames alone. Nothing
    /// was stored and the ordinal stays unconsumed.
    #[error("capture batch {batch_ordinal} holds no frames")]
    CaptureBatchEmpty { batch_ordinal: u64 },
    /// A batch ordinal already stored a different body under the same
    /// writer.
    #[error(
        "capture batch {batch_ordinal} of writer {invocation}#{attempt_epoch} of turn `{turn_id}` in session `{session_id}` was already stored with a different body"
    )]
    CaptureBatchConflict {
        session_id: SessionId,
        turn_id: crate::TurnId,
        invocation: String,
        attempt_epoch: u32,
        batch_ordinal: u64,
    },
    /// A capture write or base advance named a base other than the turn's
    /// current one.
    #[error(
        "capture base {offered} of turn `{turn_id}` in session `{session_id}` is stale; the turn is at base {current}"
    )]
    CaptureBaseStale {
        session_id: SessionId,
        turn_id: crate::TurnId,
        offered: u32,
        current: u32,
    },
    /// A seal would stop short of a sequence the drive's recorded outcomes
    /// already reference.
    #[error(
        "capture seal of turn `{turn_id}` in session `{session_id}` through {sealed_through} is below the recorded watermark {recorded}"
    )]
    CaptureSealBelowWatermark {
        session_id: SessionId,
        turn_id: crate::TurnId,
        sealed_through: u64,
        recorded: u64,
    },
    /// The turn's capture frames do not fold into a partial.
    #[error("capture of turn `{turn_id}` in session `{session_id}` is corrupt: {violation:?}")]
    CaptureCorrupt {
        session_id: SessionId,
        turn_id: crate::TurnId,
        violation: crate::capture::CaptureReduceViolation,
    },
    /// A commit named a stopped partial the store holds no seal for.
    #[error("stopped partial of turn `{turn_id}` in session `{session_id}` is not sealed")]
    StoppedPartialNotSealed {
        session_id: SessionId,
        turn_id: crate::TurnId,
    },
    /// A commit named a stopped partial whose digest differs from the one
    /// already sealed or committed for its turn. The digests are boxed so
    /// they do not grow every `Result` that returns a store error.
    #[error(
        "stopped partial of turn `{turn_id}` in session `{session_id}` is {}, not {}",
        existing.to_hex(),
        offered.to_hex()
    )]
    StoppedPartialConflict {
        session_id: SessionId,
        turn_id: crate::TurnId,
        existing: Box<lash_sansio::StoppedPartialDigest>,
        offered: Box<lash_sansio::StoppedPartialDigest>,
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
    pub fn checked_monotonic_increment(counter: &'static str, current: u64) -> Result<u64, Self> {
        if current >= i64::MAX as u64 {
            return Err(Self::MonotonicCounterOverflow { counter, current });
        }
        Ok(current + 1)
    }

    /// Whether this is a fault of the storage substrate rather than a refusal:
    /// the identical operation may succeed when it is made again. Every other
    /// variant is the store's deterministic answer to the request, and making
    /// it again is refused the same way.
    ///
    /// The match is exhaustive for the same reason as [`Self::variant_name`]'s:
    /// a new variant does not compile until it is classified.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Contended | Self::StorageFailure { .. } | Self::Backend(_) => true,
            Self::ExecutionStateCaptureFailed { .. }
            | Self::TurnOutcomeMaterializationRefused { .. }
            | Self::CommitNodeBudgetExceeded { .. }
            | Self::CommitByteBudgetExceeded { .. }
            | Self::QueuedWorkActionReserveExhaustsContext { .. }
            | Self::QueuedWorkRowExceedsContextWindow { .. }
            | Self::SessionBindingMismatch { .. }
            | Self::SessionRelationMismatch { .. }
            | Self::SessionNotBound
            | Self::SessionResolutionAmbiguous { .. }
            | Self::SessionBindingNotMaterialized { .. }
            | Self::SessionStateVersionUnsupported { .. }
            | Self::Incompatible { .. }
            | Self::WriterFenced { .. }
            | Self::SessionStateVersionNewerThanRuntime { .. }
            | Self::InvalidSessionId { .. }
            | Self::SessionDeleted { .. }
            | Self::UnsupportedStoreOperation { .. }
            | Self::UnfinishedRootConflict { .. }
            | Self::FollowOnPending { .. }
            | Self::FollowOnFrameNotCurrent { .. }
            | Self::FollowOnHeadInvariant { .. }
            | Self::FollowOnNotPending { .. }
            | Self::HeadRevisionConflict { .. }
            | Self::TurnCancelIntentChanged { .. }
            | Self::TurnCancelBindingMismatch { .. }
            | Self::TurnCancelClosureConflict { .. }
            | Self::TurnCancelClosureAuthorizationMismatch { .. }
            | Self::TurnCancelClosureLifecyclePinned { .. }
            | Self::TurnCancelClosureScopeRetired { .. }
            | Self::UnknownAttachment { .. }
            | Self::StaleWritePermit { .. }
            | Self::RuntimeTurnCommitConflict { .. }
            | Self::AppendOperationIdentityConflict { .. }
            | Self::SemanticBoundaryIdentityConflict { .. }
            | Self::AppendReceiptRequestedNodeCountCorrupt { .. }
            | Self::TokenUsageAccountingOverflow { .. }
            | Self::CheckpointTurnIndexOutOfRange { .. }
            | Self::CheckpointTokenUsageOutOfRange { .. }
            | Self::AppendAncestorNotActive { .. }
            | Self::NodeIdDerivationMismatch { .. }
            | Self::NodeIdCollision { .. }
            | Self::InvalidGraphNodeId { .. }
            | Self::GraphGenerationCollision { .. }
            | Self::InvalidGraphLeaf { .. }
            | Self::ForkPointNotRetained { .. }
            | Self::TurnBaseNotRetained { .. }
            | Self::ForkSessionAlreadyExists { .. }
            | Self::InvalidGraphParent { .. }
            | Self::MissingFrameOpenAncestor { .. }
            | Self::CurrentFrameNodeMismatch { .. }
            | Self::IngressTurnAddressUnknown { .. }
            | Self::IngressRowNotAdmitted { .. }
            | Self::IngressSettlementDuplicate { .. }
            | Self::IngressSettlementUnfenced { .. }
            | Self::SessionCommandWithdrawn { .. }
            | Self::StaleDriveFence { .. }
            | Self::RootAlreadyTerminal { .. }
            | Self::RootInputWithdrawn { .. }
            | Self::SessionClosing { .. }
            | Self::ControlIntentUnknown { .. }
            | Self::DriveEpochUnavailable { .. }
            | Self::DriveFenceSessionMismatch { .. }
            | Self::IngressReservedSourceKey { .. }
            | Self::UnstagedUsageConfirmation { .. }
            | Self::MonotonicCounterOverflow { .. }
            | Self::PendingTurnInputSourceKeyConflict { .. }
            | Self::PendingTurnInputIdConflict { .. }
            | Self::PendingTurnInputBatchDuplicate { .. }
            | Self::PendingTurnInputBatchForeignSession { .. }
            | Self::RunSpecHashCollision { .. }
            | Self::PendingTurnInputRunSpecMismatch { .. }
            | Self::RunSpecMissing { .. }
            | Self::ProcessWakeSequenceRewound { .. }
            | Self::SessionExecutionLeaseExpired { .. }
            | Self::UnfencedHeadPublication { .. }
            | Self::UnsupportedRecordSchemaVersion { .. }
            | Self::MissingRecordSchemaVersion { .. }
            | Self::InvalidRecordSchemaVersion { .. }
            | Self::CheckpointComponentMissing { .. }
            | Self::CheckpointRootMissing { .. }
            | Self::CheckpointComponentEncodingVersionMismatch { .. }
            | Self::IncompleteCheckpointComponentSet
            | Self::RecordEncodingFailed { .. }
            | Self::ExecutionStateBodiesReleased
            | Self::StoredDataCorrupt { .. }
            | Self::ArtifactReferrerEnded { .. }
            | Self::ArtifactMissing { .. }
            | Self::ArtifactCarryMissing { .. }
            | Self::ParkFeedCursorCompacted { .. }
            | Self::CaptureWriterFenced { .. }
            | Self::CaptureSealed { .. }
            | Self::CaptureBatchTooLarge { .. }
            | Self::CaptureBatchEmpty { .. }
            | Self::CaptureBatchConflict { .. }
            | Self::CaptureBaseStale { .. }
            | Self::CaptureSealBelowWatermark { .. }
            | Self::CaptureCorrupt { .. }
            | Self::StoppedPartialNotSealed { .. }
            | Self::StoppedPartialConflict { .. } => false,
        }
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
            Self::SessionRelationMismatch { .. } => "SessionRelationMismatch",
            Self::SessionNotBound => "SessionNotBound",
            Self::SessionResolutionAmbiguous { .. } => "SessionResolutionAmbiguous",
            Self::SessionBindingNotMaterialized { .. } => "SessionBindingNotMaterialized",
            Self::SessionStateVersionUnsupported { .. } => "SessionStateVersionUnsupported",
            Self::Incompatible { .. } => "Incompatible",
            Self::WriterFenced { .. } => "WriterFenced",
            Self::SessionStateVersionNewerThanRuntime { .. } => {
                "SessionStateVersionNewerThanRuntime"
            }
            Self::InvalidSessionId { .. } => "InvalidSessionId",
            Self::SessionDeleted { .. } => "SessionDeleted",
            Self::UnsupportedStoreOperation { .. } => "UnsupportedStoreOperation",

            Self::UnfinishedRootConflict { .. } => "UnfinishedRootConflict",
            Self::FollowOnPending { .. } => "FollowOnPending",
            Self::FollowOnFrameNotCurrent { .. } => "FollowOnFrameNotCurrent",
            Self::FollowOnHeadInvariant { .. } => "FollowOnHeadInvariant",
            Self::FollowOnNotPending { .. } => "FollowOnNotPending",
            Self::HeadRevisionConflict { .. } => "HeadRevisionConflict",
            Self::TurnCancelIntentChanged { .. } => "TurnCancelIntentChanged",
            Self::TurnCancelBindingMismatch { .. } => "TurnCancelBindingMismatch",
            Self::TurnCancelClosureConflict { .. } => "TurnCancelClosureConflict",
            Self::TurnCancelClosureAuthorizationMismatch { .. } => {
                "TurnCancelClosureAuthorizationMismatch"
            }
            Self::TurnCancelClosureLifecyclePinned { .. } => "TurnCancelClosureLifecyclePinned",
            Self::TurnCancelClosureScopeRetired { .. } => "TurnCancelClosureScopeRetired",
            Self::UnknownAttachment { .. } => "UnknownAttachment",
            Self::StaleWritePermit { .. } => "StaleWritePermit",
            Self::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict",
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
            Self::TurnBaseNotRetained { .. } => "TurnBaseNotRetained",
            Self::ForkSessionAlreadyExists { .. } => "ForkSessionAlreadyExists",
            Self::InvalidGraphParent { .. } => "InvalidGraphParent",
            Self::MissingFrameOpenAncestor { .. } => "MissingFrameOpenAncestor",
            Self::CurrentFrameNodeMismatch { .. } => "CurrentFrameNodeMismatch",
            Self::IngressTurnAddressUnknown { .. } => "IngressTurnAddressUnknown",
            Self::IngressRowNotAdmitted { .. } => "IngressRowNotAdmitted",
            Self::IngressSettlementDuplicate { .. } => "IngressSettlementDuplicate",
            Self::IngressSettlementUnfenced { .. } => "IngressSettlementUnfenced",
            Self::SessionCommandWithdrawn { .. } => "SessionCommandWithdrawn",
            Self::StaleDriveFence { .. } => "StaleDriveFence",
            Self::RootAlreadyTerminal { .. } => "RootAlreadyTerminal",
            Self::RootInputWithdrawn { .. } => "RootInputWithdrawn",
            Self::SessionClosing { .. } => "SessionClosing",
            Self::ControlIntentUnknown { .. } => "ControlIntentUnknown",
            Self::DriveEpochUnavailable { .. } => "DriveEpochUnavailable",
            Self::DriveFenceSessionMismatch { .. } => "DriveFenceSessionMismatch",
            Self::IngressReservedSourceKey { .. } => "IngressReservedSourceKey",
            Self::UnstagedUsageConfirmation { .. } => "UnstagedUsageConfirmation",
            Self::MonotonicCounterOverflow { .. } => "MonotonicCounterOverflow",
            Self::PendingTurnInputSourceKeyConflict { .. } => "PendingTurnInputSourceKeyConflict",
            Self::PendingTurnInputIdConflict { .. } => "PendingTurnInputIdConflict",
            Self::PendingTurnInputBatchDuplicate { .. } => "PendingTurnInputBatchDuplicate",
            Self::PendingTurnInputBatchForeignSession { .. } => {
                "PendingTurnInputBatchForeignSession"
            }
            Self::RunSpecHashCollision { .. } => "RunSpecHashCollision",
            Self::PendingTurnInputRunSpecMismatch { .. } => "PendingTurnInputRunSpecMismatch",
            Self::RunSpecMissing { .. } => "RunSpecMissing",
            Self::ProcessWakeSequenceRewound { .. } => "ProcessWakeSequenceRewound",
            Self::SessionExecutionLeaseExpired { .. } => "SessionExecutionLeaseExpired",
            Self::UnfencedHeadPublication { .. } => "UnfencedHeadPublication",
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
            Self::ArtifactReferrerEnded { .. } => "ArtifactReferrerEnded",
            Self::ArtifactMissing { .. } => "ArtifactMissing",
            Self::ArtifactCarryMissing { .. } => "ArtifactCarryMissing",
            Self::ParkFeedCursorCompacted { .. } => "ParkFeedCursorCompacted",
            Self::CaptureWriterFenced { .. } => "CaptureWriterFenced",
            Self::CaptureSealed { .. } => "CaptureSealed",
            Self::CaptureBatchTooLarge { .. } => "CaptureBatchTooLarge",
            Self::CaptureBatchEmpty { .. } => "CaptureBatchEmpty",
            Self::CaptureBatchConflict { .. } => "CaptureBatchConflict",
            Self::CaptureBaseStale { .. } => "CaptureBaseStale",
            Self::CaptureSealBelowWatermark { .. } => "CaptureSealBelowWatermark",
            Self::CaptureCorrupt { .. } => "CaptureCorrupt",
            Self::StoppedPartialNotSealed { .. } => "StoppedPartialNotSealed",
            Self::StoppedPartialConflict { .. } => "StoppedPartialConflict",
            Self::StorageFailure { .. } => "StorageFailure",
            Self::Backend(_) => "Backend",
        }
    }
}
