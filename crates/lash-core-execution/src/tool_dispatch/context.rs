use crate::SessionId;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;

use crate::plugin::{
    PluginSession, SessionGraphService, SessionLifecycleService, SessionStateService,
};
use crate::{
    PreparedToolCall, ToolCallRecord, ToolCatalog, ToolFailure, ToolFailureClass, ToolOutcome,
    ToolProvider,
};

#[derive(Clone, Default)]
pub struct CheckpointMessageBuffer {
    queue: Arc<Mutex<Vec<crate::PluginMessage>>>,
}

impl CheckpointMessageBuffer {
    pub fn enqueue(&self, messages: Vec<crate::PluginMessage>) {
        let mut queue = self.queue.lock_recover();
        queue.extend(messages);
    }

    pub fn drain(&self) -> Vec<crate::PluginMessage> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolTriggerEffectOutcome {
    pub source_type: String,
    pub source_key: String,
    pub occurrence_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    pub deliveries: Vec<crate::TriggerDeliveryEmitReceipt>,
}

#[derive(Clone, Default)]
pub struct ToolTriggerOutcomeBuffer {
    queue: Arc<Mutex<Vec<ToolTriggerEffectOutcome>>>,
}

impl ToolTriggerOutcomeBuffer {
    pub fn enqueue(&self, outcome: ToolTriggerEffectOutcome) {
        let mut queue = self.queue.lock_recover();
        queue.push(outcome);
    }

    pub fn drain(&self) -> Vec<ToolTriggerEffectOutcome> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
    }
}

/// What an orchestrating group child's body leaves for its driver outside any
/// attempt frame: the processes it realized, captured at the start's journal
/// boundary (ADR 0099 §6), and the refusal a nested call met.
///
/// An orchestrating child runs *outside* an attempt frame, so a process its
/// body starts never appears in a `ToolIntentExecutionOutcome` — the channel
/// [`ToolSettlement`](crate::runtime::ToolSettlement) possession reads from.
/// The orchestrating `InternalProcessAdmin` enqueues each started id here at
/// the moment the durable start succeeds, and the child's driver drains it
/// into the settlement's possession, so replay serves the same possession set
/// the live run produced.
///
/// A nested call the body issued whose attempt or deferred await a controller
/// refused — a replay divergence against its record, a live journal fault —
/// answers the body a failure, but the refusal is kept here too: the driver
/// refuses the child with it instead of settling whatever the body made of
/// that failure, so neither the divergence nor the fault reaches the model as
/// the child's result (FIG-3679).
#[derive(Clone, Default)]
pub struct OrchestratingChildSinks {
    queue: Arc<Mutex<Vec<crate::ProcessId>>>,
    refusal: Arc<Mutex<Option<crate::RuntimeEffectControllerError>>>,
}

impl OrchestratingChildSinks {
    pub(crate) fn enqueue(&self, process_id: crate::ProcessId) {
        let mut queue = self.queue.lock_recover();
        queue.push(process_id);
    }

    pub(crate) fn drain(&self) -> Vec<crate::ProcessId> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
    }

    /// Keeps the first refusal a nested call met.
    pub(crate) fn refuse(&self, error: crate::RuntimeEffectControllerError) {
        self.refusal.lock_recover().get_or_insert(error);
    }

    /// The refusal a nested call met, if any.
    pub(crate) fn take_refusal(&self) -> Option<crate::RuntimeEffectControllerError> {
        self.refusal.lock_recover().take()
    }
}

#[derive(Clone)]
pub struct ToolDispatchContext<'run> {
    pub plugins: Arc<PluginSession>,
    pub tools: Arc<dyn ToolProvider>,
    pub tool_registry: Option<Arc<crate::ToolRegistry>>,
    pub tool_catalog: Arc<ToolCatalog>,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub trigger_router: Option<crate::TriggerRouter>,
    /// Durable home for the named process-definition registry (FIG-2995).
    /// Unset only in fixtures that exercise no registration intent.
    pub process_definitions: Option<Arc<dyn crate::ProcessDefinitionRegistry>>,
    /// The engines a definition registration resolves against.
    pub process_engines: crate::ProcessEngineRegistry,
    pub effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    /// The resolved key one call's observation lanes are emitted under
    /// (ADR 0105 §1): when set it *replaces* the resolved base — the
    /// invocation the dispatch serves or the scope's journal identity — so a
    /// call that shares its dispatch's base with siblings still mints
    /// distinct `(key, ordinal)` identities when a model repeats a `call_id`.
    /// Install through [`Self::observation_keyed`], which qualifies caller
    /// material under this dispatch's base; a group child passes its own
    /// `{group}:child:{position}` replay key verbatim, and the
    /// turn-dispatched protocol path passes `{iteration}:{index}:{call_id}`.
    /// `None` everywhere else: a dispatch that serves one call — an attempt's,
    /// a command's — already keys uniquely through `parent_invocation`.
    pub observation_call_key: Option<String>,
    pub execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub session_id: SessionId,
    pub agent_frame_id: crate::FrameNodeId,
    /// The turn's observation sink (ADR 0105 §1): every host-facing event a
    /// dispatch emits is a synchronous [`ObservationSink::observe`] call,
    /// keyed by replay key and ordinal, and never awaited. A group child
    /// borrows the opener's so its nested calls surface the same
    /// `ToolCallStarted`/`ToolCallCompleted` activities a turn-dispatched call
    /// emits (ADR 0099 §3's live half of the split); a dispatch that serves no
    /// turn stream carries [`NullObservationSink`](crate::engine::NullObservationSink).
    pub observer: Arc<dyn crate::engine::ObservationSink>,
    pub checkpoint_messages: CheckpointMessageBuffer,
    pub trigger_outcomes: ToolTriggerOutcomeBuffer,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    pub turn_context: crate::TurnContext,
    pub clock: Arc<dyn crate::Clock>,
    /// The lineage of the process this dispatch runs inside, when it runs
    /// inside one (FIG-3607 R1): a start an intent realizes records it above
    /// its starter.
    pub process_lineage: Option<crate::ProcessLineage>,
}

impl ToolDispatchContext<'_> {
    pub fn is_orchestrating_tool(&self, tool_id: &crate::ToolId) -> bool {
        self.tool_registry
            .as_deref()
            .is_some_and(|registry| registry.is_orchestrating_tool(tool_id))
    }

    pub(crate) fn attempt_may_defer(
        &self,
        tool_id: &crate::ToolId,
        grant: Option<&crate::ToolExecutionGrant>,
    ) -> bool {
        // A registry's pinned catalog deliberately omits out-of-catalog grant
        // routes. Only the live source named by the grant can declare deferral;
        // an unresolved route must not borrow the answer from a same-id catalog
        // tool. Direct non-registry providers retain their ordinary lookup.
        if let Some(grant) = grant
            && let Some(registry) = self.tool_registry.as_deref()
        {
            return registry.attempt_may_defer_for_grant(tool_id, grant.source_id.as_deref());
        }
        self.tools.attempt_may_defer(tool_id)
    }

    /// The replay-key base this dispatch's observation lanes key under: the
    /// invocation the dispatch serves when it carries one, else the admitted
    /// scope's journal identity — deterministic for a given dispatch, so a
    /// replay keys the same observations identically (ADR 0105 §1).
    pub fn observation_base_key(&self) -> String {
        if let Some(key) = self
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::replay_key)
        {
            return key.to_owned();
        }
        let scoped = self.effect_controller.scoped();
        let scope = scoped.execution_scope();
        debug_assert!(
            scope.journal_identity().is_ok(),
            "dispatch on scope `{}` names no journal identity for its observation base",
            scope.id(),
        );
        scope
            .journal_identity()
            .map(|identity| identity.key().to_owned())
            .unwrap_or_else(|_| format!("dispatch:{}:{}", scope.id(), self.session_id))
    }

    /// This dispatch with one call's observation key installed:
    /// `material` qualified under this dispatch's *resolved* key — its
    /// installed call key when it carries one, else its base — so sibling
    /// calls that share a base mint distinct lanes, and a call nested inside
    /// an orchestrating body's own keyed call nests under it rather than
    /// colliding with a sibling of the outer call. Pass the call's own
    /// effect-invocation replay key where it has one — a group child's
    /// `{group}:child:{position}` envelope, a command's key — else positional
    /// material the caller can prove unique: `{iteration}:{index}:{call_id}`
    /// on the turn-dispatched protocol path, `{batch_id}:{index}:{call_id}`
    /// inside a batch, `{index}:{call_id}` inside a nested orchestration.
    /// Buffers and services stay shared; only emissions re-key.
    pub fn observation_keyed(&self, call_material: impl Into<String>) -> Self {
        let mut keyed = self.clone();
        keyed.observation_call_key = Some(format!(
            "{}:call:{}",
            self.observation_call_key
                .clone()
                .unwrap_or_else(|| self.observation_base_key()),
            call_material.into()
        ));
        keyed
    }

    /// A fresh observation cursor for one emission lane of this dispatch —
    /// `lane` keeps sibling lanes distinct under one base key. A dispatch
    /// carrying a per-call key ([`Self::observation_keyed`]) resolves it
    /// instead of the base, so every lane the call emits — directive folds,
    /// stream events, activities — lands under the call's own key.
    pub fn observation_cursor(&self, lane: &str) -> crate::engine::ObservationCursor {
        let base = self
            .observation_call_key
            .clone()
            .unwrap_or_else(|| self.observation_base_key());
        crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new(format!(
            "{base}:{lane}"
        )))
    }

    /// Attribution available without a causal parent comes only from the
    /// admitted execution scope. `CurrentSession` also hosts process and
    /// runtime-operation work, so its descriptive session id is not provenance
    /// for those sessionless scopes.
    pub(crate) fn parentless_attribution(&self) -> crate::RuntimeAttribution {
        self.effect_controller
            .scoped()
            .execution_scope()
            .session_id()
            .map(crate::RuntimeAttribution::for_session)
            .unwrap_or_else(crate::RuntimeAttribution::none)
    }
}

/// Version of the tool-child rebind checklist below (ADR 0099 section 3).
///
/// Bump this when [`REBIND_FIELDS`] or the [`RebindField`]/[`RebindDisposition`]
/// vocabulary changes: the list is the contract every tool-child driver rebinds
/// a lent opener context against, so an edit that slips by unnoticed is a field
/// a child can inherit under the wrong opener's authority.
pub const TOOL_CHILD_REBIND_VERSION: u16 = 4;

/// Where a tool child's value for one [`ToolDispatchContext`] field comes from
/// (ADR 0099 section 3).
///
/// The checklist is deliberately closed: a field is one of these three, and the
/// meta-test in `tool_dispatch/tests/rebind_checklist.rs` refuses a field that
/// arrives unclassified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebindDisposition {
    /// Rebound to the child — taken from its recorded request or rebuilt under
    /// the child's own admitted authority. The lent value is unreachable in the
    /// child's context.
    Rebound,
    /// Lent from the live opener: deployment wiring and live channels a request
    /// deliberately does not record.
    Lent,
    /// A fresh child-local instance: neither lent nor recorded, so nothing the
    /// opener accumulated can leak into the child's settlement.
    Fresh,
}

/// One field of [`ToolDispatchContext`], as the rebind checklist names it.
///
/// The enum exists so a fixture — or a mutant — can name one field of the
/// context rather than a line of one driver's rebind. [`REBIND_FIELDS`] carries
/// every variant exactly once; adding a field to `ToolDispatchContext` without
/// adding its ruling here fails the completeness meta-test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RebindField {
    Plugins,
    Tools,
    ToolRegistry,
    ToolCatalog,
    Sessions,
    SessionLifecycle,
    SessionGraph,
    Processes,
    TriggerRouter,
    ProcessDefinitions,
    ProcessEngines,
    EffectController,
    DirectCompletions,
    ParentInvocation,
    ObservationCallKey,
    ExecutionEnvSpec,
    SessionId,
    AgentFrameId,
    Observer,
    CheckpointMessages,
    TriggerOutcomes,
    AttachmentStore,
    AttachmentSourcePolicy,
    TurnContext,
    Clock,
    ProcessLineage,
}

impl RebindField {
    /// The [`ToolDispatchContext`] field this ruling covers.
    #[must_use]
    pub const fn context_field(self) -> &'static str {
        match self {
            Self::Plugins => "plugins",
            Self::Tools => "tools",
            Self::ToolRegistry => "tool_registry",
            Self::ToolCatalog => "tool_catalog",
            Self::Sessions => "sessions",
            Self::SessionLifecycle => "session_lifecycle",
            Self::SessionGraph => "session_graph",
            Self::Processes => "processes",
            Self::TriggerRouter => "trigger_router",
            Self::ProcessDefinitions => "process_definitions",
            Self::ProcessEngines => "process_engines",
            Self::EffectController => "effect_controller",
            Self::DirectCompletions => "direct_completions",
            Self::ParentInvocation => "parent_invocation",
            Self::ObservationCallKey => "observation_call_key",
            Self::ExecutionEnvSpec => "execution_env_spec",
            Self::SessionId => "session_id",
            Self::AgentFrameId => "agent_frame_id",
            Self::Observer => "observer",
            Self::CheckpointMessages => "checkpoint_messages",
            Self::TriggerOutcomes => "trigger_outcomes",
            Self::AttachmentStore => "attachment_store",
            Self::AttachmentSourcePolicy => "attachment_source_policy",
            Self::TurnContext => "turn_context",
            Self::Clock => "clock",
            Self::ProcessLineage => "process_lineage",
        }
    }

    /// The disposition a child's context assigns this field.
    #[must_use]
    pub const fn disposition(self) -> RebindDisposition {
        match self {
            // The recorded request is authoritative for what the child was
            // admitted under: its catalog manifest, its lineage, its session
            // and frame attribution, its environment and its own admitted
            // controller. The completion client's transport is lent, but the
            // ledger it reports into is the child's, so the value as a whole
            // is rebound rather than lent.
            Self::ToolCatalog
            | Self::EffectController
            | Self::DirectCompletions
            | Self::ParentInvocation
            | Self::ExecutionEnvSpec
            | Self::SessionId
            | Self::AgentFrameId => RebindDisposition::Rebound,
            // Facts that ride the child's own outcome: a buffer the opener
            // filled would smuggle the opener's pending facts into the child's
            // settlement. The observation call key is likewise call-scoped —
            // inherited, it would key the child's lanes under a call that is
            // not theirs.
            Self::CheckpointMessages | Self::TriggerOutcomes | Self::ObservationCallKey => {
                RebindDisposition::Fresh
            }
            // Everything else is deployment wiring and live channels, which
            // section 3 puts on the lent side of the split.
            Self::Plugins
            | Self::Tools
            | Self::ToolRegistry
            | Self::Sessions
            | Self::SessionLifecycle
            | Self::SessionGraph
            | Self::Processes
            | Self::TriggerRouter
            | Self::ProcessDefinitions
            | Self::ProcessEngines
            | Self::Observer
            | Self::AttachmentStore
            | Self::AttachmentSourcePolicy
            | Self::TurnContext
            | Self::Clock => RebindDisposition::Lent,
            // The lineage of the process the opener's body runs inside is
            // lent with the opener: request validation makes the opener and
            // the enclosing process one fact, so a live opener's lineage is
            // the child's. A context the deployment built carries none, and a
            // start the child makes reads the enclosing process's recorded
            // lineage back from its row (FIG-3607 R2).
            Self::ProcessLineage => RebindDisposition::Lent,
        }
    }
}

/// Every [`ToolDispatchContext`] field's rebind ruling, in declaration order.
///
/// This is the single list a tool-child driver answers to: for each entry the
/// child's context either carries the recorded value, borrows the live one, or
/// holds a fresh instance — and the two-opener oracle generates its fixtures
/// from it, so a field added here is a field the differential cannot forget.
pub const REBIND_FIELDS: &[RebindField] = &[
    RebindField::Plugins,
    RebindField::Tools,
    RebindField::ToolRegistry,
    RebindField::ToolCatalog,
    RebindField::Sessions,
    RebindField::SessionLifecycle,
    RebindField::SessionGraph,
    RebindField::Processes,
    RebindField::TriggerRouter,
    RebindField::ProcessDefinitions,
    RebindField::ProcessEngines,
    RebindField::EffectController,
    RebindField::DirectCompletions,
    RebindField::ParentInvocation,
    RebindField::ObservationCallKey,
    RebindField::ExecutionEnvSpec,
    RebindField::SessionId,
    RebindField::AgentFrameId,
    RebindField::Observer,
    RebindField::CheckpointMessages,
    RebindField::TriggerOutcomes,
    RebindField::AttachmentStore,
    RebindField::AttachmentSourcePolicy,
    RebindField::TurnContext,
    RebindField::Clock,
    RebindField::ProcessLineage,
];

impl<'run> ToolDispatchContext<'run> {
    pub fn process_scope(&self) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.effect_controller.scoped())
            .with_parent_invocation(self.parent_invocation.clone())
            .with_agent_frame_id(Some(self.agent_frame_id.clone()))
            .with_process_lineage(self.process_lineage.clone())
    }

    pub(crate) fn to_static(&self) -> Option<ToolDispatchContext<'static>> {
        Some(ToolDispatchContext {
            plugins: Arc::clone(&self.plugins),
            tools: Arc::clone(&self.tools),
            tool_registry: self.tool_registry.clone(),
            tool_catalog: Arc::clone(&self.tool_catalog),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            session_graph: Arc::clone(&self.session_graph),
            processes: Arc::clone(&self.processes),
            trigger_router: self.trigger_router.clone(),
            process_definitions: self.process_definitions.clone(),
            process_engines: self.process_engines.clone(),
            effect_controller: self.effect_controller.to_static()?,
            direct_completions: self.direct_completions.to_static()?,
            parent_invocation: self.parent_invocation.clone(),
            observation_call_key: self.observation_call_key.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            observer: Arc::clone(&self.observer),
            checkpoint_messages: self.checkpoint_messages.clone(),
            trigger_outcomes: self.trigger_outcomes.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::clone(&self.attachment_source_policy),
            turn_context: self.turn_context.clone(),
            clock: Arc::clone(&self.clock),
            process_lineage: self.process_lineage.clone(),
        })
    }

    /// This context taken to `'static` with its controller slots lent
    /// `controller` — the conversion an opener registration performs when the
    /// dispatch's own controller cannot be taken static.
    ///
    /// What is lent is the deployment host's owned controller for the opener's
    /// admitted scope ([`EffectHost::scoped_static`]), never the opener's live
    /// handler-bound controller: a Restate handler cannot lend its `ctx`-bound
    /// controller past its handler, and the group-child driver replaces the
    /// lent slot at its rebind anyway (`rebind_child_dispatch` overwrites
    /// `effect_controller` and rebinds `direct_completions` through
    /// [`DirectCompletionClient::bind_tool_child`]), so no child ever executes
    /// under it.
    ///
    /// [`EffectHost::scoped_static`]: crate::EffectHost::scoped_static
    /// [`DirectCompletionClient::bind_tool_child`]: crate::DirectCompletionClient::bind_tool_child
    pub(crate) fn lend_static(
        &self,
        controller: crate::ScopedEffectController<'static>,
    ) -> ToolDispatchContext<'static> {
        ToolDispatchContext {
            plugins: Arc::clone(&self.plugins),
            tools: Arc::clone(&self.tools),
            tool_registry: self.tool_registry.clone(),
            tool_catalog: Arc::clone(&self.tool_catalog),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            session_graph: Arc::clone(&self.session_graph),
            processes: Arc::clone(&self.processes),
            trigger_router: self.trigger_router.clone(),
            process_definitions: self.process_definitions.clone(),
            process_engines: self.process_engines.clone(),
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(
                controller.clone(),
            ),
            direct_completions: self.direct_completions.lend_static(
                crate::runtime::RuntimeEffectControllerHandle::borrowed(controller),
            ),
            parent_invocation: self.parent_invocation.clone(),
            observation_call_key: self.observation_call_key.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            observer: Arc::clone(&self.observer),
            checkpoint_messages: self.checkpoint_messages.clone(),
            trigger_outcomes: self.trigger_outcomes.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::clone(&self.attachment_source_policy),
            turn_context: self.turn_context.clone(),
            clock: Arc::clone(&self.clock),
            process_lineage: self.process_lineage.clone(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolDispatchOutcome {
    pub record: ToolCallRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<lash_trace::TraceRetryAttempt>,
    #[serde(default, skip_serializing_if = "crate::ToolIntents::is_empty")]
    pub intents: crate::ToolIntents,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    /// The per-attempt captures in attempt order — committed
    /// `EnqueueMessages` facts and per-provider-attempt usage deltas — which
    /// the opener's settlement incorporation applies exactly once (ADR 0099
    /// §6/§13, FIG-3411). Guarded by `TOOL_SETTLEMENT_VERSION` because this
    /// rides the journaled `ToolInvocation` outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<crate::runtime::ToolAttemptCapture>,
    /// Trigger receipts the attempts emitted, carried to the same
    /// incorporation boundary the captures are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingToolDispatchOutcome {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub key: crate::AwaitEventKey,
    pub pending: crate::PendingCompletion,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<lash_trace::TraceRetryAttempt>,
    /// Captures collected from the attempts that ran before this call parked,
    /// carried across the park so the journaled pending row keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<crate::runtime::ToolAttemptCapture>,
    /// Trigger receipts emitted before this call parked, carried likewise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallLaunch {
    Done(Box<ToolDispatchOutcome>),
    Pending(Box<PendingToolDispatchOutcome>),
    ControllerAborted(crate::RuntimeEffectControllerError),
}

pub enum ToolPreparationOutcome {
    Prepared(Box<PreparedToolCall>),
    Completed(Box<ToolDispatchOutcome>),
}

pub(super) fn completed_preparation(outcome: ToolDispatchOutcome) -> ToolPreparationOutcome {
    ToolPreparationOutcome::Completed(Box::new(outcome))
}
pub(super) fn outcome(
    tool_name: String,
    args: serde_json::Value,
    result: super::retry::NormalizedToolOutput,
) -> ToolDispatchOutcome {
    let record = ToolCallRecord {
        call_id: None,
        tool: tool_name,
        args,
        output: result.into_output(),
    };
    ToolDispatchOutcome {
        record,
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

pub(super) fn launch_done(outcome: ToolDispatchOutcome) -> ToolCallLaunch {
    ToolCallLaunch::Done(Box::new(outcome))
}

pub(super) fn runtime_failure(
    class: ToolFailureClass,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::runtime(class, code, message))
}
