//! Plugin registration: `PluginSpec` (the declarative bundle of all a
//! plugin's hooks), the `PluginFactory` / `SessionPlugin` traits
//! plugin crates implement, and the two convenience factories
//! (`StaticPluginFactory`, `PluginSpecFactory`) + the `SpecPlugin`
//! glue that walks a spec and wires each field into the registrar.
//!
//! Split out of `plugin/mod.rs` for file size; outer path preserved by
//! `pub use` in `plugin/mod.rs`.

use std::sync::Arc;

use super::{
    AfterToolCallHook, AfterTurnHook, AssistantResponseHook, AssistantStreamFinishedHook,
    AssistantStreamHook, BeforeToolCallHook, BeforeTurnHook, CheckpointHook, ContextCompactor,
    ErasedPluginOperationInvokeFuture, PluginCommand, PluginCommandHandler, PluginError,
    PluginHost, PluginLifecycleEventHook, PluginOperationFailure, PluginOperationOutcome,
    PluginOperationRegistration, PluginOperationSpec, PluginQuery, PluginQueryHandler,
    PluginQueryInvokeFuture, PluginRegistrar, PluginSnapshotMeta, PluginTask, PluginTaskHandler,
    PromptContributor, SessionConfigMutator, SessionToolAccess, SnapshotReader, SnapshotWriter,
    SubagentSessionContext, ToolCatalogContributor, ToolResultProjector, TurnContextTransform,
};
use crate::{PluginOptions, ToolProvider};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginExtensionContribution {
    pub extension_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl PluginExtensionContribution {
    pub fn new(
        extension_id: impl Into<String>,
        payload: impl serde::Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            extension_id: extension_id.into(),
            payload: serde_json::to_value(payload)?,
        })
    }

    pub fn from_value(extension_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            extension_id: extension_id.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginExtensions {
    contributions: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
}

impl PluginExtensions {
    /// Builds a `PluginExtensions` from contributions data for protocol and process-engine
    /// implementors while preparing or executing plugin and tool work.
    pub fn from_contributions(
        contributions: impl IntoIterator<Item = PluginExtensionContribution>,
    ) -> Self {
        let mut extensions = Self::default();
        for contribution in contributions {
            extensions.insert(contribution);
        }
        extensions
    }

    /// Adds one extension payload for protocol implementors composing a shared extension set.
    pub fn insert(&mut self, contribution: PluginExtensionContribution) {
        self.contributions
            .entry(contribution.extension_id)
            .or_default()
            .push(contribution.payload);
    }

    /// Borrows every payload registered under an extension ID for protocol implementors applying
    /// that extension; an unknown ID yields an empty slice.
    pub fn payloads(&self, extension_id: &str) -> &[serde_json::Value] {
        self.contributions
            .get(extension_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Clone, Default)]
pub struct PluginSpec {
    pub tool_providers: Vec<Arc<dyn ToolProvider>>,
    #[doc(hidden)]
    pub orchestrating_tools: Vec<crate::tool_provider::orchestration::OrchestratingToolDef>,
    pub triggers: Vec<crate::TriggerEvent>,
    pub prompt_contributors: Vec<PromptContributor>,
    pub tool_catalog_contributors: Vec<ToolCatalogContributor>,
    pub before_turn_hooks: Vec<BeforeTurnHook>,
    pub before_tool_call_hooks: Vec<BeforeToolCallHook>,
    pub after_tool_call_hooks: Vec<AfterToolCallHook>,
    pub after_turn_hooks: Vec<AfterTurnHook>,
    pub checkpoint_hooks: Vec<CheckpointHook>,
    pub assistant_stream_hooks: Vec<AssistantStreamHook>,
    pub assistant_response_hooks: Vec<AssistantResponseHook>,
    pub assistant_stream_finished_hooks: Vec<AssistantStreamFinishedHook>,
    pub tool_result_projector: Option<ToolResultProjector>,
    pub runtime_event_hooks: Vec<PluginLifecycleEventHook>,
    pub session_config_mutators: Vec<SessionConfigMutator>,
    pub(crate) plugin_operations: Vec<PluginOperationRegistration>,
    pub turn_context_transforms: Vec<(i32, Arc<dyn TurnContextTransform>)>,
    pub context_compactors: Vec<(i32, Arc<dyn ContextCompactor>)>,
}

impl PluginSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tool_provider(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_providers.push(provider);
        self
    }

    /// Enable a completed first-party orchestrating tool definition in this
    /// host's plugin configuration.
    ///
    /// This is an **integrator class 3: protocol and process-engine
    /// implementor** seam. Use a first-party definition such as
    /// `lash_protocol_standard::standard_batch_orchestrating_tool`; external
    /// code cannot construct arbitrary orchestrating definitions.
    pub fn with_orchestrating_tool(
        mut self,
        definition: crate::tool_provider::orchestration::OrchestratingToolDef,
    ) -> Self {
        self.orchestrating_tools.push(definition);
        self
    }

    pub fn with_trigger_event(mut self, event: crate::TriggerEvent) -> Self {
        self.triggers.push(event);
        self
    }

    pub fn with_prompt_contributor(mut self, contributor: PromptContributor) -> Self {
        self.prompt_contributors.push(contributor);
        self
    }

    pub fn with_tool_catalog_contributor(mut self, contributor: ToolCatalogContributor) -> Self {
        self.tool_catalog_contributors.push(contributor);
        self
    }

    pub fn with_before_turn(mut self, hook: BeforeTurnHook) -> Self {
        self.before_turn_hooks.push(hook);
        self
    }

    pub fn with_before_tool_call(mut self, hook: BeforeToolCallHook) -> Self {
        self.before_tool_call_hooks.push(hook);
        self
    }

    pub fn with_after_tool_call(mut self, hook: AfterToolCallHook) -> Self {
        self.after_tool_call_hooks.push(hook);
        self
    }

    pub fn with_after_turn(mut self, hook: AfterTurnHook) -> Self {
        self.after_turn_hooks.push(hook);
        self
    }

    pub fn with_checkpoint(mut self, hook: CheckpointHook) -> Self {
        self.checkpoint_hooks.push(hook);
        self
    }

    pub fn with_assistant_stream(mut self, hook: AssistantStreamHook) -> Self {
        self.assistant_stream_hooks.push(hook);
        self
    }

    pub fn with_assistant_response(mut self, hook: AssistantResponseHook) -> Self {
        self.assistant_response_hooks.push(hook);
        self
    }

    pub fn with_assistant_stream_finished(mut self, hook: AssistantStreamFinishedHook) -> Self {
        self.assistant_stream_finished_hooks.push(hook);
        self
    }

    pub fn with_tool_result_projector(mut self, projector: ToolResultProjector) -> Self {
        self.tool_result_projector = Some(projector);
        self
    }

    pub fn with_runtime_event(mut self, hook: PluginLifecycleEventHook) -> Self {
        self.runtime_event_hooks.push(hook);
        self
    }

    pub fn with_session_config_mutator(mut self, hook: SessionConfigMutator) -> Self {
        self.session_config_mutators.push(hook);
        self
    }

    pub(crate) fn with_plugin_query(
        mut self,
        spec: PluginOperationSpec,
        handler: PluginQueryHandler,
    ) -> Self {
        self.plugin_operations
            .push(PluginOperationRegistration::query(spec, handler));
        self
    }

    pub fn with_plugin_query_typed<Op, F, Fut>(self, handler: F) -> Self
    where
        Op: PluginQuery,
        F: Fn(super::PluginQueryContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, PluginOperationFailure>>
            + Send
            + 'static,
    {
        self.with_plugin_query(
            super::plugin_operation_spec::<Op>(),
            Arc::new(move |ctx, args| {
                let parsed = serde_json::from_value::<Op::Args>(args);
                match parsed {
                    Ok(args) => {
                        let fut = handler(ctx, args);
                        Box::pin(async move {
                            let output = fut.await?;
                            serde_json::to_value(output).map_err(|err| {
                                PluginOperationFailure::new(format!(
                                    "failed to serialize {} output: {err}",
                                    Op::NAME
                                ))
                            })
                        }) as PluginQueryInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(PluginOperationFailure::new(format!(
                            "invalid {} args: {err}",
                            Op::NAME
                        )))
                    }) as PluginQueryInvokeFuture,
                }
            }),
        )
    }

    pub(crate) fn with_plugin_command(
        mut self,
        spec: PluginOperationSpec,
        handler: PluginCommandHandler,
    ) -> Self {
        self.plugin_operations
            .push(PluginOperationRegistration::command(spec, handler));
        self
    }

    pub fn with_plugin_command_typed<Op, F, Fut>(self, handler: F) -> Self
    where
        Op: PluginCommand,
        F: Fn(super::PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<
                Output = Result<PluginOperationOutcome<Op::Output>, PluginOperationFailure>,
            > + Send
            + 'static,
    {
        self.with_plugin_command(
            super::plugin_operation_spec::<Op>(),
            Arc::new(move |ctx, args| {
                let parsed = serde_json::from_value::<Op::Args>(args);
                match parsed {
                    Ok(args) => {
                        let fut = handler(ctx, args);
                        Box::pin(async move {
                            let outcome = fut.await?;
                            let output = serde_json::to_value(outcome.output).map_err(|err| {
                                PluginOperationFailure::new(format!(
                                    "failed to serialize {} output: {err}",
                                    Op::NAME
                                ))
                            })?;
                            Ok(super::actions::ErasedPluginOperationOutcome {
                                output,
                                events: outcome.events,
                                directives: outcome.directives,
                            })
                        }) as ErasedPluginOperationInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(PluginOperationFailure::new(format!(
                            "invalid {} args: {err}",
                            Op::NAME
                        )))
                    }) as ErasedPluginOperationInvokeFuture,
                }
            }),
        )
    }

    pub fn with_plugin_command_value<Op, F, Fut>(self, handler: F) -> Self
    where
        Op: PluginCommand,
        F: Fn(super::PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, PluginOperationFailure>>
            + Send
            + 'static,
    {
        self.with_plugin_command_typed::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginOperationOutcome::new) }
        })
    }

    pub(crate) fn with_plugin_task(
        mut self,
        spec: PluginOperationSpec,
        handler: PluginTaskHandler,
    ) -> Self {
        self.plugin_operations
            .push(PluginOperationRegistration::task(spec, handler));
        self
    }

    pub fn with_plugin_task_typed<Op, F, Fut>(self, handler: F) -> Self
    where
        Op: PluginTask,
        F: Fn(super::PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<
                Output = Result<PluginOperationOutcome<Op::Output>, PluginOperationFailure>,
            > + Send
            + 'static,
    {
        self.with_plugin_task(
            super::plugin_operation_spec::<Op>(),
            Arc::new(move |ctx, args| {
                let parsed = serde_json::from_value::<Op::Args>(args);
                match parsed {
                    Ok(args) => {
                        let fut = handler(ctx, args);
                        Box::pin(async move {
                            let outcome = fut.await?;
                            let output = serde_json::to_value(outcome.output).map_err(|err| {
                                PluginOperationFailure::new(format!(
                                    "failed to serialize {} output: {err}",
                                    Op::NAME
                                ))
                            })?;
                            Ok(super::actions::ErasedPluginOperationOutcome {
                                output,
                                events: outcome.events,
                                directives: outcome.directives,
                            })
                        }) as ErasedPluginOperationInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(PluginOperationFailure::new(format!(
                            "invalid {} args: {err}",
                            Op::NAME
                        )))
                    }) as ErasedPluginOperationInvokeFuture,
                }
            }),
        )
    }

    pub fn with_plugin_task_value<Op, F, Fut>(self, handler: F) -> Self
    where
        Op: PluginTask,
        F: Fn(super::PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, PluginOperationFailure>>
            + Send
            + 'static,
    {
        self.with_plugin_task_typed::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginOperationOutcome::new) }
        })
    }

    pub fn with_turn_context_transform(
        mut self,
        priority: i32,
        transform: Arc<dyn TurnContextTransform>,
    ) -> Self {
        self.turn_context_transforms.push((priority, transform));
        self
    }

    pub fn with_context_compactor(
        mut self,
        priority: i32,
        compactor: Arc<dyn ContextCompactor>,
    ) -> Self {
        self.context_compactors.push((priority, compactor));
        self
    }
}

#[derive(Clone, Debug)]
pub struct PluginSessionContext {
    pub session_id: String,
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub plugin_options: PluginOptions,
    /// Protocol-owned options loaded from durable session state, when any.
    pub protocol_turn_options: crate::ProtocolTurnOptions,
    pub extensions: PluginExtensions,
    /// Session id of the caller that created this one. `None` identifies
    /// a root session; any subagent / compaction / forked-child session
    /// carries the parent here so plugin factories can gate themselves
    /// on root-only behavior (e.g. `update_plan`'s sticky plan dock).
    pub parent_session_id: Option<String>,
}

impl PluginSessionContext {
    /// Returns `true` when this context represents a root session, not a
    /// subagent or internal child. Plugins that should only surface in
    /// user-facing top-level turns check this in their `build`.
    pub fn is_root_session(&self) -> bool {
        self.parent_session_id.is_none()
    }
}

#[derive(Clone)]
pub struct SessionReadyContext {
    pub session_id: String,
    pub host: PluginHost,
}

pub trait SessionPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn version(&self) -> &'static str {
        "1"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError>;

    fn snapshot(
        &self,
        _writer: &mut dyn SnapshotWriter,
    ) -> Result<PluginSnapshotMeta, PluginError> {
        Ok(PluginSnapshotMeta {
            plugin_id: self.id().to_string(),
            plugin_version: self.version().to_string(),
            revision: self.snapshot_revision(),
            state: None,
        })
    }

    fn snapshot_revision(&self) -> u64 {
        0
    }

    fn restore(
        &self,
        _meta: &PluginSnapshotMeta,
        _reader: &dyn SnapshotReader,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn session_ready(&self, _ctx: SessionReadyContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Registers a plugin with the runtime and produces a per-session
/// `SessionPlugin` instance for each new session.
///
/// # Cheap-build / stateful-factory contract
///
/// `build(ctx)` **must be cheap**. It runs on the hot path every time
/// a new session is created (including subagents, forked children,
/// and compaction children) and any latency here is paid per session.
///
/// Specifically, `build` must **not**:
/// - perform any I/O (disk reads, HTTP calls, DB queries),
/// - compile regexes, templates, or schemas,
/// - open network connections or initialize connection pools,
/// - load models, parse large config files, or allocate large buffers,
/// - block the current thread for non-trivial work.
///
/// Expensive state belongs on the `PluginFactory` struct itself,
/// wrapped in `Arc` so it can be cheaply cloned into per-session
/// closures. The `PluginFactory` is constructed once by the embedder
/// and held in the `RuntimeEnvironment`; its fields outlive every
/// session. Hooks captured into a `PluginSpec` are closures that clone
/// the `Arc`s off `self` and reference the shared state directly, so
/// every session sees the same pool / cache / compiled artifact without
/// rebuilding it. Factories that own host-visible resources also
/// participate in the explicit post-intake lifecycle through
/// [`shutdown`](Self::shutdown).
///
/// The typical shape is:
/// ```ignore
/// pub struct MyFactory {
///     pool: Arc<ConnectionPool>,          // expensive, built once
///     compiled: Arc<Regex>,               // expensive, built once
/// }
///
/// impl PluginFactory for MyFactory {
///     fn id(&self) -> &'static str { "my_plugin" }
///
///     fn build(&self, _ctx: &PluginSessionContext)
///         -> Result<Arc<dyn SessionPlugin>, PluginError>
///     {
///         // Cheap: clone Arcs, assemble spec, wrap in SpecPlugin.
///         let pool = Arc::clone(&self.pool);
///         let spec = PluginSpec::new().with_before_turn(Arc::new(move |_ctx| {
///             let pool = Arc::clone(&pool);
///             Box::pin(async move { /* use pool */ Ok(vec![]) })
///         }));
///         Ok(Arc::new(SpecPluginFromSpec::new("my_plugin", spec)))
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait PluginFactory: Send + Sync {
    fn id(&self) -> &'static str;

    /// Release host-visible resources owned by this factory after intake stops.
    ///
    /// Hosts call this before process exit so resources such as child processes,
    /// connections, and background tasks are explicitly released. It takes
    /// `&self` because reusable factory state lives behind its own
    /// synchronization and is commonly shared with every session plugin. Lash
    /// does not stop intake, drain, or abort turns as part of plugin shutdown.
    /// Implementations must be idempotent and bound their cleanup; the first-party
    /// MCP implementation's per-entry bound is rmcp's three-second cancellation
    /// grace plus transport-task drain. The default is a no-op for factories
    /// without host-visible resources.
    async fn shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }

    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        Vec::new()
    }

    /// Host-level contribution of [`ProcessEngine`](crate::ProcessEngine)s,
    /// mirroring [`extension_contributions`](Self::extension_contributions).
    ///
    /// After the plugin host is built, core asks each factory for the process
    /// engines it wants registered, passing a read-only host context (the built
    /// plugin extensions, the runtime trace context, and whether process
    /// lifecycle is available). The returned engines land in the runtime host's
    /// [`ProcessEngineRegistry`](crate::runtime::ProcessEngineRegistry) with
    /// unique [`ProcessEngine::kind`](crate::ProcessEngine::kind) enforced.
    ///
    /// The default contributes nothing, so most plugins never implement this.
    fn process_engine_contributions(
        &self,
        _ctx: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<Arc<dyn crate::ProcessEngine>>, PluginError> {
        Ok(Vec::new())
    }

    /// Produce a session-scoped plugin. **Must be cheap** — see the
    /// trait-level docs for the full contract.
    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError>;
}

/// Read-only host context handed to
/// [`PluginFactory::process_engine_contributions`]. Exposes the built
/// plugin-host extensions (the same data
/// [`PluginHost::extensions`](super::PluginHost::extensions) returns), the
/// runtime trace context, and whether process lifecycle is available on this
/// deployment (i.e. a process registry is wired).
///
/// Integrator class (ADR 0051): **protocol and process-engine implementors**.
/// This is the argument of the only method that yields
/// [`ProcessEngine`](crate::ProcessEngine)s, so it is named by whoever ships an
/// engine — never by a host, which acquires engines by installing the plugin
/// that contributes them. It therefore has no facade home by design.
pub struct ProcessEngineContributionContext<'a> {
    extensions: &'a PluginExtensions,
    trace_context: &'a crate::TraceContext,
    process_lifecycle_available: bool,
}

impl<'a> ProcessEngineContributionContext<'a> {
    /// Constructs a `ProcessEngineContributionContext` for protocol and process-engine implementors
    /// while implementing `PluginFactory::process_engine_contributions` (process engine
    /// contributions).
    pub fn new(
        extensions: &'a PluginExtensions,
        trace_context: &'a crate::TraceContext,
        process_lifecycle_available: bool,
    ) -> Self {
        Self {
            extensions,
            trace_context,
            process_lifecycle_available,
        }
    }

    /// Exposes extensions to protocol and process-engine implementors while implementing
    /// `PluginFactory::process_engine_contributions` (process engine contributions).
    pub fn extensions(&self) -> &PluginExtensions {
        self.extensions
    }

    /// Exposes trace context to protocol and process-engine implementors while implementing
    /// `PluginFactory::process_engine_contributions` (process engine contributions).
    pub fn trace_context(&self) -> &crate::TraceContext {
        self.trace_context
    }

    /// Tells plugin factories whether the host supplied the lifecycle services required to
    /// contribute a runnable process engine.
    pub fn process_lifecycle_available(&self) -> bool {
        self.process_lifecycle_available
    }
}

pub type PluginSpecBuilder =
    Arc<dyn Fn(&PluginSessionContext) -> Result<PluginSpec, PluginError> + Send + Sync>;

pub struct PluginSpecFactory {
    id: &'static str,
    builder: PluginSpecBuilder,
}

impl PluginSpecFactory {
    pub fn new(id: &'static str, builder: PluginSpecBuilder) -> Self {
        Self { id, builder }
    }
}

pub struct StaticPluginFactory {
    id: &'static str,
    spec: PluginSpec,
}

impl StaticPluginFactory {
    pub fn new(id: &'static str, spec: PluginSpec) -> Self {
        Self { id, spec }
    }
}

struct SpecPlugin {
    id: &'static str,
    spec: PluginSpec,
}

impl PluginFactory for PluginSpecFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(SpecPlugin {
            id: self.id,
            spec: (self.builder)(ctx)?,
        }))
    }
}

impl PluginFactory for StaticPluginFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(SpecPlugin {
            id: self.id,
            spec: self.spec.clone(),
        }))
    }
}

impl SessionPlugin for SpecPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        for provider in &self.spec.tool_providers {
            reg.tools().provider(Arc::clone(provider))?;
        }
        for definition in &self.spec.orchestrating_tools {
            reg.tools().orchestrating(definition.clone())?;
        }
        for event in &self.spec.triggers {
            reg.triggers().declare(event.clone())?;
        }
        for contributor in &self.spec.prompt_contributors {
            reg.prompt().contribute(Arc::clone(contributor));
        }
        for contributor in &self.spec.tool_catalog_contributors {
            reg.tool_catalog().contribute(Arc::clone(contributor));
        }
        for hook in &self.spec.before_turn_hooks {
            reg.turn().before(Arc::clone(hook));
        }
        for hook in &self.spec.before_tool_call_hooks {
            reg.tool_calls().before(Arc::clone(hook));
        }
        for hook in &self.spec.after_tool_call_hooks {
            reg.tool_calls().after(Arc::clone(hook));
        }
        for hook in &self.spec.after_turn_hooks {
            reg.turn().after(Arc::clone(hook));
        }
        for hook in &self.spec.checkpoint_hooks {
            reg.turn().checkpoint(Arc::clone(hook));
        }
        for hook in &self.spec.assistant_stream_hooks {
            reg.output().stream(Arc::clone(hook));
        }
        for hook in &self.spec.assistant_response_hooks {
            reg.output().response(Arc::clone(hook));
        }
        for hook in &self.spec.assistant_stream_finished_hooks {
            reg.output().stream_finished(Arc::clone(hook));
        }
        if let Some(projector) = &self.spec.tool_result_projector {
            reg.tool_results().projector(Arc::clone(projector))?;
        }
        for hook in &self.spec.runtime_event_hooks {
            reg.session().on_event(Arc::clone(hook));
        }
        for hook in &self.spec.session_config_mutators {
            reg.session().config_mutator(Arc::clone(hook));
        }
        for operation in &self.spec.plugin_operations {
            reg.operations().register(operation.clone())?;
        }
        for (priority, transform) in &self.spec.turn_context_transforms {
            reg.context().prepare_turn(*priority, Arc::clone(transform));
        }
        for (priority, compactor) in &self.spec.context_compactors {
            reg.context().compact(*priority, Arc::clone(compactor));
        }
        Ok(())
    }
}
