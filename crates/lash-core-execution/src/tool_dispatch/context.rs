use crate::SessionId;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use tokio::sync::mpsc;

use crate::plugin::{
    PluginSession, SessionGraphService, SessionLifecycleService, SessionStateService,
};
use crate::{
    PreparedToolCall, SessionStreamEvent, ToolCallRecord, ToolCatalog, ToolFailure,
    ToolFailureClass, ToolOutcome, ToolProvider,
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

/// The processes an orchestrating tool realized, captured at the start's
/// journal boundary (ADR 0099 §6).
///
/// An orchestrating child runs *outside* an attempt frame, so a process its
/// body starts never appears in a `ToolIntentExecutionOutcome` — the channel
/// [`ToolSettlement`](crate::runtime::ToolSettlement) possession reads from.
/// The orchestrating `InternalProcessAdmin` enqueues each started id here at
/// the moment the durable start succeeds, and the child's driver drains it
/// into the settlement's possession, so replay serves the same possession set
/// the live run produced.
#[derive(Clone, Default)]
pub struct OrchestratingStartsBuffer {
    queue: Arc<Mutex<Vec<crate::ProcessId>>>,
}

impl OrchestratingStartsBuffer {
    pub(crate) fn enqueue(&self, process_id: crate::ProcessId) {
        let mut queue = self.queue.lock_recover();
        queue.push(process_id);
    }

    pub(crate) fn drain(&self) -> Vec<crate::ProcessId> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
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
    pub execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub session_id: SessionId,
    pub agent_frame_id: crate::FrameNodeId,
    pub event_tx: mpsc::Sender<SessionStreamEvent>,
    pub checkpoint_messages: CheckpointMessageBuffer,
    pub trigger_outcomes: ToolTriggerOutcomeBuffer,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    pub turn_context: crate::TurnContext,
    pub clock: Arc<dyn crate::Clock>,
    /// FIG-3472 probe: a deliberately unruled field that proves the
    /// rebind-completeness guard and the version gate refuse unlisted fields.
    pub(crate) probe_private_field: (),
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
pub const TOOL_CHILD_REBIND_VERSION: u16 = 1;

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
    ExecutionEnvSpec,
    SessionId,
    AgentFrameId,
    EventTx,
    CheckpointMessages,
    TriggerOutcomes,
    AttachmentStore,
    AttachmentSourcePolicy,
    TurnContext,
    Clock,
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
            Self::ExecutionEnvSpec => "execution_env_spec",
            Self::SessionId => "session_id",
            Self::AgentFrameId => "agent_frame_id",
            Self::EventTx => "event_tx",
            Self::CheckpointMessages => "checkpoint_messages",
            Self::TriggerOutcomes => "trigger_outcomes",
            Self::AttachmentStore => "attachment_store",
            Self::AttachmentSourcePolicy => "attachment_source_policy",
            Self::TurnContext => "turn_context",
            Self::Clock => "clock",
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
            // settlement.
            Self::CheckpointMessages | Self::TriggerOutcomes => RebindDisposition::Fresh,
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
            | Self::EventTx
            | Self::AttachmentStore
            | Self::AttachmentSourcePolicy
            | Self::TurnContext
            | Self::Clock => RebindDisposition::Lent,
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
    RebindField::ExecutionEnvSpec,
    RebindField::SessionId,
    RebindField::AgentFrameId,
    RebindField::EventTx,
    RebindField::CheckpointMessages,
    RebindField::TriggerOutcomes,
    RebindField::AttachmentStore,
    RebindField::AttachmentSourcePolicy,
    RebindField::TurnContext,
    RebindField::Clock,
];

impl<'run> ToolDispatchContext<'run> {
    pub fn process_scope(&self) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.effect_controller.scoped())
            .with_parent_invocation(self.parent_invocation.clone())
            .with_agent_frame_id(Some(self.agent_frame_id.clone()))
    }

    pub(crate) fn to_static(&self) -> Option<ToolDispatchContext<'static>> {
        Some(ToolDispatchContext {
            probe_private_field: (),
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
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            event_tx: self.event_tx.clone(),
            checkpoint_messages: self.checkpoint_messages.clone(),
            trigger_outcomes: self.trigger_outcomes.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::clone(&self.attachment_source_policy),
            turn_context: self.turn_context.clone(),
            clock: Arc::clone(&self.clock),
        })
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
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingToolDispatchOutcome {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub key: crate::AwaitEventKey,
    pub pending: crate::PendingCompletion,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<lash_trace::TraceRetryAttempt>,
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
    duration_ms: u64,
) -> ToolDispatchOutcome {
    let record = ToolCallRecord {
        call_id: None,
        tool: tool_name,
        args,
        output: result.into_output(),
        duration_ms,
    };
    ToolDispatchOutcome {
        record,
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
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
