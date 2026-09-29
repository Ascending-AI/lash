use super::*;
use crate::session_model::SessionStreamEvent;
use crate::{SessionSnapshot, TokenUsage, ToolCallRecord};
use lash_sansio::sync::MutexExt;
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

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

#[derive(Clone)]
pub struct ProtocolSessionExtensionHandle(Arc<dyn ProtocolSessionExtension>);

impl ProtocolSessionExtensionHandle {
    /// Type-erases and shares a session extension for protocol implementors restoring plugin-owned
    /// session state.
    pub fn new(extension: impl ProtocolSessionExtension + 'static) -> Self {
        Self(Arc::new(extension))
    }

    /// Exposes the erased extension for protocol implementors that must downcast back to their
    /// concrete session-extension type.
    pub fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }
}

impl fmt::Debug for ProtocolSessionExtensionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtocolSessionExtensionHandle(..)")
    }
}

pub trait ProtocolSessionExtension: Send + Sync {
    fn as_any(&self) -> &dyn Any;
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
    pub assistant_output: AssistantOutput,
    pub execution: TurnExecutionMetrics,
    #[serde(default)]
    pub token_usage: TokenUsage,
    /// Provider calls made by this session during the turn, in protocol order.
    /// Child-session calls remain on the child turn result.
    #[serde(default)]
    pub llm_calls: Vec<crate::LlmCallRecord>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    /// Typed accounting for tool calls omitted from the bounded record view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<crate::OmittedToolCalls>,
    /// Bounded, non-transcript evidence retained when host charge-safety
    /// policy refuses regeneration of a failed provider generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<TurnFailureEvidence>,
    #[serde(default)]
    pub errors: Vec<TurnIssue>,
    /// Durable admission identity of the input this turn was driven from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_acceptance: Option<TurnInputAcceptanceReceipt>,
    /// Undelivered active-turn inputs repaired under this turn's cancellation policy.
    #[serde(
        default,
        skip_serializing_if = "crate::TurnCancelInputOutcome::is_empty"
    )]
    pub turn_cancel_input_outcome: crate::TurnCancelInputOutcome,
    /// The stopped turn's sealed and committed partial output: `Some`
    /// exactly when the turn's terminal is `TurnOutcome::Stopped(_)`
    /// (ADR 0114 §1.2). Data returned to the caller; it never enters the
    /// graph, `assistant_output` or history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_partial: Option<lash_sansio::StoppedPartial>,
}

/// Result of driving one logical host turn through any AgentFrame switches.
///
/// A frame switch is an internal runtime continuation, similar to compaction
/// from a host's perspective. Callers that need a final answer inspect
/// `final_turn()`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentFrameRun {
    pub turns: Vec<AssembledTurn>,
    /// Durable admission identity committed before this run was driven.
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

/// Termination policy knobs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TerminationPolicy {
    #[serde(default)]
    pub treat_missing_done_as_failure: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            treat_missing_done_as_failure: true,
        }
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

    /// The announcement that `summary`'s partial is durable (ADR 0114 §5.2).
    /// Its id derives from the partial's turn, so the turn's own terminal and
    /// a lost root's announcement name one activity, and a redelivery
    /// collapses into it.
    pub fn stopped_partial_available(summary: lash_sansio::StoppedPartialSummary) -> Self {
        let id = TurnActivityId::new(format!("{}:stopped_partial", summary.id.turn_id));
        Self {
            correlation_id: id.clone(),
            id,
            event: TurnEvent::StoppedPartialAvailable { summary },
        }
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
    QueuedWorkStarted {
        boundary: crate::AdmissionBoundary,
        batch_ids: Vec<String>,
        causes: Vec<crate::TurnCause>,
    },
    ModelRequestStarted {
        protocol_iteration: usize,
    },
    AssistantProseDelta {
        text: Arc<str>,
        /// Provider-minted identity of the assistant-text block this delta
        /// belongs to. The activity's `correlation_id` carries the same `id`.
        block: crate::llm::types::StreamBlockIdentity,
    },
    ReasoningDelta {
        text: Arc<str>,
        /// Provider-minted identity of the reasoning block this delta belongs
        /// to. The activity's `correlation_id` carries the same `id`.
        block: crate::llm::types::StreamBlockIdentity,
    },
    /// A provider-minted assistant-text or reasoning block opened. Hosts
    /// render one block per streamed unit — an OpenAI reasoning summary part,
    /// an Anthropic content block, or an ordinal run for providers with no
    /// native notion. Merging adjacent blocks is a host rendering choice;
    /// Lash injects no separators.
    StreamBlockStarted {
        kind: crate::llm::types::StreamBlockKind,
        block: crate::llm::types::StreamBlockIdentity,
    },
    /// A streamed block closed; `text` is the block's authoritative text, so
    /// hosts can correct drift from accumulated deltas.
    StreamBlockCompleted {
        kind: crate::llm::types::StreamBlockKind,
        block: crate::llm::types::StreamBlockIdentity,
        text: Arc<str>,
    },
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
    /// One chunk a running tool reported through its progress sink, published
    /// after the chunk was persisted to the turn's capture (ADR 0114 §2.2).
    ToolOutputProgress {
        call_id: String,
        chunk: lash_sansio::ToolOutputChunk,
    },
    /// A stopped turn's partial is durable: published after the commit that
    /// made it so, or after the lost-root write that sealed it (ADR 0114
    /// §4.3, §4.4). Identity and facts, never payload; the host reads the
    /// partial by its id.
    StoppedPartialAvailable {
        summary: lash_sansio::StoppedPartialSummary,
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
        success: bool,
        duration_ms: u64,
        tool_call_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolCallStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        /// `None` for top-level tool calls.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
        duration_ms: u64,
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        /// `None` for top-level tool calls.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolIntentOutcome {
        call_id: String,
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
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    PluginRuntime {
        plugin_id: String,
        event: crate::PluginRuntimeEvent,
    },
    QueuedInputAccepted {
        applications: Vec<crate::TurnInputApplication>,
    },
    QueuedMessagesCommitted {
        messages: Vec<crate::PluginMessage>,
        checkpoint: crate::CheckpointKind,
    },
    Error {
        message: String,
    },
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
    /// Bind the effect host whose scope fences and journal rows
    /// [`reclaim_retained_evidence`](Self::reclaim_retained_evidence) reads
    /// and retires. The facade binds the host its backend supplies. Where
    /// the journal lives is the backend's own wiring, fixed when the
    /// backend is opened; the binding carries no location.
    fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>);

    /// Count the deployment's turns that are not settled yet: parked turns
    /// and every turn in flight (FIG-3586). `drain_status` reads it, so a
    /// deployment with a parked turn — or one whose claims a crashed driver
    /// still holds — never reports drained. A store that keeps no countable
    /// catalog returns `StoreError::UnsupportedStoreOperation` rather than
    /// report zero: an inferred zero would let a host retire a deployment
    /// with turns still in flight.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError>;

    /// List the deployment's parked turns under `query` (FIG-3659), ordered by
    /// `(since_ms, session_id)` with `query.after` as the keyset. A parked
    /// turn is unfinished work a host must be able to enumerate across
    /// sessions; a store with no park ledger returns
    /// `StoreError::UnsupportedStoreOperation` rather than an empty page.
    async fn list_turn_parks(
        &self,
        query: &crate::store::TurnParkQuery,
    ) -> Result<Vec<crate::store::TurnPark>, crate::StoreError>;

    /// Read the durable turn park feed strictly after `after` (FIG-3659): one
    /// event per park transition, in commit order. A position below the
    /// compaction horizon fails with
    /// `StoreError::ParkFeedCursorCompacted`.
    async fn turn_park_feed(
        &self,
        after: crate::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::ParkFeedPage<crate::store::TurnParkTarget>, crate::StoreError>;

    /// Compact the turn park feed: events at or below `through` are removed
    /// and the cursor horizon advances to it. Host-gated — the feed's
    /// retention lever — never automatic (FIG-3659).
    async fn compact_turn_park_feed(
        &self,
        through: crate::store::ParkFeedCursor,
    ) -> Result<(), crate::StoreError>;

    /// Open logical roots in `(session, root)` order, after `after`. Recovery
    /// checks their engine runs in bounded pages; it never guesses liveness
    /// from a missing terminal row alone.
    async fn non_terminal_roots_page(
        &self,
        after: Option<&crate::engine::RootRef>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::engine::RootRef>, crate::StoreError>;

    /// End a root only after the engine proved its one workflow run failed
    /// terminally. The write settles the root's ingress and arms scope close
    /// atomically. An already terminal or deleted root is a no-op.
    async fn end_lost_root(
        &self,
        target: &crate::engine::RootRef,
        at_ms: u64,
    ) -> Result<Option<crate::store::RootTerminal>, crate::StoreError>;

    /// Retained intents, including permanent engine refusals, in ID order.
    async fn list_control_intents(
        &self,
        after: Option<crate::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::ControlIntent>, crate::StoreError>;

    /// Atomically refuse retirement while any closure names `scope`, otherwise
    /// persist the scope tombstone that every later authorization checks.
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), crate::StoreError>;

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
    /// The sweep is also the durable owner of deferred effect-scope
    /// retirement (ADR 0049, ADR 0067): every session-free runtime-operation
    /// scope whose operation has recorded its receipt and under which nothing
    /// is live any more (no effect in progress, no group still waiting on a
    /// child, no unresolved promise) is retired under the same fence the
    /// receipt-time retirement takes, in this same transaction. A receipt
    /// whose scope is still live is retained past the horizon so the proof
    /// survives until the scope can go. Stores whose effect journal lives
    /// elsewhere (the in-memory store, or a host that keeps its journal in
    /// its own engine) retire nothing here.
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
/// behalf, such as a Restate effect-group child, may be routed to a newer
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

/// Records the park of the turn a group tool child belongs to, when the child
/// refused where it parks its opener and recorded nothing (FIG-3725) — its
/// tool drifted and it would run live, or its replay diverged — and returns
/// it. The child runs in its own invocation on an engine whose opener cannot
/// learn of a refusal that settles nothing, so the child writes the park its
/// opener would have: keyed by the opener's logical root (D2 §1.3) — a turn
/// scope's root, a queue drain's queued root — in the root's own session. A
/// refusal that parks nothing, a scope with no root, and a session the
/// catalog does not hold live answer `None`.
pub async fn park_turn_of_refused_group_child(
    store: &dyn crate::store::RuntimeStore,
    scope: &crate::ExecutionScope,
    refusal: &crate::RuntimeError,
    at_ms: u64,
) -> Result<Option<crate::store::TurnPark>, crate::StoreError> {
    let Some(reason) = crate::store::ParkReason::of_error(refusal) else {
        return Ok(None);
    };
    let (session_id, root) = match (scope, scope.logical_root()) {
        (
            crate::ExecutionScope::Turn { session_id, .. }
            | crate::ExecutionScope::QueueDrain { session_id, .. },
            Some(root),
        ) => (session_id, root),
        _ => return Ok(None),
    };
    if !session_is_live(store, session_id).await? {
        return Ok(None);
    }
    let park = super::record_root_park(
        store,
        &crate::store::TurnParkWrite::refusal(session_id.clone(), root, reason, at_ms),
    )
    .await?;
    crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
    Ok(Some(park))
}

/// Records the park of the turn a durable engine redelivered to a build whose
/// generation gate refused its session (FIG-3735), and returns it. `store` is
/// the deployment's store, read without admission.
///
/// The gate refuses before any effect, so a redrive meets `refusal` before it
/// issues its first command. A turn the refused session holds in flight for
/// `scope` — an admitted, unfinished root for a queue-drain scope; for a
/// direct turn scope, the open input row the turn's journaled acceptance
/// wrote (its id is provisioned from the acceptance address) — was driven by
/// an earlier execution, whose journal already holds commands the refused
/// redrive cannot replay. Its handler must not return, or fail terminally,
/// where that journal holds its next command: the turn parks, typed
/// ([`ParkReason::SessionStateGenerationRefused`](crate::store::ParkReason::SessionStateGenerationRefused)),
/// keeps its claims for a build of its own generation, and its handler ends
/// the attempt as every parked turn does. A scope with nothing in flight
/// returns `None`: nothing ran before the refusal, and the refusal is the
/// turn's terminal answer.
pub async fn park_turn_refused_by_generation(
    store: &dyn crate::store::RuntimeStore,
    scope: &crate::ExecutionScope,
    refusal: crate::SessionStateVersionRefusal,
    at_ms: u64,
) -> Result<Option<crate::store::TurnPark>, crate::StoreError> {
    let session_id = match scope {
        crate::ExecutionScope::QueueDrain { session_id, .. }
        | crate::ExecutionScope::Turn { session_id, .. } => session_id,
        _ => return Ok(None),
    };
    let in_flight = match scope {
        crate::ExecutionScope::QueueDrain { .. } => store
            .unfinished_root(session_id)
            .await?
            .is_some_and(|unfinished| unfinished.root.as_str() == scope.id()),
        crate::ExecutionScope::Turn { turn_id, .. } => {
            let accepted = super::provisioned_turn_input_id(
                super::causal::turn_acceptance_effect_invocation(scope, session_id, turn_id)
                    .address(),
            );
            store
                .list_pending_turn_inputs(session_id)
                .await?
                .iter()
                .any(|read| read.input.input_id.as_str() == accepted)
        }
        _ => false,
    };
    if !in_flight {
        return Ok(None);
    }
    let park = super::record_root_park(
        store,
        &crate::store::TurnParkWrite::refusal(
            session_id.clone(),
            TurnId::from(scope.id()),
            crate::store::ParkReason::session_state_generation_refused(refusal),
            at_ms,
        ),
    )
    .await?;
    crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
    Ok(Some(park))
}
