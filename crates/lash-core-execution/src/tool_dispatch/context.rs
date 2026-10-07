use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;

use crate::plugin::{
    PluginSession, SessionGraphService, SessionLifecycleService, SessionStateService,
};
use crate::{
    PreparedToolCall, ToolCallRecord, ToolCatalog, ToolFailure, ToolFailureClass, ToolOutcome,
    ToolProvider,
};

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

#[derive(Clone)]
pub struct ToolDispatchContext<'run> {
    pub plugins: Arc<PluginSession>,
    pub tools: Arc<dyn ToolProvider>,
    pub tool_registry: Option<Arc<crate::ToolRegistry>>,
    pub tool_catalog: Arc<ToolCatalog>,
    pub sessions: Arc<dyn SessionStateService>,
    /// The fleet format the owning store records: what a provider's
    /// preparation reads for the build it prepares under.
    pub fleet_format: crate::FleetFormat,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub trigger_router: Option<crate::TriggerRouter>,
    /// The engines a definition resolves against.
    pub process_engines: crate::ProcessEngineRegistry,
    pub effect_controller: crate::ActorContext,
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
    /// Who this dispatch runs for: a session on its admitted agent frame, or
    /// a process, which has neither a session nor a frame of its own.
    pub owner: crate::ExecutionOwner,
    /// The turn's observation sink (ADR 0105 §1): every host-facing event a
    /// dispatch emits is a synchronous [`ObservationSink::observe`] call,
    /// keyed by replay key and ordinal, and never awaited. A group child
    /// borrows the opener's so its call surfaces the same
    /// `ToolCallStarted`/`ToolCallCompleted` activities a turn-dispatched call
    /// emits (ADR 0099 §3's live half of the split); a dispatch that serves no
    /// turn stream carries [`NullObservationSink`](crate::engine::NullObservationSink).
    pub observer: Arc<dyn crate::engine::ObservationSink>,
    pub trigger_outcomes: ToolTriggerOutcomeBuffer,
    pub attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    pub turn_context: crate::TurnContext,
    pub clock: Arc<dyn crate::Clock>,
    /// The lineage of the process this dispatch runs inside, when it runs
    /// inside one (FIG-3607 R1): a start an intent realizes records it above
    /// its starter.
    pub process_lineage: Option<crate::ProcessLineage>,
    /// Recorded process originator whose authority this dispatch inherits.
    /// `None` outside process execution, where the session and frame own it.
    pub process_originator: Option<crate::ProcessOriginator>,
}

impl ToolDispatchContext<'_> {
    /// The replay-key base this dispatch's observation lanes key under: the
    /// invocation the dispatch serves when it carries one, else the admitted
    /// scope's journal identity — deterministic for a given dispatch, so a
    /// replay keys the same observations identically (ADR 0105 §1).
    pub fn observation_base_key(&self) -> String {
        if let Some(key) = self
            .parent_invocation
            .as_ref()
            .and_then(crate::RuntimeInvocation::effect_replay_key)
        {
            return key.to_owned();
        }
        let scoped = self.effect_controller.clone();
        let scope = scoped.execution_scope();
        debug_assert!(
            scope.journal_identity().is_ok(),
            "dispatch on scope `{}` names no journal identity for its observation base",
            scope.id(),
        );
        scope
            .journal_identity()
            .map(|identity| identity.key().to_owned())
            .unwrap_or_else(|_| format!("dispatch:{}:{}", scope.id(), self.owner.runtime_owner()))
    }

    /// This dispatch with one call's observation key installed:
    /// `material` qualified under this dispatch's *resolved* key — its
    /// installed call key when it carries one, else its base — so sibling
    /// calls that share a base mint distinct lanes. Pass the call's own
    /// effect-invocation replay key where it has one — a group child's
    /// `{group}:child:{position}` envelope, a command's key — else positional
    /// material the caller can prove unique: `{iteration}:{index}:{call_id}`
    /// on the turn-dispatched protocol path, `{batch_id}:{index}:{call_id}`
    /// inside a batch.
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
    /// instead of the base, so every lane the call emits — hook evidence,
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
            .execution_scope()
            .session_id()
            .map(crate::RuntimeAttribution::for_session)
            .unwrap_or_else(crate::RuntimeAttribution::none)
    }
}

impl<'run> ToolDispatchContext<'run> {
    pub fn process_scope(&self) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.effect_controller.clone())
            .with_parent_invocation(self.parent_invocation.clone())
            .with_agent_frame_id(self.owner.agent_frame_id().cloned())
            .with_process_lineage(self.process_lineage.clone())
    }

    pub(crate) fn to_static(&self) -> Option<ToolDispatchContext<'static>> {
        Some(ToolDispatchContext {
            fleet_format: self.fleet_format,
            plugins: Arc::clone(&self.plugins),
            tools: Arc::clone(&self.tools),
            tool_registry: self.tool_registry.clone(),
            tool_catalog: Arc::clone(&self.tool_catalog),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            session_graph: Arc::clone(&self.session_graph),
            processes: Arc::clone(&self.processes),
            trigger_router: self.trigger_router.clone(),
            process_engines: self.process_engines.clone(),
            effect_controller: self.effect_controller.clone(),
            direct_completions: self.direct_completions.to_static()?,
            parent_invocation: self.parent_invocation.clone(),
            observation_call_key: self.observation_call_key.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            owner: self.owner.clone(),
            observer: Arc::clone(&self.observer),
            trigger_outcomes: self.trigger_outcomes.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::clone(&self.attachment_source_policy),
            turn_context: self.turn_context.clone(),
            clock: Arc::clone(&self.clock),
            process_lineage: self.process_lineage.clone(),
            process_originator: self.process_originator.clone(),
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
    /// Trigger receipts the attempts emitted, applied exactly once at the
    /// opener's incorporation boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallLaunch {
    Done(Box<ToolDispatchOutcome>),
    ControllerAborted(crate::RuntimeEffectControllerError),
}

pub enum ToolPreparationOutcome {
    Prepared(Box<PreparedToolCall>),
    Completed(Box<ToolDispatchOutcome>),
}

pub(super) fn completed_preparation(outcome: ToolDispatchOutcome) -> ToolPreparationOutcome {
    ToolPreparationOutcome::Completed(Box::new(outcome))
}
/// The two ids every record of one call carries: lash's identity for the
/// call and, when a model issued it, the provider's correlation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallIds {
    pub call_id: lash_sansio::ToolCallId,
    /// Protocol correlation only: never key material.
    pub provider_call_id: Option<String>,
}

impl ToolCallIds {
    pub fn of(call: &PreparedToolCall) -> Self {
        Self {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
        }
    }

    pub fn of_pending(call: &crate::sansio::PendingToolCall) -> Self {
        Self {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
        }
    }
}

pub(super) fn outcome(
    ids: &ToolCallIds,
    tool_name: String,
    args: serde_json::Value,
    result: super::retry::NormalizedToolOutput,
) -> ToolDispatchOutcome {
    let record = ToolCallRecord {
        call_id: ids.call_id.clone(),
        provider_call_id: ids.provider_call_id.clone(),
        tool: tool_name,
        args,
        output: result.into_output(),
    };
    ToolDispatchOutcome {
        record,
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        triggers: Vec::new(),
    }
}

/// A completed attempt's launch: the record it settled and the intents it
/// declared.
pub(super) fn attempt_done(outcome: ToolDispatchOutcome) -> crate::ToolAttemptLaunch {
    crate::ToolAttemptLaunch::Done {
        record: Box::new(outcome.record),
        intents: outcome.intents,
    }
}

pub(super) fn runtime_failure(
    class: ToolFailureClass,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::runtime(class, code, message))
}
