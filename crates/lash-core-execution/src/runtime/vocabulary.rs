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
}

/// Result of driving one logical host turn through any AgentFrame switches.
///
/// A frame switch is an internal runtime continuation, similar to compaction
/// from a host's perspective. Callers that need a final answer can use
/// [`LashRuntime::stream_turn_with_agent_frames`] and inspect `final_turn()`.
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
        boundary: crate::QueuedWorkClaimBoundary,
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

#[async_trait::async_trait]
pub trait SessionStoreFactory: crate::AttachmentRootSet + Send + Sync {
    /// Bind the effect host whose scope fences and journal rows
    /// [`reclaim_retained_evidence`](Self::reclaim_retained_evidence) reads
    /// and retires. The facade binds the host its backend supplies. Where
    /// the journal lives is the backend's own wiring, fixed when the
    /// backend is opened; the binding carries no location.
    fn bind_effect_host(&self, _effect_host: &Arc<dyn crate::EffectHost>) {}

    /// Bind the exact artifact stores whose execution-owner cleanup this
    /// factory resumes from committed scope-retirement evidence.
    fn bind_artifact_stores(
        &self,
        _process_env_store: Arc<dyn ProcessExecutionEnvStore>,
        _process_engines: ProcessEngineRegistry,
    ) {
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::store::RuntimePersistence>, crate::StoreError>;

    async fn open_existing_store(
        &self,
        _request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, String> {
        Ok(None)
    }

    /// Read one settled session without acquiring its execution lease or
    /// exposing a persistence capability that can mutate it.
    ///
    /// The returned value is the same canonical [`crate::SessionReadView`]
    /// used by a live session. Implementations must not create, admit, bind,
    /// migrate, claim, renew, release, or otherwise write while answering this
    /// call. Factories without a read-only backend seam fail explicitly rather
    /// than falling back to [`Self::open_existing_store`].
    ///
    /// # Integrator class
    ///
    /// Store and durable-substrate implementors provide this capability for
    /// inspection hosts that must coexist with a live writer.
    async fn read_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<Option<crate::SessionReadView>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "read_session",
        })
    }

    /// Enumerate durable catalog rows without opening a session or acquiring
    /// execution authority.
    ///
    /// Results are ordered by `created_at_ms`, then `session_id`. Permanent
    /// deletion tombstones remain visible with `deleted == true`.
    async fn list_sessions(
        &self,
        _filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_sessions",
        })
    }

    /// Page the live sessions that hold open ingress, in `session_id` order
    /// strictly after `after`, at most `limit` of them, each with its oldest
    /// open item (ADR 0104 O2). A read of the one ingress table across the
    /// catalog: it opens no session and takes no fence. The reconcile sweep
    /// asks a drive for each.
    async fn sessions_with_open_ingress(
        &self,
        _after: Option<&SessionId>,
        _limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::OpenIngressSession>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "sessions_with_open_ingress",
        })
    }

    /// Count the deployment's turns that are not settled yet: parked turns
    /// and every turn in flight (FIG-3586). `drain_status` reads it, so a
    /// deployment with a parked turn — or one whose claims a crashed driver
    /// still holds — never reports drained.
    ///
    /// Required, with no default: every factory states its answer, so a
    /// factory that silently lacks one fails to compile rather than failing
    /// the first drain at runtime. A decorator forwards to the catalog it
    /// wraps. A factory that keeps no countable catalog returns
    /// `StoreError::UnsupportedStoreOperation` rather than report zero: an
    /// inferred zero would let a host retire a deployment with turns still in
    /// flight.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError>;

    /// List the deployment's parked turns under `query` (FIG-3659), ordered by
    /// `(since_ms, session_id)` with `query.after` as the keyset. A parked
    /// turn is unfinished work a host must be able to enumerate across
    /// sessions, so this is required with no default: a decorator forwards to
    /// the catalog it wraps, and a factory with no park ledger returns
    /// `StoreError::UnsupportedStoreOperation` rather than an empty page.
    async fn list_turn_parks(
        &self,
        query: &crate::store::TurnParkQuery,
    ) -> Result<Vec<crate::store::TurnPark>, crate::StoreError>;

    /// Read the durable turn park feed strictly after `after` (FIG-3659): one
    /// event per park transition, in commit order. A position below the
    /// compaction horizon fails with
    /// `StoreError::ParkFeedCursorCompacted`.
    ///
    /// Required, with no default, like [`Self::count_unsettled_turns`].
    async fn turn_park_feed(
        &self,
        after: crate::store::TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::TurnParkFeedPage, crate::StoreError>;

    /// Compact the turn park feed: events at or below `through` are removed
    /// and the cursor horizon advances to it. Host-gated — the feed's
    /// retention lever — never automatic (FIG-3659).
    ///
    /// Required, with no default, like [`Self::count_unsettled_turns`].
    async fn compact_turn_park_feed(
        &self,
        through: crate::store::TurnParkFeedCursor,
    ) -> Result<(), crate::StoreError>;

    /// Open an existing session when only its durable routing identity is
    /// known, without creating one.
    ///
    /// Required, with no default. This is the non-creating acquisition seam a
    /// **Durable Session** resolves through (ADR 0097), and its negative
    /// answers mean opposite things: `Ok(None)` is "this catalog has no such
    /// session", while `Err` is "this catalog cannot answer". An inherited
    /// `Ok(None)` collapses the second into the first, so every durable
    /// operation on a session that *does* exist would report it missing and
    /// send the host looking for it.
    ///
    /// The error is typed so the two failure shapes stay apart:
    /// `StoreError::UnsupportedStoreOperation` is the deterministic "this
    /// catalog has no by-id lookup" answer — a capability fact retrying
    /// cannot change, so consumers that strictly require the seam (session
    /// initialisation reopening a committed session, a `durable()` handle
    /// bound to a session id) fail fast instead of looping. Any other `Err`
    /// is a lookup failure a caller may retry. A factory with no by-id
    /// lookup returns the typed refusal; a decorator forwards to the
    /// catalog it wraps.
    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, crate::StoreError>;

    /// Read exact cancellation closure pins before session deletion or
    /// process-scope retirement. Implementors that cannot provide this
    /// lifecycle fence fail closed; callers must never infer an empty set from
    /// an unsupported inspection.
    async fn pending_turn_cancel_closure_pins(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::pending_turn_cancel_closure_pins",
        })
    }

    /// Atomically refuse retirement while any closure names `scope`, otherwise
    /// persist the scope tombstone that every later authorization checks.
    async fn retire_turn_cancel_closure_scope(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::retire_turn_cancel_closure_scope",
        })
    }

    /// Cheap durable read used to reject an idle queued-work notification
    /// before session state, plugins, and a runtime are hydrated.
    ///
    /// First-party factories override this at their database seam. `Some`
    /// reports a known durable answer; `None` means claimability is unknown and
    /// admits one conservative, successfully completed run. A transiently
    /// failed pass still receives the driver's finite retry ladder before the
    /// demand idles. Unknown must hydrate rather than silently strand durable
    /// work, but it must not become a permanently positive poll after that run
    /// drains nothing. The fallback never creates a session.
    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, crate::StoreError> {
        let Some(store) = self
            .open_existing_store(request)
            .await
            .map_err(crate::StoreError::Backend)?
        else {
            return Ok(None);
        };
        if store
            .list_pending_queued_work(&request.session_id)
            .await?
            .into_iter()
            .any(|batch| batch.available_at_ms <= now_epoch_ms)
        {
            return Ok(Some(true));
        }
        Ok(Some(
            store
                .list_pending_turn_inputs(&request.session_id)
                .await?
                .into_iter()
                .any(|read| read.input.state == crate::TurnInputState::DeferredNextTurn),
        ))
    }

    /// Required, with no default. This answer decides whether a resume returns
    /// the caller's conversation or a brand-new empty one under a dead id, so
    /// an inherited `false` is a factory claiming "no session was ever deleted
    /// here" without having been asked. A factory that keeps no tombstone says
    /// so explicitly; a decorator forwards to the store it wraps.
    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String>;

    /// Delete one session and reclaim blobs whose final exact reference edge is
    /// severed by that transaction.
    ///
    /// Failure carries the partial report accumulated before the transaction
    /// rolled back; a zero success report therefore means witnessed emptiness,
    /// never an unreported reclaim failure.
    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport>;

    /// Reclaim factory-wide evidence before an explicit host horizon (FIG-653).
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
        _bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport> {
        Err(crate::store::MaintenanceFailure::failed_before_any_work(
            crate::StoreError::UnsupportedStoreOperation {
                operation: "reclaim_retained_evidence",
            },
        ))
    }

    /// Retain the continuation checkpoint for `node_id`.
    ///
    /// A new pin can be created only while some live head is exactly at the
    /// node, because an unpinned past checkpoint is ordinarily already
    /// collectible. Re-pinning an existing point is idempotent.
    async fn pin(&self, node_id: &str) -> Result<ForkPoint, crate::StoreError> {
        let _ = node_id;
        Err(crate::StoreError::UnsupportedStoreOperation { operation: "pin" })
    }

    /// Release an explicit continuation pin. A live head at the same node
    /// continues to make that tip forkable.
    async fn unpin(&self, node_id: &str) -> Result<(), crate::StoreError> {
        let _ = node_id;
        Err(crate::StoreError::UnsupportedStoreOperation { operation: "unpin" })
    }

    /// Enumerate retained continuation points. This includes pinned past turns
    /// and unpinned live tips, de-duplicated by node id.
    async fn fork_points(&self) -> Result<Vec<ForkPoint>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "fork_points",
        })
    }

    /// Add a new session-head root at a retained point without writing graph nodes.
    async fn fork_at(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, crate::StoreError> {
        let _ = request;
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "fork_at",
        })
    }
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
    sessions: &dyn SessionStoreFactory,
    session_id: &SessionId,
) -> Result<(), crate::StoreError> {
    let Some(store) = sessions.open_existing_store_by_id(session_id).await? else {
        return Ok(());
    };
    match store.read_session_state_version().await {
        Ok(_) | Err(crate::StoreError::SessionDeleted { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Records the park of the turn a group tool child belongs to, when the child
/// refused where it parks its opener and recorded nothing (FIG-3725) — its
/// tool drifted and it would run live, or its replay diverged — and returns
/// it. The child runs in its own invocation on an engine whose opener cannot
/// learn of a refusal that settles nothing, so the child writes the park its
/// opener would have: keyed by the turn — a turn scope's root turn, a queue
/// drain's physical turn — in the turn's own session store. A refusal that
/// parks nothing, a scope with no turn, and a session with no store answer
/// `None`.
pub async fn park_turn_of_refused_group_child(
    sessions: &dyn SessionStoreFactory,
    scope: &crate::ExecutionScope,
    physical_turn: Option<&TurnId>,
    refusal: &crate::RuntimeError,
    at_ms: u64,
) -> Result<Option<crate::store::TurnPark>, crate::StoreError> {
    let Some(reason) = crate::store::ParkReason::of_error(refusal) else {
        return Ok(None);
    };
    let (session_id, turn_id) = match (scope, physical_turn) {
        (
            crate::ExecutionScope::Turn {
                session_id,
                turn_id,
            },
            _,
        ) => (session_id, turn_id),
        (crate::ExecutionScope::QueueDrain { session_id, .. }, Some(turn_id)) => {
            (session_id, turn_id)
        }
        _ => return Ok(None),
    };
    let Some(store) = sessions.open_existing_store_by_id(session_id).await? else {
        return Ok(None);
    };
    let park = store
        .record_turn_park(&crate::store::TurnParkWrite {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            reason,
            at_ms,
        })
        .await?;
    crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
    Ok(Some(park))
}

/// Records the park of the turn a durable engine redelivered to a build whose
/// generation gate refused its session (FIG-3735), and returns it. `store` is
/// the refused session's own store, opened without admission (a catalog's
/// [`open_existing_store_by_id`](SessionStoreFactory::open_existing_store_by_id)).
///
/// The gate refuses before any effect, so a redrive meets `refusal` before it
/// issues its first command. A turn the refused session holds in flight for
/// `scope` — a begun, unsettled queued run for a queue-drain scope; for a
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
    store: &dyn crate::store::RuntimePersistence,
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
            .queued_run(scope)
            .await?
            .is_some_and(|run| run.terminal.is_none()),
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
    let park = store
        .record_turn_park(&crate::store::TurnParkWrite {
            session_id: session_id.clone(),
            turn_id: TurnId::from(scope.id()),
            reason: crate::store::ParkReason::session_state_generation_refused(refusal),
            at_ms,
        })
        .await?;
    crate::operational_metrics::record_work_parked("turn", park.reason.code().as_str());
    Ok(Some(park))
}
