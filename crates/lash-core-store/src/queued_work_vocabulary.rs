//! Durable queued-work vocabulary.
//!
//! The queue's durable rows, their admission and completion payloads and the
//! session-command family that rides in them. The runtime's queue driver
//! stays in `lash-core`; only the data it persists lives here.

use crate::SessionId;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCommand {
    // No generation guard: the command drains asynchronously, so any
    // generation observed at enqueue time may legitimately have advanced by
    // drain time, and the refresh recomputes the surface from live sources
    // regardless — a guard could only fail spuriously.
    RefreshToolCatalog {
        reason: String,
    },
    /// An administrative compaction (FIG-4201): the command drain summarizes
    /// the frame current at the boundary and opens a compaction frame seeded
    /// with the summary, under the command run's sealed fence. It applies
    /// only at a turn boundary, so the bound turn owns the head until it
    /// ends. It settles as a [`CompactContextOutcome`].
    CompactContext {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// A host's append to the session graph (FIG-4202). The bound turn owns
    /// the session head, so a host append is a command the shift applies at
    /// a turn boundary, against the boundary's resident head: its nodes land
    /// after everything the bound turn committed. The request's
    /// `operation_id` is the command's idempotency key. It settles as a
    /// [`SessionCommandOutcome::AppendSessionNodes`]: appended, or refused
    /// because its required ancestor left the active path.
    AppendSessionNodes {
        #[schemars(with = "serde_json::Value")]
        request: Box<crate::session_append::AppendSessionNodesRequest>,
    },
    /// A host's plugin command (FIG-4202). The plugin's code runs only once
    /// the shift admits the command, at a turn boundary, under the command
    /// run's fence; its events, state and queued turns commit with the
    /// command's settlement. It settles as a
    /// [`SessionCommandOutcome::PluginOperation`].
    RunPluginCommand {
        name: String,
        #[schemars(with = "serde_json::Value")]
        args: serde_json::Value,
    },
    /// A host's plugin task (FIG-4202): a tool-bearing operation that runs
    /// as its own operation run (K8, FIG-4888), whose invocation journals
    /// its effects under the command's own scope and owns them until they
    /// drained, so a redrive of the unsettled command replays them. It
    /// settles as a
    /// [`SessionCommandOutcome::PluginOperation`].
    RunPluginTask {
        name: String,
        #[schemars(with = "serde_json::Value")]
        args: serde_json::Value,
    },
    /// A typed config transaction (FIG-4379): its commands resolve once, at
    /// a turn boundary, into a recorded resolution, and the commit that
    /// settles the command publishes it with one config revision step. It
    /// settles as a [`SessionCommandOutcome::ConfigTransaction`].
    ApplyConfigTransaction {
        transaction: Box<crate::ConfigTransactionRecord>,
    },
    /// A host's durable frame open (FIG-4202): the shift opens the frame at
    /// a turn boundary, in the commit that settles the command, and restarts
    /// its live interpreter from the frame's seed. It settles as a
    /// [`SessionCommandOutcome::OpenAgentFrame`].
    OpenAgentFrame {
        #[schemars(with = "serde_json::Value")]
        request: Box<crate::OpenAgentFrameRequest>,
    },
    /// A host's tool-state change (FIG-5134). A session builds its tool
    /// registry only when a run publishes its plugin transition, so the
    /// change is durable first and the command run applies it, in lane
    /// order, against the capabilities that run built. It settles as a
    /// [`SessionCommandOutcome::ToolState`].
    ChangeToolState {
        change: Box<crate::tool_state::ToolStateChange>,
    },
}
impl SessionCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RefreshToolCatalog { .. } => "refresh_tool_catalog",
            Self::CompactContext { .. } => "compact_context",
            Self::AppendSessionNodes { .. } => "append_session_nodes",
            Self::RunPluginCommand { .. } => "run_plugin_command",
            Self::RunPluginTask { .. } => "run_plugin_task",
            Self::OpenAgentFrame { .. } => "open_agent_frame",
            Self::ApplyConfigTransaction { .. } => "apply_config_transaction",
            Self::ChangeToolState { .. } => "change_tool_state",
        }
    }

    /// Whether the command applies alone, under its own scope, and settles
    /// with a typed [`SessionCommandOutcome`] in the commit that applies it
    /// (FIG-4201, FIG-4202, FIG-4379). A catalog refresh settles without
    /// one.
    pub fn settles_with_outcome(&self) -> bool {
        match self {
            Self::RefreshToolCatalog { .. } => false,
            Self::CompactContext { .. }
            | Self::AppendSessionNodes { .. }
            | Self::RunPluginCommand { .. }
            | Self::RunPluginTask { .. }
            | Self::OpenAgentFrame { .. }
            | Self::ApplyConfigTransaction { .. }
            | Self::ChangeToolState { .. } => true,
        }
    }

    pub fn source_key(&self, idempotency_key: impl AsRef<str>) -> String {
        format!("command:{}:{}", self.kind(), idempotency_key.as_ref())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCommandReceipt {
    pub session_id: SessionId,
    pub batch_id: crate::BatchId,
    pub source_key: String,
}
impl std::fmt::Display for SessionCommandReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "batch `{}` for session `{}`",
            self.batch_id, self.session_id
        )
    }
}
/// The semantically distinct outcomes of config-command submission. Rejection
/// happens before durable queue acceptance; `Durable` means the command's
/// completion and session head committed together. `Pending` preserves an
/// accepted receipt when the configured settlement deadline expires, while
/// `Cancelled` reports a queued command withdrawn before that commit.
/// `Stale` is the config patch's typed non-application (FIG-3541): the
/// command settled — durably — without changing the config.
#[derive(Clone, Debug)]
pub enum SessionCommandSettlement {
    Rejected(crate::RuntimeError),
    Durable(SessionCommandReceipt),
    Pending(SessionCommandReceipt),
    Cancelled(SessionCommandReceipt),
    /// A command that settles with a typed outcome was applied: its commit
    /// completed the command, and `outcome` is what it settled as, a typed
    /// refusal included (FIG-4201, FIG-4202).
    Applied {
        receipt: SessionCommandReceipt,
        outcome: SessionCommandOutcome,
    },
}

/// What a command that settles with an outcome settled as (FIG-4201,
/// FIG-4202). It is written with the commit that completes the command and
/// read back from that commit's receipt, so a submitter on any runtime, or
/// one reattaching by the command's receipt, sees the same answer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SessionCommandOutcome {
    /// An administrative compaction's outcome.
    CompactContext { outcome: CompactContextOutcome },
    /// A host append's outcome: its nodes landed, or its required ancestor
    /// left the active path and nothing was written.
    AppendSessionNodes {
        outcome: crate::session_append::AppendSessionNodesOutcome,
    },
    /// A host plugin command's or task's outcome.
    PluginOperation {
        outcome: PluginOperationCommandOutcome,
    },
    /// A host frame open's outcome.
    OpenAgentFrame {
        outcome: OpenAgentFrameCommandOutcome,
    },
    /// A config transaction's outcome: applied with each command's output,
    /// stale, or refused by an owner (FIG-4379).
    ConfigTransaction {
        outcome: crate::ConfigTransactionOutcome,
    },
    /// A host tool-state change's outcome: applied, restored with its
    /// report, or refused against the session's tool state (FIG-5134).
    ToolState {
        outcome: crate::tool_state::ToolStateChangeOutcome,
    },
    /// The command could not apply, for a reason its own outcome does not
    /// name: nothing of it committed, and the command is settled, so it is
    /// never applied again and the lane never waits on it. It keeps the
    /// refusal with its typed cause (FIG-5391).
    Failed {
        refusal: crate::runtime_error::RecordedRefusal,
    },
}

/// How a host plugin command or task the command lane applied settled
/// (FIG-4202).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginOperationCommandOutcome {
    /// The operation ran: its output, the runtime events its plugin emitted
    /// (each with the id of the plugin that owns it) and the turn inputs it
    /// queued. Its events and plugin state committed with the settlement.
    Completed {
        plugin_id: String,
        output: serde_json::Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<lash_sansio::PluginRuntimeEvent>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending_turn_inputs: Vec<crate::PendingTurnInput>,
    },
    /// The operation failed: nothing of it committed, and the command is
    /// settled, so it is never applied again.
    Failed {
        failure: Box<lash_sansio::PluginOperationFailure>,
    },
    /// Queued input was refused at admission. The refusal and its typed
    /// cause are retained for the submitting host, and none of the
    /// operation's inputs were admitted.
    Refused {
        refusal: crate::runtime_error::RecordedRefusal,
    },
    /// A host cancelled the task after a shift admitted it, and its shift
    /// found the cancel requested before or once the task's code returned
    /// (FIG-4391, FIG-4453): nothing of the task committed, and the command
    /// is settled, so it is never applied again.
    Cancelled,
}

/// How a host frame open the command lane applied settled (FIG-4202).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpenAgentFrameCommandOutcome {
    /// The open was accepted: a new frame opened, or the key named the
    /// current frame and the open replayed it.
    Opened {
        outcome: crate::OpenAgentFrameOutcome,
    },
    /// The open was refused and nothing of it committed: the key named a
    /// historical frame, the seed carried artifacts a host open cannot hand
    /// over, or a follow-on owns the frame.
    Refused {
        refusal: crate::runtime_error::RecordedRefusal,
    },
}

/// How an administrative compaction the command lane applied settled
/// (FIG-4201). It is written with the commit that completes the command and
/// read back from that commit's receipt, so a submitter on any runtime sees
/// the same answer.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactContextOutcome {
    /// The compaction opened its frame; the session now stands in it.
    Opened { frame_node_id: crate::FrameNodeId },
    /// The compactor found nothing to compact. No frame opened.
    NothingToCompact,
    /// The compaction failed. No frame opened, and the command is settled:
    /// it is never applied again.
    Failed {
        refusal: crate::runtime_error::RecordedRefusal,
    },
}

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    EarliestSafeBoundary,
    AfterCurrentTurnCommit,
}
impl DeliveryPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EarliestSafeBoundary => "earliest_safe_boundary",
            Self::AfterCurrentTurnCommit => "after_current_turn_commit",
        }
    }

    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "earliest_safe_boundary" => Some(Self::EarliestSafeBoundary),
            "after_current_turn_commit" => Some(Self::AfterCurrentTurnCommit),
            _ => None,
        }
    }
}
/// Producer-stamped execution authority for queued work.
///
/// Both fields are opaque to Lash and belong to the row that carries them.
/// Lash applies no authorization policy: composition does not compare
/// authorities, and rows with different principals may share a turn unless
/// the host's [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) stops the
/// drain at a principal change (ADR 0101 §5.2). Keeping this separate from
/// `merge_key` keeps a grouping label from becoming an authority encoding.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct QueuedWorkAuthority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation: Option<String>,
}
impl QueuedWorkAuthority {
    /// Stamps queued work with an opaque principal and no elevation override.
    pub fn new(principal: impl Into<String>) -> Self {
        Self {
            principal: Some(principal.into()),
            elevation: None,
        }
    }

    /// Adds or replaces the opaque elevation override used by the work.
    pub fn with_elevation(mut self, elevation: impl Into<String>) -> Self {
        self.elevation = Some(elevation.into());
        self
    }
}
/// Complete admission-time bounds and drain policy passed to durable store
/// implementations.
#[derive(Clone, Debug)]
pub struct TurnLaneAdmissionPolicy {
    pub max_context_tokens: usize,
    pub action_token_reserve: usize,
    pub max_rows: usize,
    pub max_pending_age_ms: u64,
    pub drain_policy: std::sync::Arc<dyn crate::QueuedDrainPolicy>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedWorkPayload {
    SessionCommand { command: Box<SessionCommand> },
}
impl QueuedWorkPayload {
    pub fn session_command(command: SessionCommand) -> Self {
        Self::SessionCommand {
            command: Box::new(command),
        }
    }

    /// The session command this payload carries.
    pub fn command(&self) -> &SessionCommand {
        match self {
            Self::SessionCommand { command } => command,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueuedWorkBatch {
    pub batch_id: crate::BatchId,
    pub session_id: SessionId,
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub delivery_policy: DeliveryPolicy,
    pub authority: QueuedWorkAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub enqueued_at_ms: u64,
    pub payload: QueuedWorkPayload,
    /// The immutable digest admission recorded
    /// ([`QueuedWorkBatchDraft::submission_digest`]): a resubmission under the
    /// same source key must carry it (ADR 0101 §8).
    pub submission_digest: String,
    /// The batch's terminal tombstone, `None` while it is open or admitted.
    /// A tombstone holds no admission binding and is never selected again;
    /// it stays until host vacuum (ADR 0101 §8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<crate::store::IngressTerminal>,
    /// What caused the batch, as its first enqueue retained it
    /// ([`QueuedWorkBatchDraft::trace_cause`]).
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    pub trace_cause: lash_trace::TraceCause,
}
/// Receiver-side result of an idempotent queued-work enqueue.
#[derive(Clone, Debug)]
pub enum QueuedWorkEnqueueOutcome {
    Inserted(QueuedWorkBatch),
    Existing(QueuedWorkBatch),
}
impl QueuedWorkEnqueueOutcome {
    pub fn batch(&self) -> &QueuedWorkBatch {
        match self {
            Self::Inserted(batch) | Self::Existing(batch) => batch,
        }
    }

    pub fn into_batch(self) -> QueuedWorkBatch {
        match self {
            Self::Inserted(batch) | Self::Existing(batch) => batch,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedWorkBatchDraft {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub delivery_policy: DeliveryPolicy,
    pub authority: QueuedWorkAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub payload: QueuedWorkPayload,
    /// What caused the batch, for telemetry: its producer's context. The
    /// enqueue that inserts the batch retains it beside the row; an absorbed
    /// re-enqueue reads the retained cause back. It is no part of the
    /// submission digest.
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    pub trace_cause: lash_trace::TraceCause,
}
impl QueuedWorkBatchDraft {
    pub fn new(
        session_id: impl Into<SessionId>,
        delivery_policy: DeliveryPolicy,
        payload: impl Into<QueuedWorkPayload>,
    ) -> Self {
        let payload = payload.into();
        Self {
            session_id: session_id.into(),
            source_key: None,
            delivery_policy,
            authority: QueuedWorkAuthority::default(),
            merge_key: None,
            payload,
            trace_cause: lash_trace::TraceCause::Root,
        }
    }

    /// Sets what caused the batch ([`Self::trace_cause`]).
    pub fn with_trace_cause(mut self, trace_cause: lash_trace::TraceCause) -> Self {
        self.trace_cause = trace_cause;
        self
    }

    pub fn with_source_key(mut self, source_key: impl Into<String>) -> Self {
        self.source_key = Some(source_key.into());
        self
    }

    pub fn with_authority(mut self, authority: QueuedWorkAuthority) -> Self {
        self.authority = authority;
        self
    }

    /// Assign a non-empty producer-selected merge key.
    ///
    /// # Panics
    ///
    /// Panics when the supplied key is empty.
    pub fn with_merge_key(mut self, merge_key: impl Into<String>) -> Self {
        let merge_key = merge_key.into();
        assert!(
            !merge_key.is_empty(),
            "queued-work merge key must be non-empty"
        );
        self.merge_key = Some(merge_key);
        self
    }

    /// The digest this draft is admitted and compared under (ADR 0101 §8).
    ///
    /// A session command's submission is its command with the delivery
    /// policy, authority and merge key its producer chose. The preimage is the `lash.queued-work-submission` identity family at
    /// [`QUEUED_WORK_SUBMISSION_FAMILY_VERSION`]; the rendered form is
    /// `queued-work-submission:v<family>:blake3:<hex>`.
    pub fn submission_digest(&self) -> Result<String, serde_json::Error> {
        Ok(crate::stable_identity::rendered_hash(
            "queued-work-submission",
            QUEUED_WORK_SUBMISSION_FAMILY_VERSION,
            &queued_work_submission_preimage(self)?,
        ))
    }

    /// Stored references carried by typed queued payloads, sorted and deduplicated.
    pub fn stored_attachment_ids(&self) -> Vec<crate::AttachmentId> {
        match &self.payload {
            QueuedWorkPayload::SessionCommand { .. } => Vec::new(),
        }
    }
}
/// The queued-work submission identity family's current version.
/// version_surface = "coexist"
/// version_guard(items(QUEUED_WORK_SUBMISSION_FAMILY_VERSION, queued_work_submission_preimage, submission_digest))
pub const QUEUED_WORK_SUBMISSION_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for the queued-work submission preimage.
///
/// Payload: 2 session command (one canonical JSON payload leaf), 4 config
/// transaction (its id and digest); tag 1, the process wake, is retired. The
/// batch then appends tag 3 with its delivery policy
/// (1 `earliest_safe_boundary`, 2 `after_current_turn_commit`), authority and
/// merge key. An optional field is tag 0 when absent and tag 1 and its value
/// when present. Retired tags remain burned.
fn queued_work_submission_preimage(
    draft: &QueuedWorkBatchDraft,
) -> Result<Vec<u8>, serde_json::Error> {
    fn optional_string(
        identity: &mut crate::stable_identity::IdentityEncoder,
        value: Option<&str>,
    ) {
        match value {
            Some(value) => {
                identity.tag(1);
                identity.string(value);
            }
            None => identity.tag(0),
        }
    }
    fn authority(
        identity: &mut crate::stable_identity::IdentityEncoder,
        authority: &QueuedWorkAuthority,
    ) {
        optional_string(identity, authority.principal.as_deref());
        optional_string(identity, authority.elevation.as_deref());
    }
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.queued-work-submission",
        QUEUED_WORK_SUBMISSION_FAMILY_VERSION,
    );
    let payload = &draft.payload;
    match payload {
        // A config transaction is the request its submitter wrote, never
        // the reducer identities ingress stamped on it: a resubmission
        // from another build asks the same thing.
        QueuedWorkPayload::SessionCommand { command } => match command.as_ref() {
            SessionCommand::ApplyConfigTransaction { transaction } => {
                identity.tag(4);
                identity.string(&transaction.id);
                identity.string(&transaction.digest()?);
            }
            command => {
                identity.tag(2);
                identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
                    command,
                )?));
            }
        },
    }

    identity.tag(3);
    identity.tag(match draft.delivery_policy {
        DeliveryPolicy::EarliestSafeBoundary => 1,
        DeliveryPolicy::AfterCurrentTurnCommit => 2,
    });
    authority(&mut identity, &draft.authority);
    optional_string(&mut identity, draft.merge_key.as_deref());
    Ok(identity.finish())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionBoundary {
    ActiveTurnCheckpoint,
    Idle,
}
/// The queued-work batches one commit completes (FIG-3927): row
/// identities only, settled under the committing run's admission.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkCompletion {
    pub session_id: SessionId,
    pub batch_ids: Vec<crate::BatchId>,
}
/// Queued-work batches one admission bound to a run, with their payloads,
/// in `enqueue_seq` order (FIG-3927). The binding lives on the rows; this
/// is what the run executes and what its journal records.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AdmittedQueuedWork {
    pub session_id: SessionId,
    pub batches: Vec<QueuedWorkBatch>,
}
impl AdmittedQueuedWork {
    pub fn completion(&self) -> QueuedWorkCompletion {
        QueuedWorkCompletion {
            session_id: self.session_id.clone(),
            batch_ids: self.batch_ids(),
        }
    }

    /// The admitted batches' ids, in admission order.
    pub fn batch_ids(&self) -> Vec<crate::BatchId> {
        self.batches
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Extracts the sole exclusive session command for queued-work driver implementors.
    pub fn exclusive_session_command(&self) -> Option<(&QueuedWorkBatch, &SessionCommand)> {
        if self.batches.len() != 1 {
            return None;
        }
        let batch = self.batches.first()?;
        Some((batch, batch.payload.command()))
    }

    /// Extract every independently receipted command in a coalesced
    /// session-command run. Every batch remains exactly one control item;
    /// only the enclosing commit is shared.
    pub fn session_commands(&self) -> Option<Vec<(&QueuedWorkBatch, &SessionCommand)>> {
        let mut commands = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            commands.push((batch, batch.payload.command()));
        }
        (!commands.is_empty()).then_some(commands)
    }
}
impl From<SessionCommand> for QueuedWorkPayload {
    fn from(command: SessionCommand) -> Self {
        Self::session_command(command)
    }
}
impl crate::store::DurableRecord for QueuedWorkBatch {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::runtime_commit::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION);
}
