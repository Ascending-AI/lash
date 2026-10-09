//! Runtime commit envelope and result types.

use super::{
    BlobRef, GraphAppend, HydratedSessionCheckpoint, OperationId, RealizedNodeTimestamp,
    SessionCheckpoint, StoreError, commit_identity,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
};
use crate::SessionId;

/// A committed frame switch's artifact half (ADR 0113 §3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameTransition {
    pub ended: crate::artifact_referrer::FrameEnvironmentId,
    pub successor: crate::artifact_referrer::FrameEnvironmentId,
    pub carries: Vec<crate::artifact_referrer::ArtifactName>,
    /// The committing execution: the only one that can still read `ended`.
    pub gate: lash_sansio::EffectJournalIdentity,
}

impl FrameTransition {
    /// The `Ended` cleanup of `ended` this transition upserts: its carries
    /// are applied inside the commit, so the record carries none.
    #[must_use]
    pub fn ended_cleanup(&self) -> crate::artifact_referrer::ArtifactCleanup {
        crate::artifact_referrer::ArtifactCleanup::ended(
            crate::artifact_referrer::ArtifactReferrer::FrameEnvironment(self.ended.clone()),
            Vec::new(),
            Some(self.gate.clone()),
        )
    }
}

/// Every frame a commit leaves, in the order it left them: the prior head's
/// frame, then each frame whose open the commit appends, except the frame
/// the new head holds (ADR 0113 §3.1). A store ends all of
/// them in the commit's transaction, whether or not a [`FrameTransition`]
/// rides the commit, so no frame outlives the commit that leaves it.
#[must_use]
pub fn frames_left_by_commit(
    prior_head: Option<&crate::FrameNodeId>,
    graph: &super::GraphAppend,
    new_head: Option<&crate::FrameNodeId>,
) -> Vec<crate::FrameNodeId> {
    prior_head
        .cloned()
        .into_iter()
        .chain(graph.nodes().iter().filter_map(|node| {
            node.frame_open()
                .and_then(|_| crate::FrameNodeId::new(node.node_id.as_str().to_owned()).ok())
        }))
        .filter(|frame| Some(frame) != new_head)
        .collect()
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    /// Host policy carried to the shared facade and backend validation seams.
    /// It is operational authority and intentionally excluded from the durable
    /// semantic commit identity projection.
    pub commit_budget: super::CommitBudget,
    pub session_id: SessionId,
    pub expected_head_revision: u64,
    /// The logical run's terminal evidence, present exactly on the commit of
    /// the run's final physical turn (FIG-3600 S7): written in this commit's
    /// transaction, refused [`StoreError::RunAlreadyTerminal`](super::StoreError::RunAlreadyTerminal)
    /// when the run already ended otherwise. An instruction to the store
    /// derived from the turn it commits, never commit content: like the
    /// fences, it is excluded from the commit's serialized form.
    #[serde(skip)]
    pub run_terminal: Option<Box<super::RunTerminalWrite>>,
    /// The frame this commit ends and the frame it opens (ADR 0113 §3.1):
    /// present exactly on a commit that switches frames. The backend applies
    /// it in the commit's own transaction with the head CAS: it checks every
    /// carried artifact has an edge of `ended`, inserts the successor's edges,
    /// fences `ended` and upserts its `Ended` cleanup gated on `gate`. A
    /// store instruction derived from the graph the commit appends, never
    /// commit content: like the fences, it is excluded from the commit's
    /// serialized form.
    #[serde(skip)]
    pub frame_transition: Option<FrameTransition>,
    pub config: crate::PersistedSessionConfig,
    /// The config the committing run ran under, when it is not the config
    /// the commit writes: a run executes under its recorded execution view and
    /// writes the head's sticky config back (FIG-3841). The view is the
    /// commit's content, so the commit identity covers it in place of
    /// [`Self::config`]; the sticky config is the head's, not the operation's,
    /// and may have moved since the run first committed, so a redrive that
    /// replays the run's committed operation still answers its receipt. An
    /// input to the identity, never stored: `None` when the two agree.
    #[serde(skip)]
    pub execution_config: Option<Box<crate::PersistedSessionConfig>>,
    pub graph: GraphAppend,
    /// Resident leaf observed when this commit was built. For
    /// `GraphAppend::PreserveHead` this is the effective committed leaf bound
    /// into the whole-commit hash; for `Extend` it records the base the
    /// appended nodes extend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_base_leaf_node_id: Option<crate::NodeId>,
    pub checkpoint: HydratedSessionCheckpoint,
    /// Bounded, non-transcript evidence settled with this turn record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<crate::TurnFailureEvidence>,
    /// Terminal already computed by the turn driver. Nonturn operations have no outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnCommitOutcome>,
    /// The physical turn's admitted trace scope, committed with its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Box<TurnTraceReceipt>>,
    pub turn_commit: RuntimeTurnCommitStamp,
    /// What this commit does with the rows its run admitted (FIG-3927):
    /// completions, releases and drops, each predicated on the row still
    /// being bound to the run.
    ///
    /// A cancelled turn hands the work it withheld from its terminal
    /// checkpoint to its cancellation here (FIG-3531, FIG-3543): withheld
    /// input is released or dropped by the undelivered disposition, withheld
    /// wakes are always released, and the backend records each row on the
    /// cancellation's outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<super::IngressSettlement>,
    /// The session-command batches this commit applied (design §2.7). The
    /// command lane takes no admission: each row must still exist and be
    /// open, or the whole commit is refused
    /// [`StoreError::SessionCommandWithdrawn`](super::StoreError::SessionCommandWithdrawn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_commands: Option<crate::QueuedWorkCompletion>,
    /// Each command's outcome, keyed by its batch, recorded atomically with
    /// the head revision compare-and-set and covered by the commit identity.
    /// Every key must belong to [`Self::applied_commands`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub command_outcomes: std::collections::BTreeMap<crate::BatchId, crate::SessionCommandOutcome>,
    /// Unique attachment-manifest rows this commit will stamp as adopted.
    /// Runtime assembly derives this from explicit attachment references and
    /// turn-owned write-ahead intents before store validation begins. Per ADR
    /// 0058 this count is a declared estimate, not a store query: replay can
    /// undercount prior-attempt turn-owned rows, and cancelled or failed puts
    /// can overcount — that residual is accepted, do not re-engineer it.
    #[serde(default)]
    pub adopted_intent_rows: u64,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by the turn id in `turn_commit.operation`, including ids that appear only in plain tool
    /// JSON. This list preserves typed-output and cross-turn re-references.
    /// Adoption is an upsert keyed on (session, attachment): when this session
    /// has no manifest row for an adopted id, the backend creates one —
    /// stamping the commit's intent time and copying the earliest proven
    /// upload evidence recorded under any session.
    pub committed_attachment_ids: Vec<crate::AttachmentId>,
}

#[cfg(any(test, feature = "testing"))]
impl RuntimeCommit {
    const fn recommended_test_commit_budget() -> super::CommitBudget {
        super::CommitBudget::bounded(1024 * 1024, 512)
    }

    #[track_caller]
    pub fn persisted_state_for_test(state: &crate::RuntimeSessionState) -> Self {
        Self::persisted_state_for_test_with_budget(state, Self::recommended_test_commit_budget())
    }

    #[track_caller]
    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
    pub fn persisted_state_for_test_with_budget(
        state: &crate::RuntimeSessionState,
        commit_budget: super::CommitBudget,
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("test commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            commit_budget,
            crate::store::FleetFormat::current(),
        )
        .expect("test commit must be hashable")
    }

    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
    pub fn persisted_state_with_operation_for_testing(
        state: &crate::RuntimeSessionState,
        operation: OperationId,
    ) -> Self {
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("fixed-identity test commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
        .expect("fixed-identity test commit must be hashable")
    }

    #[track_caller]
    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
    pub fn persisted_state_with_graph_commit(
        state: &crate::RuntimeSessionState,
        mut graph: GraphAppend,
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-graph-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("test graph commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
        .expect("test graph commit must be hashable")
    }

    pub fn persisted_state_with_operation(
        state: &mut crate::RuntimeSessionState,
        operation: OperationId,
    ) -> Result<(Self, Vec<crate::NodeId>), StoreError> {
        Self::persisted_state_with_operation_and_budget(
            state,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
    }

    pub fn persisted_state_with_graph_commit_and_operation(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        operation: OperationId,
    ) -> Result<Self, StoreError> {
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
    }
}

/// Trace facts retained by the physical turn's business commit.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnTraceReceipt {
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
    pub scope: lash_trace::DurableTraceScope,
    pub context: lash_trace::TraceContext,
    pub outcome: lash_trace::TraceTurnOutcome,
    pub run_scope: Option<lash_trace::DurableTraceScope>,
}

/// The terminal of a committed physical turn, independent of live observations.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnCommitOutcome {
    Completed,
    FrameSwitch,
    Cancelled,
    Failed(TurnCommitFailureCause),
}

/// The typed stop cause of a failed turn. Cancellation has its own outcome.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnCommitFailureCause {
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
    ContextOverflow,
    PluginAbort,
    RuntimeError,
    AgentFrameSwitchLimit,
    SubmittedError,
    ToolError,
}

impl TurnCommitOutcome {
    /// Derive the durable label from the terminal produced by the turn driver.
    pub fn from_terminal(outcome: &lash_sansio::TurnOutcome) -> Self {
        use lash_sansio::{TurnOutcome, TurnStop};
        match outcome {
            TurnOutcome::Finished(_) => Self::Completed,
            TurnOutcome::AgentFrameSwitch { .. } => Self::FrameSwitch,
            TurnOutcome::Stopped(stop) => match stop {
                TurnStop::Cancelled { .. } => Self::Cancelled,
                TurnStop::Incomplete => Self::Failed(TurnCommitFailureCause::Incomplete),
                TurnStop::InvalidInput => Self::Failed(TurnCommitFailureCause::InvalidInput),
                TurnStop::MaxTurns => Self::Failed(TurnCommitFailureCause::MaxTurns),
                TurnStop::ToolFailure | TurnStop::ToolPanicked { .. } => {
                    Self::Failed(TurnCommitFailureCause::ToolFailure)
                }
                TurnStop::ProviderError => Self::Failed(TurnCommitFailureCause::ProviderError),
                TurnStop::ContextOverflow => Self::Failed(TurnCommitFailureCause::ContextOverflow),
                TurnStop::PluginAbort => Self::Failed(TurnCommitFailureCause::PluginAbort),
                TurnStop::RuntimeError => Self::Failed(TurnCommitFailureCause::RuntimeError),
                TurnStop::AgentFrameSwitchLimit => {
                    Self::Failed(TurnCommitFailureCause::AgentFrameSwitchLimit)
                }
                TurnStop::SubmittedError { .. } => {
                    Self::Failed(TurnCommitFailureCause::SubmittedError)
                }
                TurnStop::ToolError { .. } => Self::Failed(TurnCommitFailureCause::ToolError),
            },
        }
    }

    /// The checked SQL label for each outcome and failed cause.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::FrameSwitch => "frame_switch",
            Self::Cancelled => "cancelled",
            Self::Failed(TurnCommitFailureCause::Incomplete) => "failed_incomplete",
            Self::Failed(TurnCommitFailureCause::InvalidInput) => "failed_invalid_input",
            Self::Failed(TurnCommitFailureCause::MaxTurns) => "failed_max_turns",
            Self::Failed(TurnCommitFailureCause::ToolFailure) => "failed_tool_failure",
            Self::Failed(TurnCommitFailureCause::ProviderError) => "failed_provider_error",
            Self::Failed(TurnCommitFailureCause::ContextOverflow) => "failed_context_overflow",
            Self::Failed(TurnCommitFailureCause::PluginAbort) => "failed_plugin_abort",
            Self::Failed(TurnCommitFailureCause::RuntimeError) => "failed_runtime_error",
            Self::Failed(TurnCommitFailureCause::AgentFrameSwitchLimit) => {
                "failed_agent_frame_switch_limit"
            }
            Self::Failed(TurnCommitFailureCause::SubmittedError) => "failed_submitted_error",
            Self::Failed(TurnCommitFailureCause::ToolError) => "failed_tool_error",
        }
    }
}

/// Refuse a receipt whose typed terminal disagrees with its indexed SQL label.
pub fn validate_turn_commit_outcome_code(
    receipt: &RuntimeCommitReceipt,
    stored: Option<&str>,
) -> Result<(), StoreError> {
    let expected = receipt.outcome.as_ref().map(TurnCommitOutcome::as_str);
    if expected != stored {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "RuntimeCommitReceipt",
            message: format!("outcome column {stored:?} differs from receipt {expected:?}"),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RuntimeCommitReceipt {
    /// Timestamp selected by the committing store, unchanged on receipt replay.
    pub committed_at_ms: u64,
    pub schema_version: u32,
    pub head_revision: u64,
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
    /// Leaf selected by the committed operation. Receipt replay returns the
    /// first attempt's value even when later commits have advanced the session.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_leaf_node_id: Option<crate::NodeId>,
    /// Node timestamps are clock-derived and excluded from commit intent, so a
    /// receipt replay must return the first attempt's values for the resident
    /// graph to converge with durable history.
    pub realized_node_timestamps: Vec<RealizedNodeTimestamp>,
    /// Bounded failure evidence owned by this durable turn settlement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<crate::TurnFailureEvidence>,
    /// Typed terminal recorded in the same transaction as this receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnCommitOutcome>,
    /// The physical turn's admitted trace scope, committed with its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub trace: Option<Box<TurnTraceReceipt>>,
    /// Each command batch's outcome, recorded with the applying commit so
    /// later resubmissions answer their own result.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[schemars(with = "std::collections::BTreeMap<String, serde_json::Value>")]
    pub command_outcomes: std::collections::BTreeMap<crate::BatchId, crate::SessionCommandOutcome>,
    /// Whether session work remained after ingress settlement in the committing
    /// transaction. Receipt replay returns this decision unchanged.
    pub work_remaining: bool,
    /// Canonical input applications settled by this idempotent turn commit.
    ///
    /// Keeping these identities in the durable turn-commit result lets hosts
    /// reconcile after the bounded live observation window has been lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_input_applications: Vec<crate::TurnInputApplication>,
    /// Undelivered active-turn inputs disposed by this commit.
    ///
    /// This is a store-derived result, not commit intent, and is deliberately
    /// absent from `turn_commit_hash`.
    #[serde(
        default,
        skip_serializing_if = "crate::TurnCancelInputOutcome::is_empty"
    )]
    pub turn_cancel_input_outcome: crate::TurnCancelInputOutcome,
    /// Whether the store answered this attempt from an existing durable receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// set this transient decision bit when returning an earlier commit result;
    /// serialization always omits it and deserialization always clears it.
    #[serde(skip)]
    #[schemars(skip)]
    pub receipt_replayed: bool,
}

/// The durable `result_json` receipt schema this build writes.
///
/// Version 1 is the first stamped encoding. Receipts written before the field
/// existed carry no `schema_version` at all and are refused as
/// [`StoreError::MissingRecordSchemaVersion`], matching the exact-version
/// refusal every other durable record follows.
///
/// version_guard(
///     items(decode_runtime_commit_receipt, ensure_supported_receipt_version),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "RuntimeCommitReceipt"
pub const RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 2's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "RuntimeCommitReceipt"
pub const RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: u32 = 2;

/// Stable record-kind label the receipt's decode refusals carry.
pub const RUNTIME_COMMIT_RECEIPT_RECORD_KIND: &str = "RuntimeCommitReceipt";

/// Decode one persisted `result_json` receipt body for a session's operation.
///
/// Every read of the receipt column fails closed through this one codec: a
/// payload that is not valid JSON, a missing or invalid `schema_version`, a
/// version this binary does not support, or a body outside the current shape
/// is a refusal, never a skipped row. `turn_id` is the row's operation storage
/// key, named so a refusal identifies the exact durable record.
///
/// This is the no-store form: it answers what a context with no recorded `F`
/// can admit — this build's newest version alone. Reads on a bound store go
/// through [`decode_runtime_commit_receipt_for_fleet`].
pub fn decode_runtime_commit_receipt(
    session_id: &SessionId,
    turn_id: &str,
    json: &str,
) -> Result<RuntimeCommitReceipt, StoreError> {
    decode_runtime_commit_receipt_for_fleet(
        session_id,
        turn_id,
        json,
        super::FleetFormat::current(),
    )
}

/// The fleet leg of [`decode_runtime_commit_receipt`]: the receipt admits the
/// version `fleet` records for the surface as well as this build's newest —
/// the `[N-1, N]` reader window of ADR 0106 §2 (FIG-3796). An admitted older
/// payload climbs to the newest through the surface's [`RecordUpcaster`]
/// hooks before it decodes.
pub fn decode_runtime_commit_receipt_for_fleet(
    session_id: &SessionId,
    turn_id: &str,
    json: &str,
    fleet: super::FleetFormat,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
            message: format!(
                "session `{session_id}` receipt `{turn_id}` is not valid JSON: {error}"
            ),
        })?;
    let actual = ensure_supported_record_schema_version_for_fleet(
        RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        &value,
        crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        fleet,
    )?;
    let window = fleet.read_window(crate::surface_format!(
        RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
    ));
    if actual != window.newest() {
        super::upcast_json_record(
            RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
            crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
            actual,
            window.newest(),
            &mut value,
        )?;
    }
    serde_json::from_value(value).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind: RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        message: format!(
            "session `{session_id}` receipt `{turn_id}` does not match the supported shape: {error}"
        ),
    })
}

/// Enforce the receipt version contract on an already-typed receipt.
///
/// Backends that hold the receipt as a value rather than serialized bytes
/// apply the same refusal the JSON codec does.
///
/// This is the no-store form; a bound store's reads go through
/// [`ensure_supported_receipt_version_for_fleet`].
pub fn ensure_supported_receipt_version(receipt: &RuntimeCommitReceipt) -> Result<(), StoreError> {
    ensure_supported_receipt_version_for_fleet(receipt, super::FleetFormat::current())
}

/// The fleet leg of [`ensure_supported_receipt_version`]: admits the version
/// `fleet` records for the surface alongside this build's newest (FIG-3796).
pub fn ensure_supported_receipt_version_for_fleet(
    receipt: &RuntimeCommitReceipt,
    fleet: super::FleetFormat,
) -> Result<(), StoreError> {
    ensure_supported_schema_version_for_fleet(
        RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        receipt.schema_version,
        crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        fleet,
    )
}

/// Replay identity carried by one runtime commit.
///
/// A plain commit carries no append identity. An append carries its canonical
/// version, hash, and node count as one variant; only the ancestor fence is
/// genuinely optional. A semantic boundary carries the FIG-2480 request-identity
/// receipt for exactly the record-config and create-session operations: the
/// operation is a typed field beside the versioned canonical
/// request hash, never recoverable only from the hash.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppendRequestIdentity {
    /// A commit adjudicated only by its canonical runtime-commit hash.
    PlainCommit,
    /// A semantic append request with a comparable versioned identity.
    Append {
        /// Version of the canonical append-request encoding.
        #[serde(rename = "identity_encoding_version")]
        encoding_version: u32,
        /// SHA-256 identity of the semantic append request.
        #[serde(rename = "request_identity_hash")]
        request_hash: String,
        /// Number of semantic nodes supplied by the append caller.
        requested_node_count: u64,
        /// Branch ancestor named by the append caller, when one was required.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_ancestor_node_id: Option<String>,
    },
    /// A non-append boundary request with a comparable versioned identity.
    ///
    /// Receipts under this identity answer "same request retried?" for the
    /// operation named by the typed tag and never reconstruct requests
    /// (store-as-continuation doctrine). One generic variant serves every
    /// adopting operation; per-operation variants were rejected in the
    /// FIG-869 ratification.
    SemanticBoundary {
        /// Operation family that owns this receipt identity.
        operation: SemanticBoundaryOperation,
        /// Version of that operation's canonical request encoding.
        #[serde(rename = "identity_encoding_version")]
        encoding_version: u32,
        /// BLAKE3 identity of the canonical semantic boundary request.
        #[serde(rename = "request_identity_hash")]
        request_hash: String,
    },
}

/// Boundary operations that adjudicate retries through a semantic-boundary
/// receipt identity (FIG-2480).
///
/// This vocabulary is persisted replay identity: variants serialize as the
/// exact operation-key strings and must never be renamed (ADR 0063 discipline
/// applies). `commit_identity` accepts the identity for exactly these
/// operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SemanticBoundaryOperation {
    /// `record-config`: persist the materialized protocol configuration.
    #[serde(rename = "record-config")]
    RecordConfig,
    /// `create-session`: register a newly materialized child session.
    #[serde(rename = "create-session")]
    CreateSession,
}

impl SemanticBoundaryOperation {
    /// The [`OperationId::key`] this identity family is valid for.
    pub fn operation_key(self) -> &'static str {
        match self {
            Self::RecordConfig => "record-config",
            Self::CreateSession => "create-session",
        }
    }

    /// Returns `None` for every key outside the adopted set; callers refuse
    /// the identity rather than guessing a family.
    pub fn from_operation_key(key: &str) -> Option<Self> {
        match key {
            "record-config" => Some(Self::RecordConfig),
            "create-session" => Some(Self::CreateSession),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTurnCommitStamp {
    pub operation: OperationId,
    /// Plain-commit or append-request replay identity.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    // No durable JSON path writes this stamp; the column path is the durable one.
    pub append_request_identity: AppendRequestIdentity,
}

impl RuntimeTurnCommitStamp {
    /// Binds one operation identity to a runtime commit for store implementors enforcing replay and
    /// idempotency at the atomic commit boundary.
    pub fn new(operation: OperationId) -> Self {
        Self {
            operation,
            append_request_identity: AppendRequestIdentity::PlainCommit,
        }
    }

    pub fn append_session_nodes(
        operation: OperationId,
        requested_ancestor_node_id: Option<&str>,
        nodes: &[crate::SessionAppendNode],
    ) -> Result<Self, StoreError> {
        let request_identity_hash = commit_identity::append_request_identity_hash(
            &operation,
            requested_ancestor_node_id,
            nodes,
        )?;
        Ok(Self {
            operation,
            append_request_identity: AppendRequestIdentity::Append {
                encoding_version: commit_identity::APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
                request_hash: request_identity_hash,
                requested_node_count: u64::try_from(nodes.len()).map_err(|_| {
                    StoreError::Backend("append requested-node count does not fit u64".to_string())
                })?,
                requested_ancestor_node_id: requested_ancestor_node_id.map(str::to_string),
            },
        })
    }
}

impl RuntimeCommit {
    /// Whether this commit is the session's own head write, which no head
    /// owner refuses (FIG-4202): a commit under a run (the run whose end it
    /// records, or the physical turn it commits), or the command lane
    /// applying the open commands it settles ([`Self::applied_commands`]).
    #[must_use]
    pub fn is_sessions_own_head_write(&self) -> bool {
        self.run_terminal.is_some()
            || self.turn_commit.operation.turn_id().is_some()
            || self
                .applied_commands
                .as_ref()
                .is_some_and(|commands| !commands.batch_ids.is_empty())
    }

    /// This commit, ending `transition.ended` and opening
    /// `transition.successor` in its own transaction.
    #[must_use]
    pub fn with_frame_transition(mut self, transition: FrameTransition) -> Self {
        self.frame_transition = Some(transition);
        self
    }

    /// Stamp the semantic-boundary replay identity derived from this commit's
    /// operation and canonical request content (FIG-2480).
    ///
    /// Call this as the final step of building a record-config,
    /// or create-session commit, after every semantic field is
    /// in place: the identity hash is computed from the commit itself, and
    /// store validation refuses a stamp that no longer matches the content it
    /// rides with. Refuses every operation outside the adopted set.
    pub fn stamp_semantic_boundary(&mut self) -> Result<(), StoreError> {
        let operation =
            SemanticBoundaryOperation::from_operation_key(&self.turn_commit.operation.key)
                .ok_or_else(|| {
                    StoreError::Backend(format!(
                        "semantic-boundary receipt identity is not defined for operation `{}`",
                        self.turn_commit.operation.key
                    ))
                })?;
        let (encoding_version, request_hash) =
            super::semantic_boundary::semantic_boundary_request_identity(self, operation)?;
        self.turn_commit.append_request_identity = AppendRequestIdentity::SemanticBoundary {
            operation,
            encoding_version,
            request_hash,
        };
        Ok(())
    }
}

/// Opaque position in one deployment's durable turn and session terminal feed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnChangeCursor(u64);

impl TurnChangeCursor {
    pub const fn initial() -> Self {
        Self(0)
    }
    /// Store implementors bind a position; hosts persist the opaque cursor.
    pub const fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }
    pub const fn store_sequence(self) -> u64 {
        self.0
    }

    /// Validate a read or acknowledgement against the same snapshot's clock.
    pub fn check(self, current: u64, horizon: u64) -> Result<(), StoreError> {
        if self.0 < horizon {
            return Err(StoreError::TurnChangeCursorPruned {
                horizon: Self(horizon),
            });
        }
        if self.0 > current {
            return Err(StoreError::TurnChangeCursorAhead {
                current: Self(current),
            });
        }
        Ok(())
    }
}

/// The record that changed, with the existing typed terminal vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnChangeKind {
    Committed {
        operation: OperationId,
        outcome: TurnCommitOutcome,
    },
    /// Each newly recorded fault remains evidence after an operator clears it.
    SessionFault {
        record: Box<super::SessionFaultRecord>,
    },
    SessionDeleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnChange {
    pub cursor: TurnChangeCursor,
    pub session_id: SessionId,
    pub recorded_at_ms: u64,
    pub kind: TurnChangeKind,
}

/// A bounded, commit-ordered read, independent of the live replay buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnChangePage {
    pub changes: Vec<TurnChange>,
    pub next: TurnChangeCursor,
    /// Reads before this retained position fail explicitly.
    pub retained_after: TurnChangeCursor,
}

/// Explicit projector policy for terminal evidence reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnProjectionWatermark {
    UpTo(TurnChangeCursor),
    /// The host has deliberately chosen to keep no durable projector.
    NoProjector,
}

impl TurnProjectionWatermark {
    /// Highest acknowledged sequence, validated before a sweep removes evidence.
    pub fn acknowledged_sequence(self, current: u64) -> Result<i64, StoreError> {
        let seq = match self {
            Self::UpTo(cursor) => {
                cursor.check(current, 0)?;
                cursor.store_sequence()
            }
            Self::NoProjector => current,
        };
        i64::try_from(seq).map_err(|_| StoreError::StoredDataCorrupt {
            record_kind: "TurnChangeClock",
            message: "sequence exceeds SQL BIGINT".to_owned(),
        })
    }
}

impl TurnChange {
    /// Decode a feed row after the SQL page has bounded its receipt reads.
    pub fn from_stored(
        sequence: i64,
        session_id: String,
        operation: Option<String>,
        payload: Option<String>,
        outcome_code: Option<String>,
        recorded_at_ms: i64,
        fleet: super::FleetFormat,
    ) -> Result<Self, StoreError> {
        let corrupt = |message: &str| StoreError::StoredDataCorrupt {
            record_kind: "TurnChange",
            message: message.to_owned(),
        };
        let session_id = SessionId::parse(session_id)?;
        let sequence = u64::try_from(sequence).map_err(|_| corrupt("negative sequence"))?;
        if sequence == 0 {
            return Err(corrupt("zero sequence"));
        }
        let recorded_at_ms =
            u64::try_from(recorded_at_ms).map_err(|_| corrupt("negative timestamp"))?;
        let kind = match (operation, payload, outcome_code) {
            (Some(operation), Some(payload), Some(code)) => {
                let receipt = decode_runtime_commit_receipt_for_fleet(
                    &session_id,
                    &operation,
                    &payload,
                    fleet,
                )?;
                validate_turn_commit_outcome_code(&receipt, Some(&code))?;
                let operation: OperationId = serde_json::from_str(&operation)
                    .map_err(|_| corrupt("invalid operation identity"))?;
                if operation
                    .scope
                    .session_id()
                    .is_some_and(|id| id != session_id)
                {
                    return Err(corrupt("operation belongs to another session"));
                }
                TurnChangeKind::Committed {
                    operation,
                    outcome: receipt
                        .outcome
                        .ok_or_else(|| corrupt("terminal has no outcome"))?,
                }
            }
            (None, Some(payload), None) => TurnChangeKind::SessionFault {
                record: Box::new(
                    super::SessionFault::from_stored(
                        session_id.clone(),
                        &payload,
                        recorded_at_ms as i64,
                    )?
                    .record,
                ),
            },
            (None, None, None) => TurnChangeKind::SessionDeleted,
            _ => return Err(corrupt("invalid terminal column family")),
        };
        Ok(Self {
            cursor: TurnChangeCursor(sequence),
            session_id,
            recorded_at_ms,
            kind,
        })
    }
}

impl crate::store::DurableRecord for RuntimeCommitReceipt {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::runtime_commit::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_identity_cannot_deserialize_half_populated() {
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation("partial-json-stamp"),
            "append-session-nodes",
        );
        let json = serde_json::json!({
            "operation": operation,
            "append_request_identity": {
                "kind": "append",
                "identity_encoding_version": 2,
                "requested_node_count": 1
            }
        });

        serde_json::from_value::<RuntimeTurnCommitStamp>(json)
            .expect_err("append identity without its hash must be refused");
    }

    #[test]
    fn semantic_boundary_identity_refuses_unknown_operations() {
        assert_eq!(
            SemanticBoundaryOperation::from_operation_key("append-session-nodes"),
            None,
            "append must never resolve to a semantic-boundary family"
        );
        serde_json::from_value::<AppendRequestIdentity>(serde_json::json!({
            "kind": "semantic_boundary",
            "operation": "initial-park",
            "identity_encoding_version": 1,
            "request_identity_hash": "boundary-hash",
        }))
        .expect_err("an unadopted operation tag must be refused at deserialization");
        serde_json::from_value::<AppendRequestIdentity>(serde_json::json!({
            "kind": "semantic_boundary",
            "operation": "record-config",
            "identity_encoding_version": 1,
        }))
        .expect_err("a semantic-boundary identity without its hash must be refused");
    }
}

/// A session head commit as the session actor carries it to the store (ADR
/// 0132 §4): a turn's under `turn.commit`, a session command's under
/// `session.command` (FIG-5230). The store applies it inside the owner's
/// fenced transaction, so the head moves with the turn's terminal or the
/// command's settlement, or not at all.
///
/// It crosses the durable port as text and is decoded in the same
/// transaction; it is never stored. The execution view and a frame switch
/// ride beside the commit because the commit's own encoding leaves them out.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionCommitEnvelope {
    commit: RuntimeCommit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_config: Option<Box<crate::PersistedSessionConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frame_transition: Option<FrameTransitionWire>,
}

/// A [`FrameTransition`] as the envelope carries it.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameTransitionWire {
    ended: FrameEnvironmentWire,
    successor: FrameEnvironmentWire,
    carries: Vec<crate::artifact_referrer::ArtifactName>,
    #[serde(with = "crate::artifact_referrer::journal_identity")]
    gate: lash_sansio::EffectJournalIdentity,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameEnvironmentWire {
    session_id: SessionId,
    frame_node_id: crate::FrameNodeId,
}

impl From<&crate::artifact_referrer::FrameEnvironmentId> for FrameEnvironmentWire {
    fn from(frame: &crate::artifact_referrer::FrameEnvironmentId) -> Self {
        Self {
            session_id: frame.session_id().clone(),
            frame_node_id: frame.frame_node_id().clone(),
        }
    }
}

impl From<FrameEnvironmentWire> for crate::artifact_referrer::FrameEnvironmentId {
    fn from(wire: FrameEnvironmentWire) -> Self {
        Self::new(wire.session_id, wire.frame_node_id)
    }
}

impl From<&FrameTransition> for FrameTransitionWire {
    fn from(transition: &FrameTransition) -> Self {
        Self {
            ended: (&transition.ended).into(),
            successor: (&transition.successor).into(),
            carries: transition.carries.clone(),
            gate: transition.gate.clone(),
        }
    }
}

impl From<FrameTransitionWire> for FrameTransition {
    fn from(wire: FrameTransitionWire) -> Self {
        Self {
            ended: wire.ended.into(),
            successor: wire.successor.into(),
            carries: wire.carries,
            gate: wire.gate,
        }
    }
}

/// Encode `commit` for the session actor's fenced head commit.
///
/// # Errors
///
/// [`StoreError::Backend`] when the commit carries a run terminal, which
/// the durable path records on the turn's own row, or does not encode.
pub fn encode_session_commit(commit: &RuntimeCommit) -> Result<String, StoreError> {
    if commit.run_terminal.is_some() {
        return Err(StoreError::Backend(
            "a session actor's head commit cannot carry a run terminal".to_owned(),
        ));
    }
    serde_json::to_string(&SessionCommitEnvelope {
        commit: commit.clone(),
        execution_config: commit.execution_config.clone(),
        frame_transition: commit.frame_transition.as_ref().map(Into::into),
    })
    .map_err(|error| StoreError::Backend(format!("the session commit does not encode: {error}")))
}

/// Decode a commit [`encode_session_commit`] encoded.
///
/// # Errors
///
/// [`StoreError::Backend`] when `encoded` is not one.
pub fn decode_session_commit(encoded: &str) -> Result<RuntimeCommit, StoreError> {
    let SessionCommitEnvelope {
        mut commit,
        execution_config,
        frame_transition,
    } = serde_json::from_str(encoded).map_err(|error| {
        StoreError::Backend(format!("the session commit does not decode: {error}"))
    })?;
    commit.execution_config = execution_config;
    commit.frame_transition = frame_transition.map(Into::into);
    Ok(commit)
}
