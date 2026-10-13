use crate::SessionId;
use crate::plugin::{PluginSessionMaterializationRequest, PluginSessionRequest};
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_sansio::sync::{MutexExt, RwLockExt};

use super::*;

mod directives;
mod outcome_boundary;
mod prompt_inputs;
mod tools;
pub use tools::ResolvedToolSurface;

/// What a sequential turn callback returns beside its decision: commands
/// against its own plugin's namespace (K10).
pub(super) trait ProposesState {
    fn take_state(&mut self) -> StateCommands;
}

impl ProposesState for TurnContributions {
    fn take_state(&mut self) -> StateCommands {
        std::mem::take(&mut self.state)
    }
}

impl ProposesState for AfterTurnContributions {
    fn take_state(&mut self) -> StateCommands {
        std::mem::take(&mut self.state)
    }
}

/// Run a sequential turn callback slot in recorded registration order. Each
/// callback's state commands go to the recorded body running it, attributed
/// to the callback; its other contributions return in order.
async fn collect_owned_async<C, O, H, F>(
    session: &PluginSession,
    hooks: &[RegisteredHook<H>],
    ctx: C,
    hook_kind: &'static str,
    phase_probe: Option<&Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    invoke: F,
) -> Result<Vec<PluginOwned<O>>, PluginError>
where
    C: Clone,
    O: ProposesState,
    F: Fn(&H, C) -> PluginFuture<O>,
{
    let mut out = Vec::new();
    for registered in hooks {
        let phase_name = plugin_hook_phase_name(hook_kind, &registered.identity.owner);
        if let Some(probe) = phase_probe {
            probe.begin_named(&phase_name);
        }
        let result = invoke(&registered.hook, ctx.clone()).await;
        if let Some(probe) = phase_probe {
            probe.end_named(&phase_name);
        }
        let mut value = result?;
        session.propose_callback_state(
            &registered.identity,
            StateCommandOrigin::TurnHook {
                callback: registered.identity.clone(),
                segment: session.state_segment(),
            },
            value.take_state(),
        )?;
        out.push(PluginOwned {
            plugin_id: registered.identity.owner.clone(),
            value,
        });
    }
    Ok(out)
}

fn plugin_hook_phase_name(hook_kind: &str, plugin_id: &str) -> String {
    format!("plugin_hook.{hook_kind}.{plugin_id}")
}

fn lifecycle_event_hook_kind(event: &PluginLifecycleEvent) -> &'static str {
    match event {
        PluginLifecycleEvent::TurnFinalized(_) => "turn_finalized",
        PluginLifecycleEvent::SessionRestored(_) => "session_restored",
        PluginLifecycleEvent::SessionConfigChanged(_) => "session_config_changed",
    }
}

enum PluginOperationInvocation {
    Query {
        sessions: Arc<dyn SessionReadService>,
        processes: Arc<dyn ProcessReadService>,
    },
    Command {
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
    },
    Task {
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        scoped_effect_controller: crate::ActorContext,
        cancellation_token: tokio_util::sync::CancellationToken,
    },
}

impl PluginOperationInvocation {
    fn kind(&self) -> PluginOperationKind {
        match self {
            Self::Query { .. } => PluginOperationKind::Query,
            Self::Command { .. } => PluginOperationKind::Command,
            Self::Task { .. } => PluginOperationKind::Task,
        }
    }

    fn into_context(self, session_id: Option<SessionId>) -> PluginOperationContext {
        match self {
            Self::Query {
                sessions,
                processes,
            } => PluginOperationContext::Query(PluginQueryContext {
                session_id,
                sessions,
                processes,
            }),
            Self::Command {
                sessions,
                session_lifecycle,
                session_graph,
                processes,
            } => PluginOperationContext::Command(PluginCommandContext {
                session_id,
                sessions,
                session_lifecycle,
                session_graph,
                processes,
            }),
            Self::Task {
                sessions,
                session_lifecycle,
                session_graph,
                processes,
                scoped_effect_controller,
                cancellation_token,
            } => PluginOperationContext::Task(PluginTaskContext {
                session_id,
                sessions,
                session_lifecycle,
                session_graph,
                processes,
                scoped_effect_controller,
                cancellation_token,
            }),
        }
    }
}

pub fn plugin_lifecycle_hook_issue(error: PluginError) -> crate::runtime::TurnIssue {
    let failures = match &error {
        PluginError::HookFailures { causes } => causes
            .iter()
            .map(|cause| {
                let mut failure = cause.failure.clone();
                failure.error_type = "lash.plugin.hook".into();
                failure.error_version = std::num::NonZeroU32::MIN;
                failure.payload = serde_json::json!(cause);
                failure.origin = Some(cause.origin.clone());
                failure
            })
            .collect(),
        _ => vec![PluginOperationFailure::from(error.clone())],
    };
    crate::runtime::TurnIssue {
        severity: crate::runtime::TurnIssueSeverity::Advisory,
        kind: crate::TurnFailureKind::Plugin,
        code: Some(crate::TurnFailureCode::LifecycleHookFailed.into()),
        terminal_reason: None,
        message: error.to_string(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: None,
        plugin_failures: failures,
    }
}

pub(crate) fn collect_owned_sync<C, O, H, F>(
    hooks: &[RegisteredHook<H>],
    ctx: C,
    invoke: F,
) -> Result<Vec<PluginOwned<O>>, PluginError>
where
    C: Clone,
    F: Fn(&H, C) -> Result<O, PluginError>,
{
    let mut out = Vec::new();
    for registered in hooks {
        out.push(PluginOwned {
            plugin_id: registered.identity.owner.clone(),
            value: invoke(&registered.hook, ctx.clone())?,
        });
    }
    Ok(out)
}

/// The session's resident authority: the plugin configuration its hooks and
/// tool providers run under.
#[derive(Clone)]
pub(super) struct LiveSessionAuthority {
    /// The plugin configuration the session's hooks run under (FIG-4379):
    /// the running run's admitted configuration, the head's outside a run,
    /// or a process's captured configuration.
    pub(super) plugin_config: super::AdmittedPluginConfig,
}

#[derive(Clone)]
pub struct PluginSession {
    pub(super) state: Arc<std::sync::Mutex<PluginStateRegistry>>,
    pub(super) native_view: Arc<std::sync::Mutex<Option<PluginNativeView>>>,
    pub(super) host: PluginHost,
    pub(super) owner: crate::RuntimeOwner,
    pub(super) capabilities: Arc<std::sync::OnceLock<PluginSessionCapabilities>>,
    pub(super) materialized: Arc<std::sync::atomic::AtomicBool>,
    pub(super) materialization_lock: Arc<std::sync::Mutex<()>>,
    pub(super) materialization: PluginSessionMaterialization,
    pub(super) tool_catalog_overlay: ToolCatalogContribution,
    pub(super) authority: Arc<std::sync::RwLock<LiveSessionAuthority>>,
    pub(super) extensions: PluginExtensions,
    /// Whether a plugin kept its state view past registration or
    /// `session_ready`: such a plugin reads published state from any
    /// callback (FIG-3712).
    pub(super) retains_state: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the session's plugins were seeded from a parent session's
    /// capture rather than built fresh or rematerialized from their own.
    pub(super) forked: bool,
    /// The plugin admission the session's work was last admitted under
    /// (FIG-4747): the writer formats its commits encode plugin namespaces
    /// in. `None` until an admission is adopted; the session then writes
    /// each plugin's native format.
    pub(super) admission:
        Arc<std::sync::Mutex<Option<crate::store::plugin_writers::PluginAdmission>>>,
}
pub(super) struct PluginSessionCapabilities {
    pub(super) composition: Arc<ExecutionComposition>,
    pub(super) plugins: Vec<Arc<dyn SessionPlugin>>,
    pub(super) tool_registry: Arc<crate::ToolRegistry>,
    pub(super) session_extensions: PluginExtensions,
    pub(super) contributions: PluginContributions,
}

impl PluginSession {
    pub fn composition(&self) -> Arc<ExecutionComposition> {
        Arc::clone(&self.capabilities().composition)
    }

    /// The runtime's execution budgets this session's tools run under.
    pub fn data_limits(&self) -> crate::runtime::RuntimeDataLimits {
        self.host.data_limits()
    }

    pub fn execution_budgets(&self) -> crate::ExecutionBudgets {
        self.host.execution_budgets()
    }

    pub fn materialize(self: &Arc<Self>) -> Result<(), PluginError> {
        self.host.materialize_session(self)
    }

    /// Build this session's plugin contributions without readying it: no
    /// registration and no `session_ready` callback runs, so a pure
    /// derivation (a run's render resolution, FIG-6021) can read its
    /// protocol driver before the session is activated.
    /// [`materialize`](Self::materialize) readies the same contributions.
    pub fn prepare_capabilities(self: &Arc<Self>) -> Result<(), PluginError> {
        self.host.prepare_session(self)
    }

    pub fn is_materialized(&self) -> bool {
        self.materialized.load(std::sync::atomic::Ordering::Acquire)
    }

    #[expect(
        clippy::expect_used,
        reason = "the engine publishes admission before invoking capabilities"
    )]
    pub(super) fn capabilities(&self) -> &PluginSessionCapabilities {
        self.capabilities
            .get()
            .expect("plugin capabilities require published admission")
    }
}

/// A plugin dispatch bound to its already-resolved turn instrumentation.
///
/// Explicitly unstable internal instrumentation. See
/// `docs/architecture/turn-phase-probe.md` in the repository.
#[doc(hidden)]
pub struct PluginDispatchContext<'a> {
    session: &'a PluginSession,
    phase_probe: Option<&'a Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl PluginDispatchContext<'_> {
    pub async fn before_turn(
        &self,
        ctx: TurnHookContext,
    ) -> Result<Vec<PluginOwned<TurnContributions>>, PluginError> {
        collect_owned_async(
            self.session,
            &self.session.capabilities().contributions.before_turn_hooks,
            ctx,
            "before_turn",
            self.phase_probe,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    pub async fn after_turn(
        &self,
        ctx: TurnResultHookContext,
    ) -> Result<Vec<PluginOwned<AfterTurnContributions>>, PluginError> {
        collect_owned_async(
            self.session,
            &self.session.capabilities().contributions.after_turn_hooks,
            ctx,
            "after_turn",
            self.phase_probe,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    /// Deliver `event` to every lifecycle observer, sequentially in recorded
    /// registration order. Every observer runs; their failures aggregate as
    /// typed causes in that order.
    pub async fn emit_runtime_event(&self, event: PluginLifecycleEvent) -> Result<(), PluginError> {
        let hook_kind = lifecycle_event_hook_kind(&event);
        let mut causes = Vec::new();
        for registered in &self
            .session
            .capabilities()
            .contributions
            .runtime_event_hooks
        {
            let phase_name = plugin_hook_phase_name(hook_kind, registered.identity.owner.as_str());
            if let Some(probe) = self.phase_probe {
                probe.begin_named(&phase_name);
            }
            let result = (registered.hook)(event.clone()).await;
            if let Some(probe) = self.phase_probe {
                probe.end_named(&phase_name);
            }
            if let Err(error) = result {
                let identity = registered.identity.clone();
                let origin = PluginFailureOrigin {
                    plugin_id: identity.owner,
                    operation: identity.key,
                };
                let mut failure = PluginOperationFailure::from(error);
                failure.origin.get_or_insert_with(|| origin.clone());
                causes.push(PluginHookFailure { origin, failure });
            }
        }
        if causes.is_empty() {
            Ok(())
        } else {
            Err(PluginError::HookFailures { causes })
        }
    }
}

impl PluginSession {
    /// Bind the probe once for preparation, finalization and lifecycle delivery.
    /// This internal instrumentation context has no stability promise.
    #[doc(hidden)]
    pub fn dispatch<'a>(
        &'a self,
        phase_probe: Option<&'a Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> PluginDispatchContext<'a> {
        PluginDispatchContext {
            session: self,
            phase_probe,
        }
    }

    /// Adopt `admission` as the one this session's commits write under: the
    /// record of the Run or process segment that runs now. A retry or replay
    /// adopts the same record, so it writes the same formats whatever the
    /// fleet record permits by then.
    pub fn adopt_plugin_admission(&self, admission: crate::store::plugin_writers::PluginAdmission) {
        *self.admission.lock_recover() = Some(admission);
    }

    /// The plugin admission this session writes under, if it adopted one.
    pub fn plugin_admission(&self) -> Option<crate::store::plugin_writers::PluginAdmission> {
        self.admission.lock_recover().clone()
    }

    /// The writer of every plugin of this session under the adopted
    /// admission, or `None` when each one is its plugin's native format: no
    /// admission is adopted, or it chose the native format throughout. A
    /// plugin the admission does not name keeps its native format: the
    /// fleet record's check at the commit decides it.
    fn recorded_writers(&self) -> Option<BTreeMap<String, FormatVersion>> {
        let mut writers = self.plugin_admission()?.writers();
        let mut native = true;
        for factory in self.host.factories() {
            let declared = factory.plugin_declaration().format_version;
            native &= *writers.entry(factory.id().to_owned()).or_insert(declared) == declared;
        }
        (!native).then_some(writers)
    }

    /// `state`, a native capture, in the formats the adopted admission
    /// recorded.
    fn in_recorded_formats(&self, state: PluginState) -> Result<PluginState, PluginError> {
        match self.recorded_writers() {
            Some(writers) => self.host.encode_state(&state, &writers),
            None => Ok(state),
        }
    }

    /// `config` in the formats the adopted admission recorded, or `None`
    /// when it is already written in them.
    pub fn committed_plugin_config(
        &self,
        config: &PluginConfig,
    ) -> Result<Option<PluginConfig>, FormatRefusal> {
        let Some(writers) = self.recorded_writers() else {
            return Ok(None);
        };
        if config.namespaces().iter().all(|(id, namespace)| {
            writers
                .get(id)
                .is_none_or(|writer| namespace.format_version == *writer)
        }) {
            return Ok(None);
        }
        let encoded = self.host.encode_config(config, &writers)?;
        Ok((encoded != *config).then_some(encoded))
    }

    /// Whether `snapshot` is what this session would commit now: its live
    /// state in the adopted admission's formats. A head written in an older
    /// format than the plugin's native one decodes to another generation
    /// than the live state it was captured from, so identity is judged on
    /// the written form.
    fn is_committed_form(&self, snapshot: &PluginState) -> bool {
        // Written natively, the committed form is the live state itself,
        // which the hydration check already compares.
        self.is_materialized()
            && self.recorded_writers().is_some_and(|writers| {
                self.host
                    .encode_state(&self.capture_state(), &writers)
                    .is_ok_and(|committed| committed == *snapshot)
            })
    }
    /// Who this plugin session was built for.
    pub fn owner(&self) -> &crate::RuntimeOwner {
        &self.owner
    }

    /// The plugin configuration this session's hooks run under: a running
    /// run's admitted configuration and revision, taken from its recorded
    /// run (FIG-4379), never the session's current head.
    pub fn admitted_plugin_config(&self) -> super::AdmittedPluginConfig {
        self.authority.read_recover().plugin_config.clone()
    }

    /// Publish the configuration view the runtime installed — a run's
    /// recorded one, or the head's — to this session's hooks.
    pub fn publish_plugin_config(
        &self,
        plugin_config: super::AdmittedPluginConfig,
    ) -> Result<(), super::FormatRefusal> {
        self.host.validate_config_formats(&plugin_config.config)?;
        self.authority.write_recover().plugin_config = plugin_config;
        Ok(())
    }

    pub(super) fn live_authority(&self) -> LiveSessionAuthority {
        self.authority.read_recover().clone()
    }

    /// Whether this session's plugins hold mutable session state: a plugin
    /// kept its state store, or a namespace holds values (FIG-3712).
    pub fn holds_plugin_state(&self) -> bool {
        self.retains_state.load(std::sync::atomic::Ordering::SeqCst)
            || self
                .state
                .lock_recover()
                .data
                .plugins
                .values()
                .any(|namespace| !namespace.values.is_empty())
    }

    /// Whether this session's plugins were forked from a parent session.
    pub fn forked_plugins(&self) -> bool {
        self.forked || !self.tool_catalog_overlay.is_empty()
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    /// Extensions contributed by this session's plugins, distinct from the
    /// host-static extensions in [`Self::extensions`].
    pub fn session_extensions(&self) -> &PluginExtensions {
        &self.capabilities().session_extensions
    }

    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    pub fn tool_registry(&self) -> Arc<crate::ToolRegistry> {
        Arc::clone(&self.capabilities().tool_registry)
    }

    /// The id of the plugin that registered this session's protocol: the
    /// owner whose recorded namespace is the session's protocol turn options
    /// (FIG-4379).
    #[expect(
        clippy::expect_used,
        reason = "session assembly refuses a contribution set without a protocol session before this object exists"
    )]
    pub fn protocol_plugin_id(&self) -> &str {
        &self
            .capabilities()
            .contributions
            .protocol_session
            .as_ref()
            .expect("plugin session must have a protocol session")
            .identity
            .owner
    }

    #[expect(
        clippy::expect_used,
        reason = "session assembly refuses a contribution set without a protocol session before this object exists"
    )]
    pub fn protocol_session(&self) -> &Arc<dyn ProtocolSessionPlugin> {
        &self
            .capabilities()
            .contributions
            .protocol_session
            .as_ref()
            .expect("plugin session must have a protocol session")
            .hook
    }

    pub fn code_executor(&self) -> Option<Arc<dyn CodeExecutorPlugin>> {
        self.capabilities()
            .contributions
            .code_executor
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
    }

    /// The host's transcript decoders: pure, so a session decodes its
    /// committed history whether or not its plugins are materialized.
    pub fn transcript_decoders(&self) -> lash_core_store::transcript::TranscriptDecoders {
        self.host.transcript_decoders()
    }

    pub fn assistant_prose_projector(&self) -> Option<Arc<dyn AssistantProseProjectorPlugin>> {
        self.capabilities()
            .contributions
            .assistant_prose_projector
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
    }

    #[expect(
        clippy::expect_used,
        reason = "session assembly refuses a contribution set without a protocol driver before this object exists"
    )]
    pub fn protocol_driver(&self) -> Arc<dyn ProtocolDriverPlugin> {
        self.capabilities()
            .contributions
            .protocol_driver
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
            .expect("plugin session must have a protocol driver")
    }

    pub fn plugin_operations(&self) -> Vec<PluginOperationDef> {
        self.capabilities()
            .contributions
            .plugin_operations
            .values()
            .map(|op| op.def().clone())
            .collect()
    }

    /// Select response callbacks in registration order before the model call.
    pub fn assistant_response_plan(&self) -> crate::runtime::AssistantResponsePlan {
        crate::runtime::AssistantResponsePlan {
            callbacks: self
                .capabilities()
                .contributions
                .assistant_response_hooks
                .iter()
                .map(|registered| registered.identity.clone())
                .collect(),
        }
    }

    fn unavailable_callback(&self, callback: &PluginCallbackIdentity) -> PluginError {
        PluginError::Runtime(
            RecordedCallbackUnavailable {
                recorded: callback.clone(),
                available: self
                    .capabilities()
                    .contributions
                    .assistant_response_hooks
                    .iter()
                    .map(|hook| hook.identity.clone())
                    .collect(),
            }
            .into_runtime_error(),
        )
    }

    /// Ask each registered context-pressure hook, in priority order, what the
    /// turn being prepared needs. The first hook that opens a frame is the
    /// last one asked: a turn opens at most one frame. `Continue` decisions
    /// are dropped.
    pub async fn decide_context_pressure(
        &self,
        ctx: &ContextPressureContext<'_>,
        phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<Vec<DecidedContextPressure>, ContextError> {
        let mut decided = Vec::new();
        for (_, registered) in &self.capabilities().contributions.context_pressure_hooks {
            let phase_name =
                plugin_hook_phase_name("context_pressure", registered.identity.owner.as_str());
            if let Some(probe) = phase_probe.as_ref() {
                probe.begin_named(&phase_name);
            }
            let result = registered.hook.decide(ctx).await;
            if let Some(probe) = phase_probe.as_ref() {
                probe.end_named(&phase_name);
            }
            let decision = result?;
            let opens_frame = matches!(decision, ContextPressureDecision::OpenFrame { .. });
            if !matches!(decision, ContextPressureDecision::Continue) {
                decided.push(DecidedContextPressure {
                    plugin_id: registered.identity.owner.clone(),
                    hook_id: registered.hook.id().to_owned(),
                    decision,
                });
            }
            if opens_frame {
                break;
            }
        }
        Ok(decided)
    }

    /// Ask registered compactors for seed nodes for a new compaction frame.
    pub async fn compact_context(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        for (_, registered) in &self.capabilities().contributions.context_compactors {
            if let Some(compaction) = registered.hook.compact(ctx).await?
                && !compaction.is_empty()
            {
                return Ok(Some(compaction));
            }
        }
        Ok(None)
    }

    pub async fn at_checkpoint(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<Vec<PluginOwned<TurnContributions>>, PluginError> {
        collect_owned_async(
            self,
            &self.capabilities().contributions.checkpoint_hooks,
            ctx,
            "checkpoint",
            None,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    pub async fn transform_assistant_stream(
        &self,
        session_id: &SessionId,
        chunk: String,
    ) -> Result<Vec<PluginOwned<AssistantStreamTransform>>, PluginError> {
        let mut current = chunk;
        let mut transforms = Vec::new();
        for registered in &self.capabilities().contributions.assistant_stream_hooks {
            let transform = (registered.hook)(AssistantStreamHookContext {
                session_id: session_id.clone(),
                plugin_config: self.admitted_plugin_config(),
                chunk: current.clone(),
            })
            .await?;
            current = transform.chunk.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.identity.owner.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    /// Check every recorded key and stream-state format without invoking a callback.
    pub fn validate_assistant_response_plan(
        &self,
        plan: &crate::runtime::AssistantResponsePlan,
        stream_hook_states: &[crate::runtime::AssistantStreamHookState],
    ) -> Result<(), PluginError> {
        let callbacks = self.resolve_assistant_response_plan(plan)?;
        // Validate all recorded state before the first callback can run.
        for registered in &callbacks {
            if let Some(recorded) = stream_hook_states
                .iter()
                .find(|state| state.callback == registered.identity)
            {
                let declaration = self
                    .host
                    .factories()
                    .iter()
                    .find(|factory| factory.id() == registered.identity.owner)
                    .ok_or_else(|| self.unavailable_callback(&registered.identity))?
                    .plugin_declaration();
                if !declaration
                    .readable_stream_state_formats
                    .contains(&recorded.format)
                {
                    return Err(PluginError::Runtime(
                        StreamStateFormatRefusal {
                            callback: recorded.callback.clone(),
                            stored: recorded.format,
                            readable: declaration.readable_stream_state_formats,
                        }
                        .into_runtime_error(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn resolve_assistant_response_plan(
        &self,
        plan: &crate::runtime::AssistantResponsePlan,
    ) -> Result<Vec<&RegisteredHook<AssistantResponseHook>>, PluginError> {
        plan.callbacks
            .iter()
            .map(|callback| {
                self.capabilities()
                    .contributions
                    .assistant_response_hooks
                    .iter()
                    .find(|registered| &registered.identity == callback)
                    .ok_or_else(|| self.unavailable_callback(callback))
            })
            .collect()
    }

    pub async fn transform_assistant_response(
        &self,
        session_id: &SessionId,
        response: crate::llm::types::LlmResponse,
        plan: &crate::runtime::AssistantResponsePlan,
        stream_hook_states: &[crate::runtime::AssistantStreamHookState],
    ) -> Result<Vec<PluginOwned<AssistantResponseTransform>>, PluginError> {
        self.validate_assistant_response_plan(plan, stream_hook_states)?;
        let callbacks = self.resolve_assistant_response_plan(plan)?;
        let mut current = response;
        let mut transforms = Vec::new();
        for registered in callbacks {
            let stream_state = stream_hook_states
                .iter()
                .find(|recorded| recorded.callback == registered.identity);
            let transform = (registered.hook)(AssistantResponseHookContext {
                session_id: session_id.clone(),
                plugin_config: self.admitted_plugin_config(),
                response: current.clone(),
                stream_state: stream_state.map(|recorded| recorded.state.clone()),
                stream_state_format: stream_state.map(|recorded| recorded.format),
            })
            .await?;
            current = transform.response.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.identity.owner.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    /// Runs every stream-finished hook. Its state is recorded for each
    /// response callback that names it as its `stream_state_from`.
    pub async fn finish_assistant_stream(
        &self,
        session_id: &SessionId,
        reason: AssistantStreamFinishReason,
    ) -> Result<Vec<crate::runtime::AssistantStreamHookState>, PluginError> {
        let mut states = Vec::new();
        for registered in &self
            .capabilities()
            .contributions
            .assistant_stream_finished_hooks
        {
            let state = (registered.hook)(AssistantStreamFinishedContext {
                session_id: session_id.clone(),
                plugin_config: self.admitted_plugin_config(),
                reason,
            })
            .await?;
            let Some(state) = state else {
                continue;
            };
            for (response, _) in self
                .capabilities()
                .contributions
                .assistant_stream_state_pairs
                .iter()
                .filter(|(_, finished)| finished == &registered.identity)
            {
                states.push(crate::runtime::AssistantStreamHookState {
                    callback: response.clone(),
                    format: self
                        .host
                        .factories()
                        .iter()
                        .find(|factory| factory.id() == response.owner)
                        .ok_or_else(|| self.unavailable_callback(response))?
                        .plugin_declaration()
                        .stream_state_format,
                    state: state.clone(),
                });
            }
        }
        Ok(states)
    }

    /// Host handles capture every namespace. Plugin-facing handles export none.
    pub fn export_state(&self) -> PluginState {
        if self.host.export_plugin_namespaces {
            self.capture_state()
        } else {
            PluginState::default()
        }
    }

    /// The state a runtime-owned commit records: every namespace, in the
    /// formats the adopted admission recorded (FIG-4747).
    /// Plugin-facing handles refuse with [`PluginError::Session`].
    pub fn committed_state(&self) -> Result<PluginState, PluginError> {
        self.require_runtime_owner()?;
        self.in_recorded_formats(self.capture_state())
    }

    /// The state a commit carrying `staged` writes: [`Self::committed_state`]
    /// with `staged`'s resolutions applied, which publish only once that
    /// commit is acknowledged.
    ///
    /// # Errors
    ///
    /// A resolution its namespace's frontier refuses, or a namespace the
    /// recorded formats cannot encode.
    pub fn committed_state_with(
        &self,
        staged: &StagedPluginState,
    ) -> Result<PluginState, PluginError> {
        self.require_runtime_owner()?;
        self.in_recorded_formats(self.state_with(staged.resolutions())?)
    }

    pub fn require_runtime_owner(&self) -> Result<(), PluginError> {
        if self.host.export_plugin_namespaces {
            Ok(())
        } else {
            Err(PluginError::Session(
                "plugin-facing session handles cannot construct a host runtime".into(),
            ))
        }
    }

    pub(crate) fn capture_state(&self) -> PluginState {
        self.state.lock_recover().data.clone()
    }

    pub(crate) fn state_generations(&self) -> BTreeMap<String, u64> {
        self.state
            .lock_recover()
            .data
            .plugins
            .iter()
            .map(|(id, ns)| (id.clone(), ns.generation))
            .collect()
    }

    pub fn matches_state_ref(&self, reference: &crate::BlobRef) -> bool {
        self.state.lock_recover().matches_ref(reference)
    }

    pub fn require_hydrated_state(&self, snapshot: &PluginState) -> Result<(), PluginError> {
        if self.is_committed_form(snapshot) {
            return Ok(());
        }
        let snapshot = snapshot.clone();
        if self.state.lock_recover().was_hydrated_from(&snapshot) {
            Ok(())
        } else {
            Err(PluginError::Session("persisted plugin state requires a rematerialization request before runtime construction".into()))
        }
    }

    /// Adopt `snapshot`, a recorded head's plugin state, as the live state:
    /// an accepted write the head does not carry is dropped, as a cold
    /// rebuild from that head drops it (FIG-4392). A durable run restores
    /// from its pinned head and its rows instead (`restore_run`).
    #[cfg(test)]
    pub(crate) fn hydrate_state(&self, snapshot: &PluginState) -> Result<(), PluginError> {
        if self.is_committed_form(snapshot) {
            return Ok(());
        }
        self.host
            .validate_native_formats(snapshot, &self.admitted_plugin_config().config)?;
        let snapshot = snapshot.clone();
        let mut live = self.state.lock_recover();
        live.hydrate_live(&snapshot);
        for factory in self.host.factories() {
            if self
                .capabilities()
                .plugins
                .iter()
                .any(|plugin| plugin.id() == factory.id())
            {
                live.declare_fork(factory.id(), factory.plugin_declaration().state_fork);
            }
        }
        Ok(())
    }

    pub fn fork_for_session(
        &self,
        session_id: impl Into<SessionId>,
        config: super::SessionAuthorityContext,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.require_runtime_owner()?;
        let snapshot = self.capture_state();
        self.host.build_session(PluginSessionRequest {
            tool_catalog_overlay: self.tool_catalog_overlay.clone(),
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: Some(&snapshot),
            },
            owner: crate::RuntimeOwner::Session(session_id.into()),
        })
    }

    fn effective_operation_session(
        &self,
        name: &str,
        session_param: SessionParam,
        session_id: Option<SessionId>,
        default_to_current_session: bool,
    ) -> Result<Option<SessionId>, PluginOperationInvokeError> {
        let effective_session = session_id.or_else(|| {
            if default_to_current_session {
                self.owner.session_id().cloned()
            } else {
                None
            }
        });

        match (session_param, effective_session.as_ref()) {
            (SessionParam::Required, None) => {
                return Err(PluginOperationInvokeError::MissingSession(name.to_string()));
            }
            (SessionParam::Forbidden, Some(_)) => {
                return Err(PluginOperationInvokeError::UnexpectedSession(
                    name.to_string(),
                ));
            }
            _ => {}
        }
        Ok(effective_session)
    }

    async fn invoke_plugin_operation(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
        default_to_current_session: bool,
        invocation: PluginOperationInvocation,
    ) -> Result<(String, ErasedPluginOperationOutcome), PluginOperationInvokeError> {
        let Some(operation) = self
            .capabilities()
            .contributions
            .plugin_operations
            .get(name)
            .cloned()
        else {
            return Err(PluginOperationInvokeError::Unknown(name.to_string()));
        };
        if operation.def().kind() != invocation.kind() {
            return Err(PluginOperationInvokeError::Unknown(name.to_string()));
        }
        let effective_session = self.effective_operation_session(
            name,
            operation.def().session_param,
            session_id,
            default_to_current_session,
        )?;
        let outcome = operation
            .invoke(invocation.into_context(effective_session), args)
            .await
            .map_err(|mut failure| {
                failure.origin = Some(operation.failure_origin());
                PluginOperationInvokeError::Failed(Box::new(failure))
            })?;
        Ok((operation.plugin_id().to_string(), outcome))
    }

    pub async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionReadService>,
        processes: Arc<dyn ProcessReadService>,
    ) -> Result<(String, serde_json::Value), PluginOperationInvokeError> {
        let (plugin_id, outcome) = self
            .invoke_plugin_operation(
                name,
                args,
                session_id,
                default_to_current_session,
                PluginOperationInvocation::Query {
                    sessions,
                    processes,
                },
            )
            .await?;
        Ok((plugin_id, outcome.output))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "plugin command invocation carries the runtime mutation services exposed to commands"
    )]
    pub async fn run_plugin_command(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
    ) -> Result<(String, PluginOperationOutcome<serde_json::Value>), PluginOperationInvokeError>
    {
        let (plugin_id, outcome) = self
            .invoke_plugin_operation(
                name,
                args,
                session_id,
                default_to_current_session,
                PluginOperationInvocation::Command {
                    sessions,
                    session_lifecycle,
                    session_graph,
                    processes,
                },
            )
            .await?;
        Ok((
            plugin_id,
            PluginOperationOutcome {
                output: outcome.output,
                events: outcome.events,
                directives: outcome.directives,
            },
        ))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "plugin task invocation carries mutation services plus the scoped effect boundary"
    )]
    pub async fn run_plugin_task(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        scoped_effect_controller: crate::ActorContext,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<(String, PluginOperationOutcome<serde_json::Value>), PluginOperationInvokeError>
    {
        let (plugin_id, outcome) = self
            .invoke_plugin_operation(
                name,
                args,
                session_id,
                default_to_current_session,
                PluginOperationInvocation::Task {
                    sessions,
                    session_lifecycle,
                    session_graph,
                    processes,
                    scoped_effect_controller,
                    cancellation_token,
                },
            )
            .await?;
        Ok((
            plugin_id,
            PluginOperationOutcome {
                output: outcome.output,
                events: outcome.events,
                directives: outcome.directives,
            },
        ))
    }
}

impl lash_core_store::session_state::SessionPluginStateSource for PluginSession {
    fn capture_plugin_admission(
        &self,
        config: &PluginConfig,
        fleet: crate::FleetFormat,
    ) -> Result<Option<Arc<[u8]>>, crate::RuntimeError> {
        self.capture_native_view(Some(config), fleet)
            .map_err(|error| crate::RuntimeEffectControllerError::from(error).into_runtime_error())
    }

    fn export_plugin_state(&self) -> Result<PluginState, crate::RuntimeError> {
        self.in_recorded_formats(self.export_state())
            .map_err(|error| crate::RuntimeEffectControllerError::from(error).into_runtime_error())
    }

    fn capture_plugin_state(&self) -> Result<PluginState, crate::RuntimeError> {
        self.committed_state()
            .map_err(|error| crate::RuntimeEffectControllerError::from(error).into_runtime_error())
    }

    fn committed_plugin_config(
        &self,
        config: &PluginConfig,
    ) -> Result<Option<PluginConfig>, crate::RuntimeError> {
        Ok(PluginSession::committed_plugin_config(self, config)?)
    }
}

#[cfg(feature = "testing")]
impl PluginSession {
    /// A read-only view of `plugin_id`'s namespace over this session's
    /// published plugin state, bound the way the host binds the view a
    /// plugin receives.
    pub(crate) fn plugin_state_view_for_testing(&self, plugin_id: &str) -> super::PluginStateView {
        super::PluginStateView::bind(&self.owner, plugin_id, Arc::clone(&self.state))
    }
}

#[cfg(test)]
mod attachment_notice_order_tests {
    use super::*;
    use lash_sansio::core_support::ModelToolReturnCoreSupport as _;

    #[tokio::test]
    async fn attachment_notice_follows_a_step_that_replaces_the_model_parts() {
        let step: super::super::ToolPresentationStep = Arc::new(|input| {
            Box::pin(async move {
                Ok(crate::ModelToolReturn::text(
                    input.context.tool_name,
                    "replacement".to_string(),
                ))
            })
        });
        let host = crate::testing::test_plugin_host(vec![Arc::new(
            super::super::StaticPluginFactory::new(
                crate::plugin::PluginDeclaration::initial("notice-order-step"),
                super::super::PluginSpec::new()
                    .with_presentation_step(crate::hook_key!("presentation-step-1"), step),
            ),
        )]);
        let session = host
            .build_session(PluginSessionRequest::creation(
                "notice-order-session",
                crate::plugin::SessionAuthorityContext::fixture(),
            ))
            .expect("plugin session");
        let reference = crate::AttachmentRef::new(
            crate::AttachmentId::parse(
                "1b6d42cdc3895b0073391a47cf8fdd73b17d5dd9cad33c7a45b4b49765631e8d",
            )
            .expect("attachment id"),
            crate::MediaType::parse("application/octet-stream").expect("media type"),
            4,
            None,
            None,
        );
        let output =
            crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(reference));
        let facts = Arc::new(crate::plugin::ToolPresentationFacts {
            intent_outcomes: Vec::new(),
        });
        let presented = session
            .composition()
            .present_tool_result(
                super::super::ToolResultProjectionContext {
                    owner: crate::RuntimeOwner::Session("notice-order-session".into()),
                    call_id: crate::ToolCallId::fixture("call"),
                    tool_id: crate::ToolId::new("fixture:id"),
                    tool_name: "fixture".into(),
                    render: None,
                    args: serde_json::Value::Null,
                    output,
                    duration_ms: 0,
                    artifacts: Arc::new(super::super::NoPresentationArtifacts),
                },
                facts,
                &session.composition().tool_presentation_plan(),
                &crate::provider::AttachmentCapabilitySnapshot::default(),
            )
            .await
            .expect("present result");
        assert_eq!(presented.model_return.parts.len(), 2);
        assert_eq!(
            presented.model_return.parts[0],
            crate::ModelToolReturnPart::text("replacement".to_string())
        );
        assert_eq!(presented.model_return.attachment_notices.len(), 1);
    }
}

#[cfg(test)]
mod presentation_plan_tests {
    use super::*;
    use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PresenterFactory {
        text: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl PluginFactory for PresenterFactory {
        fn id(&self) -> &'static str {
            "plan-presenter"
        }

        fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(Presenter {
                calls: Arc::clone(&self.calls),
                text: self.text,
            }))
        }
    }

    impl crate::plugin::PluginMetadata for PresenterFactory {
        fn plugin_declaration(&self) -> PluginDeclaration {
            PluginDeclaration::initial("plan-presenter")
        }
    }

    struct Presenter {
        calls: Arc<AtomicUsize>,
        text: &'static str,
    }
    impl SessionPlugin for Presenter {
        fn id(&self) -> &'static str {
            "plan-presenter"
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
            let calls = Arc::clone(&self.calls);
            let text = self.text;
            registrar.tool_results().presenter(Arc::new(move |input| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(crate::ModelToolReturn::text(
                        input.context.tool_name.clone(),
                        text,
                    ))
                })
            }))
        }
    }

    fn session(text: Option<&'static str>, calls: &Arc<AtomicUsize>) -> Arc<PluginSession> {
        crate::testing::test_plugin_host(
            text.map(|text| {
                Arc::new(PresenterFactory {
                    text,
                    calls: Arc::clone(calls),
                }) as Arc<dyn PluginFactory>
            })
            .into_iter()
            .collect(),
        )
        .build_session(PluginSessionRequest::creation(
            "presentation-plan",
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap()
    }

    async fn present(
        session: &PluginSession,
        plan: &crate::runtime::PresentationBinding,
    ) -> Result<crate::runtime::effect::ToolPresentation, crate::RuntimeEffectControllerError> {
        let output = crate::ToolCallOutput::success("semantic-result");
        let facts = Arc::new(crate::plugin::ToolPresentationFacts {
            intent_outcomes: Vec::new(),
        });
        session
            .composition()
            .present_tool_result(
                ToolResultProjectionContext {
                    owner: crate::RuntimeOwner::Session("presentation-plan".into()),
                    call_id: crate::ToolCallId::fixture("call"),
                    tool_id: crate::ToolId::new("fixture:id"),
                    tool_name: "fixture".into(),
                    render: None,
                    args: serde_json::Value::Null,
                    output,
                    duration_ms: 0,
                    artifacts: Arc::new(NoPresentationArtifacts),
                },
                facts,
                plan,
                &crate::provider::AttachmentCapabilitySnapshot::default(),
            )
            .await
    }

    #[tokio::test]
    async fn a_recorded_presenter_resolves_by_key_and_refuses_a_missing_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let plan = session(Some("old"), &calls)
            .composition()
            .tool_presentation_plan();
        let error = present(&session(None, &calls), &plan)
            .await
            .unwrap_err()
            .into_runtime_error();
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::RecordedCallbackUnavailable
        );
        let Some(crate::RuntimeErrorCause::RecordedCallbackUnavailable { refusal }) = error.cause
        else {
            panic!("the presenter refusal remains typed");
        };
        assert_eq!(Some(refusal.recorded), plan.presenter);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let result = present(&session(Some("new"), &calls), &plan).await.unwrap();
        assert_eq!(
            result.model_return.parts[0],
            crate::ModelToolReturnPart::text("new".to_owned())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_explicitly_empty_plan_ignores_an_installed_presenter() {
        let calls = Arc::new(AtomicUsize::new(0));
        present(
            &session(Some("old"), &calls),
            &crate::runtime::PresentationBinding::default(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
