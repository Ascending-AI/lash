use super::*;
use crate::session_model::SessionStreamEvent;
use crate::{LlmUsage, SessionSnapshot, ToolCallRecord};
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// Scope-keyed internal instrumentation, with session fallback for frames.
/// Explicitly unstable; see `docs/architecture/turn-phase-probe.md`.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct RuntimeTurnPhaseProbeSlot {
    probes: Arc<StdMutex<HashMap<crate::SessionScopeId, Arc<dyn RuntimeTurnPhaseProbe>>>>,
}

impl RuntimeTurnPhaseProbeSlot {
    pub fn set_for_session(
        &self,
        session_id: impl Into<SessionId>,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) {
        self.set_for_scope(&crate::SessionScope::new(session_id), probe);
    }

    pub fn set_for_scope(
        &self,
        scope: &crate::SessionScope,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) {
        self.probes.lock_recover().insert(scope.id(), probe);
    }

    pub fn get_for_scope(
        &self,
        scope: &crate::SessionScope,
    ) -> Option<Arc<dyn RuntimeTurnPhaseProbe>> {
        let probes = self.probes.lock_recover();
        probes.get(&scope.id()).cloned().or_else(|| {
            probes
                .get(&crate::SessionScope::new(&scope.session_id).id())
                .cloned()
        })
    }
}

/// A protocol's session-scoped extension, recorded durably (FIG-5134): the
/// session nodes it appends, which its protocol applies when the append lands
/// and replays on every restore. A host applies it through the session's
/// command lane, so it survives every rebuild of the session's capabilities.
#[derive(Clone)]
pub struct ProtocolSessionExtension(
    Arc<dyn Fn(crate::FleetFormat) -> Vec<crate::SessionAppendNode> + Send + Sync>,
);

impl ProtocolSessionExtension {
    /// An extension whose durable form is the nodes `nodes` writes for the
    /// session's fleet format.
    pub fn new(
        nodes: impl Fn(crate::FleetFormat) -> Vec<crate::SessionAppendNode> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(nodes))
    }

    /// The nodes that record this extension in a session of `fleet`.
    pub fn session_nodes(&self, fleet: crate::FleetFormat) -> Vec<crate::SessionAppendNode> {
        (self.0)(fleet)
    }
}

impl fmt::Debug for ProtocolSessionExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtocolSessionExtension(..)")
    }
}

/// Code execution output observed during a turn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeOutputRecord {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Canonical high-level turn result returned to hosts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssembledTurn {
    pub state: SessionSnapshot,
    /// Cancellation evidence, when the turn was cancelled, rides this outcome
    /// — see [`crate::TurnOutcome::cancellation`].
    pub outcome: crate::TurnOutcome,
    pub execution: TurnExecutionMetrics,
    #[serde(default)]
    pub token_usage: LlmUsage,
    /// Provider calls made by this session during the turn, in protocol order.
    /// Child-session calls remain on the child turn result.
    #[serde(default)]
    pub llm_calls: Vec<crate::LlmCallRecord>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    /// Typed accounting for tool calls omitted from the bounded record view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<crate::OmittedToolCalls>,
    /// Outputs the turn's code cells retained out of history (FIG-1643):
    /// history keeps each one's witness and reference, and the turn's commit
    /// holds each referenced attachment for the session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_outputs: Vec<crate::RetainedOutput>,
    /// Bounded, non-transcript evidence retained when host charge-safety
    /// policy refuses regeneration of a failed provider generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<TurnFailureEvidence>,
    #[serde(default)]
    pub errors: Vec<TurnIssue>,
    /// Durable admission identity of the input this turn was executed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_acceptance: Option<TurnInputAcceptanceReceipt>,
    /// Undelivered active-turn inputs repaired under this turn's cancellation policy.
    #[serde(
        default,
        skip_serializing_if = "crate::TurnCancelInputOutcome::is_empty"
    )]
    pub turn_cancel_input_outcome: crate::TurnCancelInputOutcome,
}

/// Result of executing one logical host turn through any AgentFrame switches.
///
/// A frame switch is an internal runtime continuation, similar to compaction
/// from a host's perspective. Callers that need a final answer inspect
/// `final_turn()`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentFrameRun {
    pub turns: Vec<AssembledTurn>,
    /// Durable admission identity committed before this run was executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<TurnInputAcceptanceReceipt>,
}

impl AgentFrameRun {
    pub fn final_turn(&self) -> Option<&AssembledTurn> {
        self.turns.last()
    }

    pub fn into_final_turn(mut self) -> Option<AssembledTurn> {
        self.turns.pop()
    }

    pub fn frame_switch_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| matches!(turn.outcome, crate::TurnOutcome::AgentFrameSwitch { .. }))
            .count()
    }
}

/// Host application sink for low-level streaming runtime events.
/// `SessionStreamEvent` is protocol-specific preview/progress data.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, event: SessionStreamEvent);
}

/// No-op sink useful for callers that only care about final state.
pub struct NoopEventSink;

/// Static no-op event sink for callers that need a `&dyn EventSink` default.
pub static NOOP_EVENT_SINK: NoopEventSink = NoopEventSink;

#[async_trait::async_trait]
impl EventSink for NoopEventSink {
    fn is_noop(&self) -> bool {
        true
    }

    async fn emit(&self, _event: SessionStreamEvent) {}
}

/// App-facing semantic activity emitted during a turn.
///
/// `id` is unique per emitted activity event. `correlation_id` groups related
/// events in the same logical activity, such as code start/completion, tool
/// start/completion, or text deltas from one output block.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnActivity {
    pub id: TurnActivityId,
    pub correlation_id: TurnActivityId,
    #[serde(flatten)]
    pub event: TurnEvent,
}

impl TurnActivity {
    pub fn new(correlation_id: TurnActivityId, event: TurnEvent) -> Self {
        Self {
            id: TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
            correlation_id,
            event,
        }
    }

    pub fn independent(event: TurnEvent) -> Self {
        let correlation_id = TurnActivityId::new(uuid::Uuid::new_v4().to_string());
        Self::new(correlation_id, event)
    }
}

/// App-facing semantic event payload for a turn activity.
///
/// Unlike [`SessionStreamEvent`], these events are stable application signals rather
/// than low-level runtime/debug events. Public streams carry these payloads
/// inside [`TurnActivity`] so every emitted item has identity.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// justification: public turn events are transient stream DTOs kept inline for allocation-free emission and stable pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum TurnEvent {
    /// Announces the physical turn identity before any other activity from
    /// that turn.
    ///
    /// Hosts use this identity for exact cancellation and steering targets;
    /// they must not infer it from whichever incidental activity happens to
    /// arrive first. Session-observation envelopes also carry `turn_id`, but
    /// the payload keeps it available to turn-local and collected streams.
    TurnStarted {
        turn_id: TurnId,
    },
    /// The run's construction restored the session's persisted tool state
    /// and some persisted id had no registered source (FIG-5134). It
    /// follows the run's first `TurnStarted`; a clean restore emits nothing.
    /// Under `ToolSourcePolicy::Tolerate` the run goes on without the lost
    /// members; under `Require` a run that would lose one is refused with
    /// `RuntimeErrorCode::ToolSourcesUnavailable` instead and never starts.
    ToolRestoreReported {
        report: crate::ToolRestoreReport,
    },
    ModelRequestStarted {
        protocol_iteration: usize,
    },
    /// The checkpoint for this protocol iteration was recorded. All earlier
    /// deltas on this turn's live lane precede that checkpoint.
    CheckpointRecorded {
        protocol_iteration: usize,
    },
    /// One step of a streamed assistant-text or reasoning block: the same
    /// payload the session stream, the provider stream and the trace carry.
    /// Hosts render one block per streamed unit — an OpenAI reasoning summary
    /// part, an Anthropic content block, or an ordinal run for providers with
    /// no native notion. The activity's `correlation_id` names the block
    /// within its model call's stream.
    StreamBlock(crate::llm::types::StreamBlockEvent),
    /// Marks a provider generation boundary before a retry and retracts any
    /// visible text emitted by the superseded attempt.
    ///
    /// Observers remove only prose and reasoning deltas whose correlation ids
    /// appear here. The reset is itself replayed in order, so reconnecting
    /// observers converge on the same visible text as live observers. Empty
    /// correlation lists mean the provider regenerated before emitting visible
    /// output; they remain boundary evidence and never mean "retract all."
    ModelAttemptReset {
        assistant_prose_correlation_ids: Vec<TurnActivityId>,
        reasoning_correlation_ids: Vec<TurnActivityId>,
    },
    /// A sealed per-call attempt ledger, including provider-reported evidence.
    ModelCallRecorded {
        record: crate::LlmCallRecord,
    },
    CodeBlockStarted {
        language: String,
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    CodeBlockCompleted {
        language: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<crate::CellFailure>,
        duration_ms: u64,
        tool_call_ids: Vec<crate::ToolCallId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolCallStarted {
        /// Lash's identity for the call (ADR 0117).
        call_id: crate::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolCallCompleted {
        /// Lash's identity for the call (ADR 0117).
        call_id: crate::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
        duration_ms: u64,
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolIntentOutcome {
        call_id: crate::ToolCallId,
        outcome: crate::ToolIntentExecutionOutcome,
    },
    FinalValue {
        value: serde_json::Value,
    },
    ToolValue {
        tool_name: String,
        value: serde_json::Value,
    },
    Usage {
        protocol_iteration: usize,
        usage: LlmUsage,
        cumulative: LlmUsage,
    },
    /// A retry that is about to wait, with the failure being retried: the
    /// session stream's `RetryStatus` payload.
    RetryStatus(crate::RetryProgress),
    PluginRuntime {
        plugin_id: String,
        event: crate::PluginRuntimeEvent,
    },
    QueuedInputAccepted {
        applications: Vec<crate::TurnInputApplication>,
    },
    /// A reported failure with its typed envelope: the session stream's
    /// `Error` payload.
    Error(crate::ReportedFailure),
}

#[async_trait::async_trait]
pub trait TurnActivitySink: Send + Sync {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, activity: TurnActivity);

    /// Sinks that only consume turn-local activity can keep implementing
    /// [`emit`](Self::emit). Observation sinks override this method to carry
    /// turn identity on their enclosing event without adding it to
    /// [`TurnActivity`].
    async fn emit_for_turn(&self, turn_id: &TurnId, activity: TurnActivity) {
        let _ = turn_id;
        self.emit(activity).await;
    }
}

pub struct NoopTurnActivitySink;

/// Static no-op turn-activity sink for callers that need a `&dyn TurnActivitySink` default.
pub static NOOP_TURN_ACTIVITY_SINK: NoopTurnActivitySink = NoopTurnActivitySink;

#[async_trait::async_trait]
impl TurnActivitySink for NoopTurnActivitySink {
    fn is_noop(&self) -> bool {
        true
    }

    async fn emit(&self, _activity: TurnActivity) {}
}

/// The deployment's store: the multi-session [`RuntimeStore`](crate::store::RuntimeStore)
/// plus the operations that span the deployment's catalog (ADR 0112 §2).
///
/// A host, the facade and the engine hold `Arc<dyn DeploymentStore>`.
/// Runtime code holds a [`SessionStore`](crate::store::SessionStore), which
/// reaches the same object as an `Arc<dyn RuntimeStore>` by trait upcasting.
///
/// Every operation is required, with no default: a store states its answer,
/// and a decorator forwards to the store it wraps
/// ([`DeploymentStoreDecorator`](super::DeploymentStoreDecorator)).
#[async_trait::async_trait]
pub trait DeploymentStore:
    crate::store::RuntimeStore + crate::AttachmentRootSet + crate::store::ControlIntentStore
{
    /// Count the deployment's turns that are not settled yet: parked turns
    /// and every turn in flight (FIG-3586). `drain_status` reads it, so a
    /// deployment with a parked turn — or one whose claims a crashed driver
    /// still holds — never reports drained. A store that keeps no countable
    /// catalog returns `StoreError::UnsupportedStoreOperation` rather than
    /// report zero: an inferred zero would let a host retire a deployment
    /// with turns still in flight.
    /// Whether a committed frame remains rooted by a head, anchor or retained
    /// admission. Prepared frames that never committed answer false.
    async fn artifact_frame_is_retained(
        &self,
        frame: &crate::FrameEnvironmentId,
    ) -> Result<bool, crate::StoreError>;

    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError>;

    /// Durable turn and session terminals strictly after `after`, in commit
    /// order. Independent of live replay; pages return the retention horizon.
    /// A cursor behind that horizon refuses as `TurnChangeCursorPruned`.
    async fn turns_changed_since(
        &self,
        after: crate::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::TurnChangePage, crate::StoreError>;

    /// Retained intents, including permanent engine refusals, in ID order.
    async fn list_control_intents(
        &self,
        after: Option<crate::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::ControlIntent>, crate::StoreError>;

    /// Reclaim deployment-wide evidence before an explicit host horizon
    /// (FIG-653).
    ///
    /// Receipts require a durably deleted owning session. Usage deltas require
    /// that same terminal marker and absence of their receipt after the sweep;
    /// live-session ledgers and receipts are never eligible. Receipt deletion
    /// and dependent attachment/usage reconciliation share one transaction fence.
    /// A retry in a deleted scope returns `StoreError::SessionDeleted`, before
    /// and after receipt pruning. The permanent identity tombstone is exempt.
    /// No daemon, clock read or live policy lookup runs this operation.
    ///
    /// The host tool-intent submission ledger is evidence under it too
    /// (FIG-1509): a row admitted before the bound is reclaimed once its owner
    /// session is durably deleted, and that owner is fenced in the same
    /// transaction, so a resubmission is refused rather than realized again.
    ///
    /// SQL stores reclaim their retained storage evidence here. This sweep
    /// does not retire engine scopes or decide whether an execution is live.
    async fn reclaim_retained_evidence(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport>;
}

/// Whether the catalog holds `session_id` live: durable metadata and no
/// deletion tombstone. A catalog that cannot answer is `Err`, never `false`.
pub async fn session_is_live(
    store: &dyn crate::store::RuntimeStore,
    session_id: &SessionId,
) -> Result<bool, crate::StoreError> {
    Ok(matches!(
        store.lookup_session(session_id).await?,
        crate::store::SessionLookup::Live(_)
    ))
}

/// The view of `session_id` on `store` when the catalog holds it live, and
/// `None` when it is absent or deleted (ADR 0112 §2: `lookup_session` plus
/// `SessionStore::new`). A catalog that cannot answer is `Err`.
pub async fn live_session_view(
    store: &Arc<dyn crate::DeploymentStore>,
    session_id: &SessionId,
) -> Result<Option<crate::store::SessionStore>, crate::StoreError> {
    match store.lookup_session(session_id).await? {
        crate::store::SessionLookup::Live(_) => {
            let runtime: Arc<dyn crate::store::RuntimeStore> = store.clone();
            crate::store::SessionStore::new(runtime, session_id.clone()).map(Some)
        }
        crate::store::SessionLookup::Deleted | crate::store::SessionLookup::Absent => Ok(None),
    }
}

/// Admit `request`'s session on the catalog and return its view (ADR 0112
/// §2: `admit_session` plus `SessionStore::new`).
pub async fn admit_session_view(
    store: &Arc<dyn crate::DeploymentStore>,
    request: &crate::SessionStoreCreateRequest,
) -> Result<crate::store::SessionStore, crate::StoreError> {
    store.admit_session(request).await?;
    let runtime: Arc<dyn crate::store::RuntimeStore> = store.clone();
    crate::store::SessionStore::new(runtime, request.session_id.clone())
}

/// The session-state generation gate for work that acts for a session from
/// outside that session's execution lease (FIG-3619).
///
/// A turn meets the generation fence when it claims its lane
/// ([`SessionCommitStore::admit_session_state`](crate::store::SessionCommitStore::admit_session_state)).
/// Work a durable engine runs as a separate invocation on the session's
/// behalf, such as a process segment, may be routed to a newer
/// deployment than the turn that opened it, and never claims that lane. It
/// calls this at invocation entry, before it reads a journal or derives an
/// effect key: the owning session's physical marker is classified by the
/// same backend read the lease admission uses, so both paths refuse exactly
/// the same generations and a generation bump moves them together.
///
/// Refusal is `Err(StoreError::SessionStateVersionUnsupported)` or
/// `Err(StoreError::SessionStateVersionNewerThanRuntime)`, with the found
/// generation. A session the catalog does not hold, or one that was deleted,
/// passes: there is no generation to refuse, and the deletion fences own that
/// work's fate. Any other error is the catalog's or the store's, and the
/// caller decides whether to retry it.
pub async fn admit_session_state_generation(
    store: &dyn crate::store::RuntimeStore,
    session_id: &SessionId,
) -> Result<(), crate::StoreError> {
    if !session_is_live(store, session_id).await? {
        return Ok(());
    }
    match store.read_session_state_version(session_id).await {
        Ok(_) | Err(crate::StoreError::SessionDeleted { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}
