//! Plugin registration: `PluginSpec` (the declarative bundle of all a
//! plugin's hooks), the `PluginFactory` / `SessionPlugin` traits
//! plugin crates implement, and the two convenience factories
//! (`StaticPluginFactory`, `PluginSpecFactory`) + the `SpecPlugin`
//! glue that walks a spec and wires each field into the registrar.

use std::sync::Arc;

use super::{
    AfterTurnHook, AssistantResponseHook, AssistantStreamFinishedHook, AssistantStreamHook,
    BeforeTurnHook, CheckpointHook, ContextCompactor, ContextPressureHook,
    ErasedPluginOperationInvokeFuture, HookKey, PluginCommand, PluginCommandHandler, PluginError,
    PluginHost, PluginLifecycleEventHook, PluginOperationOutcome, PluginOperationRegistration,
    PluginOperationSpec, PluginQuery, PluginQueryHandler, PluginQueryInvokeFuture, PluginRegistrar,
    PluginTask, PluginTaskHandler, SessionToolAccess, ToolArgsCheckHook, ToolArgsTransformHook,
    ToolCatalogContributor, ToolPresentationStep, ToolResultCheckHook, ToolResultTransformHook,
};
use crate::ToolProvider;

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
    pub fn from_contributions(
        contributions: impl IntoIterator<Item = PluginExtensionContribution>,
    ) -> Self {
        let mut extensions = Self::default();
        for contribution in contributions {
            extensions.insert(contribution);
        }
        extensions
    }

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
    pub extension_contributions: Vec<PluginExtensionContribution>,
    pub tool_providers: Vec<Arc<dyn ToolProvider>>,
    pub triggers: Vec<crate::TriggerEvent>,
    pub tool_catalog_contributors: Vec<(HookKey, ToolCatalogContributor)>,
    pub before_turn_hooks: Vec<(HookKey, BeforeTurnHook)>,
    pub tool_args_transforms: Vec<(HookKey, ToolArgsTransformHook)>,
    pub tool_args_checks: Vec<(HookKey, ToolArgsCheckHook)>,
    pub tool_result_transforms: Vec<(HookKey, ToolResultTransformHook)>,
    pub tool_result_checks: Vec<(HookKey, ToolResultCheckHook)>,
    pub after_turn_hooks: Vec<(HookKey, AfterTurnHook)>,
    pub checkpoint_hooks: Vec<(HookKey, CheckpointHook)>,
    pub assistant_stream_hooks: Vec<(HookKey, AssistantStreamHook)>,
    /// Response callbacks, each with the stream-finished key whose state it
    /// receives.
    pub assistant_response_hooks: Vec<(HookKey, Option<HookKey>, AssistantResponseHook)>,
    pub assistant_stream_finished_hooks: Vec<(HookKey, AssistantStreamFinishedHook)>,
    /// Composable presentation steps, applied in list order (FIG-3420).
    pub presentation_steps: Vec<(HookKey, ToolPresentationStep)>,
    pub runtime_event_hooks: Vec<(HookKey, PluginLifecycleEventHook)>,
    pub(crate) plugin_operations: Vec<PluginOperationRegistration>,
    pub context_compactors: Vec<(i32, Arc<dyn ContextCompactor>)>,
    pub context_pressure_hooks: Vec<(i32, Arc<dyn ContextPressureHook>)>,
    /// The pure reducers a [`StateCommands::apply`](super::StateCommands::apply)
    /// names, by name (K10).
    pub state_reducers: Vec<(String, super::StateReducer)>,
}

impl PluginSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_extension_contribution(
        mut self,
        contribution: PluginExtensionContribution,
    ) -> Self {
        self.extension_contributions.push(contribution);
        self
    }

    pub fn with_tool_provider(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_providers.push(provider);
        self
    }

    pub fn with_trigger_event(mut self, event: crate::TriggerEvent) -> Self {
        self.triggers.push(event);
        self
    }

    /// The spec's keyed entries register as [`PluginRegistrar`] calls; a
    /// duplicate key in one seam fails registration.
    pub fn with_tool_catalog_contributor(
        mut self,
        key: HookKey,
        contributor: ToolCatalogContributor,
    ) -> Self {
        self.tool_catalog_contributors.push((key, contributor));
        self
    }

    pub fn with_before_turn(mut self, key: HookKey, hook: BeforeTurnHook) -> Self {
        self.before_turn_hooks.push((key, hook));
        self
    }

    pub fn with_tool_args_transform(mut self, key: HookKey, hook: ToolArgsTransformHook) -> Self {
        self.tool_args_transforms.push((key, hook));
        self
    }

    pub fn with_tool_args_check(mut self, key: HookKey, hook: ToolArgsCheckHook) -> Self {
        self.tool_args_checks.push((key, hook));
        self
    }

    pub fn with_tool_result_transform(
        mut self,
        key: HookKey,
        hook: ToolResultTransformHook,
    ) -> Self {
        self.tool_result_transforms.push((key, hook));
        self
    }

    pub fn with_tool_result_check(mut self, key: HookKey, hook: ToolResultCheckHook) -> Self {
        self.tool_result_checks.push((key, hook));
        self
    }

    pub fn with_after_turn(mut self, key: HookKey, hook: AfterTurnHook) -> Self {
        self.after_turn_hooks.push((key, hook));
        self
    }

    pub fn with_checkpoint(mut self, key: HookKey, hook: CheckpointHook) -> Self {
        self.checkpoint_hooks.push((key, hook));
        self
    }

    pub fn with_assistant_stream(mut self, key: HookKey, hook: AssistantStreamHook) -> Self {
        self.assistant_stream_hooks.push((key, hook));
        self
    }

    pub fn with_assistant_response(
        mut self,
        key: HookKey,
        stream_state_from: Option<HookKey>,
        hook: AssistantResponseHook,
    ) -> Self {
        self.assistant_response_hooks
            .push((key, stream_state_from, hook));
        self
    }

    pub fn with_assistant_stream_finished(
        mut self,
        key: HookKey,
        hook: AssistantStreamFinishedHook,
    ) -> Self {
        self.assistant_stream_finished_hooks.push((key, hook));
        self
    }

    /// Appends one composable presentation step (FIG-3420). Steps run in the
    /// order `with_presentation_step` calls list them.
    pub fn with_presentation_step(mut self, key: HookKey, step: ToolPresentationStep) -> Self {
        self.presentation_steps.push((key, step));
        self
    }

    pub fn with_runtime_event(mut self, key: HookKey, hook: PluginLifecycleEventHook) -> Self {
        self.runtime_event_hooks.push((key, hook));
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
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        self.with_plugin_query(
            super::plugin_operation_spec::<Op>(),
            Arc::new(move |ctx, args| {
                let parsed = serde_json::from_value::<Op::Args>(args);
                match parsed {
                    Ok(args) => {
                        let fut = handler(ctx, args);
                        Box::pin(async move {
                            let output =
                                fut.await.map_err(super::declared_operation_failure::<Op>)?;
                            serde_json::to_value(output).map_err(|err| {
                                super::operation_protocol_failure(format!(
                                    "failed to serialize {} output: {err}",
                                    Op::NAME
                                ))
                            })
                        }) as PluginQueryInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(super::operation_protocol_failure(format!(
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
        Fut: std::future::Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>>
            + Send
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
                            let outcome =
                                fut.await.map_err(super::declared_operation_failure::<Op>)?;
                            let output = serde_json::to_value(outcome.output).map_err(|err| {
                                super::operation_protocol_failure(format!(
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
                        Err(super::operation_protocol_failure(format!(
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
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
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
        Fut: std::future::Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>>
            + Send
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
                            let outcome =
                                fut.await.map_err(super::declared_operation_failure::<Op>)?;
                            let output = serde_json::to_value(outcome.output).map_err(|err| {
                                super::operation_protocol_failure(format!(
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
                        Err(super::operation_protocol_failure(format!(
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
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        self.with_plugin_task_typed::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginOperationOutcome::new) }
        })
    }

    pub fn with_context_compactor(
        mut self,
        priority: i32,
        compactor: Arc<dyn ContextCompactor>,
    ) -> Self {
        self.context_compactors.push((priority, compactor));
        self
    }

    pub fn with_context_pressure_hook(
        mut self,
        priority: i32,
        hook: Arc<dyn ContextPressureHook>,
    ) -> Self {
        self.context_pressure_hooks.push((priority, hook));
        self
    }

    pub fn with_state_reducer(
        mut self,
        name: impl Into<String>,
        reducer: super::StateReducer,
    ) -> Self {
        self.state_reducers.push((name.into(), reducer));
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginSessionMaterialization {
    Creation,
    Rematerialization,
}

/// The telemetry capability of one engine execution, shared with plugin observers.
/// Clones retain the engine's scope and permission to emit.
#[derive(Clone)]
pub struct PluginExecutionTrace {
    standing: crate::trace::TraceStanding,
}

impl PluginExecutionTrace {
    pub fn new(standing: crate::trace::TraceStanding) -> Self {
        Self { standing }
    }

    pub fn into_standing(self) -> crate::trace::TraceStanding {
        self.standing
    }

    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        self.standing.runtime()
    }

    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.standing.scope()
    }

    pub fn observes_language(&self) -> bool {
        self.trace_runtime().emitter().product_observer().is_some() || self.standing.is_observed()
    }

    pub fn emit(&self, record: impl FnOnce() -> (crate::TraceContext, crate::TraceEvent)) {
        self.standing.observe(record);
    }

    /// Reconstruct the product graph on replay, independently of external telemetry.
    pub fn observe_language(
        &self,
        event_key: &str,
        record: impl Fn() -> (crate::TraceContext, crate::TraceEvent),
    ) {
        self.trace_runtime().emitter().observe_product(|| {
            let (context, event) = record();
            lash_trace::TraceRecord {
                schema_version: lash_trace::TRACE_SCHEMA_VERSION,
                id: event_key.to_string(),
                timestamp: self.trace_runtime().clock().timestamp_datetime(),
                context,
                event,
            }
        });
        self.emit(record);
    }
}

#[derive(Clone, Debug)]
pub struct PluginSessionContext {
    pub tracing: crate::trace::TraceRuntime,
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// Who the plugin session is built for: a session, or a process runtime
    /// built from its captured execution environment.
    pub owner: crate::RuntimeOwner,
    pub tool_access: SessionToolAccess,
    /// The session's recorded plugin configuration at this build (FIG-4379):
    /// what it was created with or last patched to, or a process's captured
    /// configuration. It is the value at build time only: a hook reads the
    /// configuration its run was admitted under from its own context, so a
    /// hook closure must not keep a copy of this.
    pub plugin_config: super::AdmittedPluginConfig,
    /// Whether factories are constructing a new session or rebuilding one
    /// whose plugin snapshot and configuration were already recorded.
    pub materialization: PluginSessionMaterialization,
    pub extensions: PluginExtensions,
}

impl PluginSessionContext {
    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        &self.tracing
    }

    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.trace.as_ref()
    }
}

#[derive(Clone)]
pub struct SessionReadyContext {
    pub tracing: crate::trace::TraceRuntime,
    pub trace: Option<lash_trace::DurableTraceScope>,
    pub state: super::PluginStateView,
    pub owner: crate::RuntimeOwner,
    pub host: PluginHost,
}

impl SessionReadyContext {
    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        &self.tracing
    }

    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.trace.as_ref()
    }
}

pub use lash_core_ids::{BehaviorRevision, FormatVersion, PluginId};

/// What a plugin declares about itself before any session is built
/// (FIG-4732): its id, the revision of what it does, the format it reads
/// natively and the formats it can write. Lash trusts the declaration; it
/// hashes no plugin content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDeclaration {
    /// The id the plugin registers under.
    pub id: PluginId,
    /// Moves with any change in what the plugin does.
    pub behavior_revision: BehaviorRevision,
    /// The format of stored state and config the plugin reads natively.
    pub format_version: FormatVersion,
    /// Every format the plugin can write. It contains `format_version`.
    pub writable_formats: Vec<FormatVersion>,
    /// What a fork of a session does with the plugin's namespace: copy it
    /// as of the fork point, or reset it to the plugin's initial state.
    /// Recorded with the namespace, so a later fork follows it.
    pub state_fork: crate::plugin::StateFork,
}

impl PluginDeclaration {
    /// The declaration of a plugin at its first behaviour revision that
    /// reads and writes only its first format.
    pub fn initial(id: &'static str) -> Self {
        Self {
            id: PluginId::new(id),
            behavior_revision: BehaviorRevision::ONE,
            format_version: FormatVersion::ONE,
            writable_formats: vec![FormatVersion::ONE],
            state_fork: crate::plugin::StateFork::Copy,
        }
    }
}

/// Why a factory's [`PluginDeclaration`] is refused.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum PluginDeclarationError {
    /// The declaration names another plugin than the factory that gave it.
    #[error("plugin factory `{factory}` declares itself as `{declared}`")]
    IdMismatch { factory: String, declared: String },
    /// The plugin cannot write the format it reads natively.
    #[error(
        "plugin `{plugin}` reads format {format_version} natively and does not declare it writable"
    )]
    NativeFormatNotWritable {
        plugin: String,
        #[schemars(with = "std::num::NonZeroU32")]
        format_version: FormatVersion,
    },
}

/// The plugins of one core in hook order: the order their factories are
/// registered in, which is the order their hooks run in. The build
/// generation folds it in (FIG-4744), so two cores whose plugins differ in
/// any behaviour revision, or only in order, never share a lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginComposition {
    declarations: Vec<PluginDeclaration>,
}

impl PluginComposition {
    /// Validate declarations in hook order, without attaching runtime dependencies.
    pub fn new(
        declarations: impl IntoIterator<Item = PluginDeclaration>,
    ) -> Result<Self, PluginDeclarationError> {
        let declarations: Vec<_> = declarations.into_iter().collect();
        for declaration in &declarations {
            if !declaration
                .writable_formats
                .contains(&declaration.format_version)
            {
                return Err(PluginDeclarationError::NativeFormatNotWritable {
                    plugin: declaration.id.as_str().to_owned(),
                    format_version: declaration.format_version,
                });
            }
        }
        Ok(Self { declarations })
    }

    /// A Core deployment's composition: builtins followed by the supplied
    /// protocol and host declarations. Supplied ids replace matching builtins.
    /// Include `embed_tools` when registering tool providers on the Core builder.
    pub fn with_builtins(
        declarations: impl IntoIterator<Item = PluginDeclaration>,
    ) -> Result<Self, PluginDeclarationError> {
        let declarations: Vec<_> = declarations.into_iter().collect();
        let mut builtins = super::builtin_plugin_declarations();
        builtins.retain(|builtin| {
            !declarations
                .iter()
                .any(|declared| declared.id == builtin.id)
        });
        builtins.extend(declarations);
        Self::new(builtins)
    }

    /// Every plugin's declaration, in hook order.
    pub fn declarations(&self) -> &[PluginDeclaration] {
        &self.declarations
    }

    /// What each plugin declares about the formats it writes, in hook
    /// order: what the fleet record's writer ranges are provisioned from.
    pub fn writer_registrations(
        &self,
    ) -> Vec<crate::store::plugin_writers::PluginWriterRegistration> {
        self.declarations
            .iter()
            .map(
                |declaration| crate::store::plugin_writers::PluginWriterRegistration {
                    plugin: declaration.id.as_str().to_owned(),
                    native: declaration.format_version,
                    writable: declaration.writable_formats.clone(),
                },
            )
            .collect()
    }

    /// The admission of this composition under `ranges` (FIG-4747): every
    /// plugin in hook order with its behaviour revision and the highest
    /// format it writes that the fleet record permits.
    ///
    /// # Errors
    /// [`CompatRefusal`](crate::compat::CompatRefusal) for a plugin the
    /// record does not name or that writes no permitted format.
    pub fn admission(
        &self,
        ranges: &crate::store::plugin_writers::PluginWriterRanges,
    ) -> Result<crate::store::plugin_writers::PluginAdmission, crate::compat::CompatRefusal> {
        let registrations = self.writer_registrations();
        crate::store::plugin_writers::PluginAdmission::choose(
            registrations.iter().zip(
                self.declarations
                    .iter()
                    .map(|declaration| declaration.behavior_revision),
            ),
            ranges,
        )
    }
}

pub trait SessionPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError>;

    /// Per-session mirror of [`PluginFactory::extension_contributions`].
    ///
    /// Collected once per session after [`register`](Self::register), so a
    /// session plugin can derive extensions from the session's own plugin
    /// options — on a durable-process session, the process's execution env
    /// spec. Must be cheap and perform no I/O.
    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        Vec::new()
    }

    fn session_ready(&self, _ctx: SessionReadyContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// A plugin's identity, behavior revision and formats, independent of its
/// runtime dependencies. Production factory types implement this once; hosts
/// can call it before opening stores, pools or connections. It must be pure,
/// cheap and perform no I/O. Move the behavior revision whenever behavior
/// changes; the declared native format must also be writable.
pub trait PluginDefinition {
    fn declaration() -> PluginDeclaration;
}

/// Object-safe access to a declaration. For statically defined plugins the
/// blanket implementation reads [`PluginDefinition`], so boot and offline
/// generation use exactly the same declaration. Configured spec factories
/// return the declaration supplied separately from their runtime hooks.
/// This access must be pure and perform no I/O; runtime dependencies must not
/// determine the declaration. Prefer the static definition for factory types.
pub trait PluginMetadata {
    fn plugin_declaration(&self) -> PluginDeclaration;
}

impl<T: PluginDefinition> PluginMetadata for T {
    fn plugin_declaration(&self) -> PluginDeclaration {
        T::declaration()
    }
}

/// # Cheap-build / stateful-factory contract
///
/// `build(ctx)` **must be cheap**. It runs on the hot path every time
/// a new session is created (children and forks included)
/// and any latency here is paid per session.
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
/// impl PluginDefinition for MyFactory {
///     fn declaration() -> PluginDeclaration {
///         PluginDeclaration::initial("my_plugin")
///     }
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
pub trait PluginFactory: PluginMetadata + Send + Sync {
    /// Pure display extension. Available to durable readers without plugin
    /// materialization, state restoration, effects or a session writer.
    fn transcript_projector(&self) -> Option<Arc<dyn super::TranscriptRowProjectorPlugin>> {
        None
    }

    fn id(&self) -> &'static str;

    /// Pure initial values for a namespace absent from the recorded base.
    /// The engine records this result before any capability is constructed.
    fn initialize_state(
        &self,
        _owner: &crate::RuntimeOwner,
        _config: &super::PluginConfig,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, PluginError> {
        Ok(std::collections::BTreeMap::new())
    }

    /// Pure conversion of this namespace into the factory's native format.
    /// No I/O or access to other namespaces is permitted.
    fn migrate_format(
        &self,
        from: super::FormatVersion,
        namespace: super::FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, super::FormatRefusal> {
        if from == crate::plugin::PluginMetadata::plugin_declaration(self).format_version {
            Ok(value)
        } else {
            Err(super::FormatRefusal {
                plugin: self.id().into(),
                namespace,
                stored: from,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            })
        }
    }

    /// Pure encoding into the writer version supplied by the admission.
    /// Encoding the native format must preserve the value unchanged.
    fn encode_format(
        &self,
        to: super::FormatVersion,
        namespace: super::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, super::FormatRefusal> {
        if to == crate::plugin::PluginMetadata::plugin_declaration(self).format_version {
            Ok(value.clone())
        } else {
            Err(super::FormatRefusal {
                plugin: self.id().into(),
                namespace,
                stored: to,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            })
        }
    }

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

    /// Register this plugin's recorded config namespace and the typed
    /// commands that change it (FIG-4379), before any session exists.
    ///
    /// The owner creates the namespace at every session's creation — from
    /// the creator's input and its defaults, never another session's — and
    /// validates every candidate; each command is one change the namespace
    /// admits, and a setting no command changes is immutable. Every open
    /// delivers the recorded namespace unchanged, in
    /// [`PluginSessionContext::plugin_config`] and in each scoped hook's
    /// context. The default registers nothing: the plugin records no config.
    fn register_config(
        &self,
        _reg: &mut super::ConfigRegistrar,
    ) -> Result<(), super::ConfigRegistrationError> {
        Ok(())
    }

    /// The [`Backend::binding_identity`](crate::Backend::binding_identity) of
    /// the backend whose stores this factory keeps state in, when it keeps
    /// any: the RLM protocol keeps its Lashlang module artifacts there.
    ///
    /// A runtime built over one backend refuses a factory bound to another,
    /// so no plugin writes state into a substrate the runtime does not reopen
    /// or sweep (ADR 0102, D2). The default binds to none. A factory that
    /// wraps another forwards the inner factory's answer.
    fn bound_backend(&self) -> Option<&str> {
        None
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
    ) -> Result<Vec<crate::ProcessEngineRegistration>, PluginError> {
        Ok(Vec::new())
    }

    /// Produce a session-scoped plugin. **Must be cheap** — see the
    /// trait-level docs for the full contract.
    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError>;
}

/// Exposes the built plugin-host extensions (the same data
/// [`PluginHost::extensions`](super::PluginHost::extensions) returns), the runtime trace
/// context, and whether process lifecycle is available on this deployment (i.e. a process
/// registry is wired).
///
/// Integrator class (ADR 0051): **protocol and process-engine implementors**.
/// This is the argument of the only method that yields
/// [`ProcessEngine`](crate::ProcessEngine)s, so it is named by whoever ships an
/// engine, and by a host whose factory wraps one that does (the RLM protocol
/// factory, say): the wrapper forwards it, or the wrapped factory's engines
/// are never contributed. The facade exports it as
/// `lash::plugins::ProcessEngineContributionContext` (FIG-4373).
pub struct ProcessEngineContributionContext<'a> {
    plugin_host: &'a super::PluginHost,
    trace_runtime: &'a crate::trace::TraceRuntime,
    process_lifecycle_available: bool,
}

impl<'a> ProcessEngineContributionContext<'a> {
    pub fn new(
        plugin_host: &'a super::PluginHost,
        trace_runtime: &'a crate::trace::TraceRuntime,
        process_lifecycle_available: bool,
    ) -> Self {
        Self {
            plugin_host,
            trace_runtime,
            process_lifecycle_available,
        }
    }

    pub fn extensions(&self) -> &PluginExtensions {
        self.plugin_host.extensions()
    }

    /// The host whose factories supply creation-time process resource grants.
    pub fn plugin_host(&self) -> &super::PluginHost {
        self.plugin_host
    }

    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        self.trace_runtime
    }

    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        None
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
    declaration: PluginDeclaration,
    builder: PluginSpecBuilder,
}

impl PluginSpecFactory {
    pub fn new(declaration: PluginDeclaration, builder: PluginSpecBuilder) -> Self {
        Self {
            declaration,
            builder,
        }
    }
}

pub struct StaticPluginFactory {
    declaration: PluginDeclaration,
    spec: PluginSpec,
}

impl StaticPluginFactory {
    pub fn new(declaration: PluginDeclaration, spec: PluginSpec) -> Self {
        Self { declaration, spec }
    }
}

struct SpecPlugin {
    id: &'static str,
    spec: PluginSpec,
}

impl PluginFactory for PluginSpecFactory {
    fn id(&self) -> &'static str {
        self.declaration.id.as_str()
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(SpecPlugin {
            id: self.id(),
            spec: (self.builder)(ctx)?,
        }))
    }
}

impl crate::plugin::PluginMetadata for PluginSpecFactory {
    fn plugin_declaration(&self) -> PluginDeclaration {
        self.declaration.clone()
    }
}

impl PluginFactory for StaticPluginFactory {
    fn id(&self) -> &'static str {
        self.declaration.id.as_str()
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(SpecPlugin {
            id: self.id(),
            spec: self.spec.clone(),
        }))
    }
}

impl crate::plugin::PluginMetadata for StaticPluginFactory {
    fn plugin_declaration(&self) -> PluginDeclaration {
        self.declaration.clone()
    }
}

impl SessionPlugin for SpecPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        self.spec.extension_contributions.clone()
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        for provider in &self.spec.tool_providers {
            reg.tools().provider(Arc::clone(provider))?;
        }
        for event in &self.spec.triggers {
            reg.triggers().declare(event.clone())?;
        }
        for (key, contributor) in &self.spec.tool_catalog_contributors {
            reg.tool_catalog()
                .contribute(*key, Arc::clone(contributor))?;
        }
        for (key, hook) in &self.spec.before_turn_hooks {
            reg.turn().before(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.tool_args_transforms {
            reg.tool_calls().transform_args(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.tool_args_checks {
            reg.tool_calls().check_args(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.tool_result_transforms {
            reg.tool_calls().transform_result(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.tool_result_checks {
            reg.tool_calls().check_result(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.after_turn_hooks {
            reg.turn().after(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.checkpoint_hooks {
            reg.turn().checkpoint(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.assistant_stream_hooks {
            reg.output().stream(*key, Arc::clone(hook))?;
        }
        for (key, hook) in &self.spec.assistant_stream_finished_hooks {
            reg.output().stream_finished(*key, Arc::clone(hook))?;
        }
        for (key, stream_state_from, hook) in &self.spec.assistant_response_hooks {
            reg.output()
                .response(*key, *stream_state_from, Arc::clone(hook))?;
        }
        for (key, step) in &self.spec.presentation_steps {
            reg.tool_results()
                .presentation_step(*key, Arc::clone(step))?;
        }
        for (key, hook) in &self.spec.runtime_event_hooks {
            reg.session().on_event(*key, Arc::clone(hook))?;
        }
        for operation in &self.spec.plugin_operations {
            reg.operations().register(operation.clone())?;
        }
        for (priority, compactor) in &self.spec.context_compactors {
            reg.context().compact(*priority, Arc::clone(compactor))?;
        }
        for (priority, hook) in &self.spec.context_pressure_hooks {
            reg.context().pressure(*priority, Arc::clone(hook))?;
        }
        for (name, reducer) in &self.spec.state_reducers {
            reg.state_reducer(name.clone(), Arc::clone(reducer))?;
        }
        Ok(())
    }
}
