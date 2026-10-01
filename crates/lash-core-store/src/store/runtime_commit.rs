//! Runtime commit envelope and result types.

use super::{
    BlobRef, GraphAppend, HydratedSessionCheckpoint, OperationId, RealizedNodeTimestamp,
    SessionCheckpoint, StoreError, commit_identity,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
};
use crate::SessionId;
use crate::TurnId;

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
    /// The drive fence of the admission this commit's root was sealed under
    /// (ADR 0105 §2, FIG-3600 S7). A transaction predicate, never commit
    /// content: the backend refuses the commit
    /// [`StoreError::StaleDriveFence`](super::StoreError::StaleDriveFence)
    /// unless it is still the session's current drive fence, checked in the
    /// commit's own transaction before anything is written. `None` for a
    /// commit no drive sealed (a runtime operation, a process-scoped turn).
    #[serde(skip)]
    pub drive_fence: Option<Box<super::DriveFence>>,
    /// The logical root's terminal evidence, present exactly on the commit of
    /// the root's final physical turn (FIG-3600 S7): written in this commit's
    /// transaction, refused [`StoreError::RootAlreadyTerminal`](super::StoreError::RootAlreadyTerminal)
    /// when the root already ended otherwise. An instruction to the store
    /// derived from the turn it commits, never commit content: like the
    /// fences, it is excluded from the commit's serialized form.
    #[serde(skip)]
    pub root_terminal: Option<Box<super::RootTerminalWrite>>,
    /// The logical root this commit's physical turn runs under, whose park
    /// the commit clears in its own transaction (FIG-3600 S7, D2 §1.3 P3): a
    /// root is parked by its logical root, so any commit of any of its
    /// physical turns — a frame switch's follow-on, an S4 follow-on, the
    /// final turn — settles the park. `None` for a commit that runs under no
    /// root; see [`RuntimeCommit::settled_park_root`]. Like the fences, a
    /// store instruction excluded from the commit's serialized form.
    #[serde(skip)]
    pub park_root: Option<crate::TurnId>,
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
    /// The config the committing root ran under, when it is not the config
    /// the commit writes: a root runs under its recorded execution view and
    /// writes the head's sticky config back (FIG-3841). The view is the
    /// commit's content, so the commit identity covers it in place of
    /// [`Self::config`]; the sticky config is the head's, not the operation's,
    /// and may have moved since the root first committed, so a redrive that
    /// replays the root's committed operation still answers its receipt. An
    /// input to the identity, never stored: `None` when the two agree.
    #[serde(skip)]
    pub execution_config: Option<Box<crate::PersistedSessionConfig>>,
    pub current_frame_node_id: Option<crate::FrameNodeId>,
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
    pub turn_commit: RuntimeTurnCommitStamp,
    /// What this commit does with the rows its root admitted (FIG-3927):
    /// completions, releases and drops, each predicated on the row still
    /// being bound to the root. Requires [`Self::drive_fence`].
    ///
    /// A cancelled turn hands the work it withheld from its terminal
    /// checkpoint to its cancellation here (FIG-3531, FIG-3543): withheld
    /// input is released or dropped by the undelivered disposition, withheld
    /// wakes are always released, and the backend records each row on the
    /// cancellation's outcome beside `interrupted_turn_input_turn_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<super::IngressSettlement>,
    /// The session-command batches this commit applied (design §2.7). The
    /// command lane takes no admission: each row must still exist and be
    /// open, or the whole commit is refused
    /// [`StoreError::SessionCommandWithdrawn`](super::StoreError::SessionCommandWithdrawn).
    /// Requires [`Self::drive_fence`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_commands: Option<crate::QueuedWorkCompletion>,
    /// Each command's outcome, keyed by its batch, recorded atomically with
    /// the head revision compare-and-set and covered by the commit identity.
    /// Every key must belong to [`Self::applied_commands`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub command_outcomes: std::collections::BTreeMap<crate::BatchId, crate::SessionCommandOutcome>,
    /// The follow-on the head owes once this commit publishes (ADR 0101 §3):
    /// the value the head holds after the write, not a delta. A frame-switch
    /// commit writes it, the follow-on's terminal commit clears or replaces
    /// it, and every other commit carries the head's value unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow_on: Option<super::PendingFollowOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<TurnId>,
    /// Exact cancellation evidence returned by the authoritative turn gate.
    ///
    /// Absence explicitly selects ordinary non-cancellation re-deferral. Store
    /// implementations must never infer this decision from a request row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
    /// Transient predicate observed before the turn gate was settled. Backends
    /// compare it atomically before cancellation-dependent publication.
    #[serde(skip)]
    pub interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
    /// Exact pending closure authorization consumed atomically with a fresh
    /// cancellation-dependent commit. Receipt replay is adjudicated first and
    /// may consume only the same exact still-pending authorization.
    #[serde(skip)]
    pub turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
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
                TurnStop::ToolFailure => Self::Failed(TurnCommitFailureCause::ToolFailure),
                TurnStop::ProviderError => Self::Failed(TurnCommitFailureCause::ProviderError),
                TurnStop::ContextOverflow => Self::Failed(TurnCommitFailureCause::ContextOverflow),
                TurnStop::PluginAbort => Self::Failed(TurnCommitFailureCause::PluginAbort),
                TurnStop::RuntimeError => Self::Failed(TurnCommitFailureCause::RuntimeError),
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
    /// The follow-on the head owes after this commit (ADR 0101 §3), so a
    /// replayed switch commit returns the fact it wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow_on: Option<super::PendingFollowOn>,
    /// Each command batch's outcome, recorded with the applying commit so
    /// later resubmissions answer their own result.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[schemars(with = "std::collections::BTreeMap<String, serde_json::Value>")]
    pub command_outcomes: std::collections::BTreeMap<crate::BatchId, crate::SessionCommandOutcome>,
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
    /// it is stored as `false` in the receipt itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipt_replayed: bool,
}

/// The durable `result_json` receipt schema this build writes.
///
/// Version 1 is the first stamped encoding. Receipts written before the field
/// existed carry no `schema_version` at all and are refused as
/// [`StoreError::MissingRecordSchemaVersion`], matching the exact-version
/// refusal every other durable record follows.
///
/// Version 2 (FIG-3542) replaces the frame-handoff `enqueued_queue_batches`
/// with the `pending_follow_on` the commit left on the head. A version-1
/// receipt is refused, not converted.
///
/// version_guard(
///     items(RuntimeCommitReceipt, decode_runtime_commit_receipt, ensure_supported_receipt_version),
///     items(
///         path = "crates/lash-core-store/src/store/runtime_commit_plan.rs",
///         RuntimeCommitReceiptRecord,
///     ),
///     items(path = "crates/lash-core-store/src/store/pending_follow_on.rs", PendingFollowOn),
///     items(path = "crates/lash-core-store/src/session_graph.rs", RealizedNodeTimestamp),
///     items(
///         path = "crates/lash-core-store/src/turn_failure_evidence.rs", TurnFailureEvidence,
///         TurnFailurePartialOutput, ChargeSafetyRefusalEvidence,
///     ),
///     items(path = "crates/lash-core-store/src/queued_work_vocabulary.rs", QueuedWorkBatch),
///     items(path = "crates/lash-core-store/src/turn_input_vocabulary.rs", TurnInputApplication),
///     items(
///         path = "crates/lash-core-store/src/turn_control_vocabulary.rs", TurnCancelInputOutcome,
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
pub const RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: u32 = 2;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 2's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
pub const RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: u32 = 3;

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
    /// The root whose park this commit clears: [`Self::park_root`], else the
    /// root whose end it records, else the physical turn it commits (a turn
    /// that runs under no root parks under its own id).
    #[must_use]
    pub fn settled_park_root(&self) -> Option<&crate::TurnId> {
        self.park_root
            .as_ref()
            .or_else(|| self.root_terminal.as_deref().map(|terminal| &terminal.root))
            .or_else(|| self.turn_commit.operation.turn_id())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_outcome_follows_every_terminal_stop_kind() {
        use lash_sansio::{TurnOutcome, TurnStop};
        let cases = [
            (TurnStop::Incomplete, TurnCommitFailureCause::Incomplete),
            (TurnStop::InvalidInput, TurnCommitFailureCause::InvalidInput),
            (TurnStop::MaxTurns, TurnCommitFailureCause::MaxTurns),
            (TurnStop::ToolFailure, TurnCommitFailureCause::ToolFailure),
            (
                TurnStop::ProviderError,
                TurnCommitFailureCause::ProviderError,
            ),
            (
                TurnStop::ContextOverflow,
                TurnCommitFailureCause::ContextOverflow,
            ),
            (TurnStop::PluginAbort, TurnCommitFailureCause::PluginAbort),
            (TurnStop::RuntimeError, TurnCommitFailureCause::RuntimeError),
            (
                TurnStop::SubmittedError {
                    value: serde_json::Value::Null,
                },
                TurnCommitFailureCause::SubmittedError,
            ),
            (
                TurnStop::ToolError {
                    tool_name: "tool".into(),
                    value: serde_json::Value::Null,
                },
                TurnCommitFailureCause::ToolError,
            ),
        ];
        for (stop, cause) in cases {
            let stored = TurnCommitOutcome::from_terminal(&TurnOutcome::Stopped(stop));
            assert_eq!(stored, TurnCommitOutcome::Failed(cause));
            assert!(stored.as_str().starts_with("failed_"));
            let encoded = serde_json::to_value(&stored).expect("serialize stored outcome");
            assert!(encoded.get("failed").is_some());
            assert_eq!(
                serde_json::from_value::<TurnCommitOutcome>(encoded)
                    .expect("decode stored outcome"),
                stored
            );
        }
        assert_eq!(
            TurnCommitOutcome::from_terminal(&TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: lash_sansio::TurnCancellationEvidence::internal("outcome-test"),
            })),
            TurnCommitOutcome::Cancelled,
        );
        assert_eq!(
            TurnCommitOutcome::from_terminal(&TurnOutcome::Finished(
                lash_sansio::TurnFinish::AssistantMessage {
                    text: String::new()
                },
            )),
            TurnCommitOutcome::Completed,
        );
    }

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
    fn semantic_boundary_wire_values_match_the_persisted_receipt_encoding() {
        // Hand-spelled wire literals: this vocabulary is persisted replay
        // identity, and a serde-rename drift would be globally self-consistent
        // while silently orphaning every stored receipt.
        for (operation, wire_operation) in [
            (SemanticBoundaryOperation::RecordConfig, "record-config"),
            (SemanticBoundaryOperation::CreateSession, "create-session"),
        ] {
            let identity = AppendRequestIdentity::SemanticBoundary {
                operation,
                encoding_version: 1,
                request_hash: "boundary-hash".to_string(),
            };
            let encoded = serde_json::to_value(&identity).expect("encode semantic identity");
            assert_eq!(
                encoded,
                serde_json::json!({
                    "kind": "semantic_boundary",
                    "operation": wire_operation,
                    "identity_encoding_version": 1,
                    "request_identity_hash": "boundary-hash",
                }),
                "semantic-boundary wire shape moved for {wire_operation}"
            );
            let decoded: AppendRequestIdentity =
                serde_json::from_value(encoded).expect("decode semantic identity");
            assert_eq!(decoded, identity, "operation tag must round-trip");
            assert_eq!(operation.operation_key(), wire_operation);
            assert_eq!(
                SemanticBoundaryOperation::from_operation_key(wire_operation),
                Some(operation)
            );
        }
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
