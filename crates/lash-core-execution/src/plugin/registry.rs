//! Typed plugin recipes, ergonomic specs, and owner lifecycle traits.
//! Specs feed the same declared registrations as authored recipes.

use super::ServiceBindings;
use std::sync::Arc;

use super::{
    AfterTurnHook, AssistantResponseHook, AssistantStreamFinishedHook, AssistantStreamHook,
    BeforeTurnHook, CheckpointHook, ContextCompactor, ContextPressureHook,
    ErasedPluginOperationInvokeFuture, HookKey, PluginCommand, PluginCommandHandler, PluginError,
    PluginHost, PluginLifecycleEventHook, PluginOperationOutcome, PluginOperationRegistration,
    PluginOperationSpec, PluginQuery, PluginQueryHandler, PluginQueryInvokeFuture, PluginTask,
    PluginTaskHandler, ToolArgsCheckHook, ToolArgsTransformHook, ToolCatalogContributor,
    ToolPresentationStep, ToolResultCheckHook, ToolResultTransformHook,
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

    pub(crate) fn merged(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        for (key, values) in &other.contributions {
            merged
                .contributions
                .entry(key.clone())
                .or_default()
                .extend(values.clone());
        }
        merged
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
    pub(crate) registrations: super::PluginRegistrar<()>,
    pub(crate) errors: Vec<PluginError>,
}
impl PluginSpec {
    fn admit(&mut self, result: Result<(), PluginError>) {
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_extension_contribution(
        mut self,
        contribution: PluginExtensionContribution,
    ) -> Self {
        let result = self
            .registrations
            .extension(contribution.extension_id.clone(), move |_, _| {
                Ok(contribution.payload.clone())
            });
        self.admit(result);
        self
    }

    pub fn with_tool_provider(
        mut self,
        key: super::ToolProviderKey,
        provider: Arc<dyn ToolProvider>,
    ) -> Self {
        let result = self
            .registrations
            .tools()
            .record_provider(key, move |_, _| Ok(Arc::clone(&provider)));
        self.admit(result);
        self
    }

    /// Declare the provider `key` with the reader its `Read` tools need
    /// (ADR 0144).
    pub fn with_tool_provider_registration(
        mut self,
        key: super::ToolProviderKey,
        registration: crate::ToolProviderRegistration,
    ) -> Self {
        let result = self
            .registrations
            .tools()
            .record_provider_with_reads(key, move |_, _| Ok(registration.clone()));
        self.admit(result);
        self
    }

    /// Declare the one deferred provider under `key`; duplicate addresses are refused.
    pub fn with_deferred_provider(
        mut self,
        key: super::ToolProviderKey,
        provider: Arc<dyn crate::DeferredToolProvider>,
    ) -> Self {
        let result = self
            .registrations
            .tools()
            .record_deferred(key, move |_, _| Ok(Arc::clone(&provider)));
        self.admit(result);
        self
    }

    /// The spec's keyed entries register as [`PluginRegistrar`] calls; a
    /// duplicate key in one seam fails registration.
    pub fn with_tool_catalog_contributor(
        mut self,
        key: HookKey,
        contributor: ToolCatalogContributor,
    ) -> Self {
        let result = self
            .registrations
            .tool_catalog()
            .contribute(key, move |_, _| Ok(Arc::clone(&contributor)));
        self.admit(result);
        self
    }

    pub fn with_before_turn(mut self, key: HookKey, hook: BeforeTurnHook) -> Self {
        let result = self
            .registrations
            .turn()
            .before(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_tool_args_transform(mut self, key: HookKey, hook: ToolArgsTransformHook) -> Self {
        let result = self
            .registrations
            .tool_calls()
            .transform_args(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_tool_args_check(mut self, key: HookKey, hook: ToolArgsCheckHook) -> Self {
        let result = self
            .registrations
            .tool_calls()
            .check_args(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_tool_result_transform(
        mut self,
        key: HookKey,
        hook: ToolResultTransformHook,
    ) -> Self {
        let result = self
            .registrations
            .tool_calls()
            .transform_result(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_tool_result_check(mut self, key: HookKey, hook: ToolResultCheckHook) -> Self {
        let result = self
            .registrations
            .tool_calls()
            .check_result(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_after_turn(mut self, key: HookKey, hook: AfterTurnHook) -> Self {
        let result = self
            .registrations
            .turn()
            .after(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_checkpoint(mut self, key: HookKey, hook: CheckpointHook) -> Self {
        let result = self
            .registrations
            .turn()
            .checkpoint(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_assistant_stream(mut self, key: HookKey, hook: AssistantStreamHook) -> Self {
        let result = self
            .registrations
            .output()
            .stream(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_assistant_response(
        mut self,
        key: HookKey,
        stream_state_from: Option<HookKey>,
        hook: AssistantResponseHook,
    ) -> Self {
        let result = self
            .registrations
            .output()
            .response(key, stream_state_from, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub fn with_assistant_stream_finished(
        mut self,
        key: HookKey,
        hook: AssistantStreamFinishedHook,
    ) -> Self {
        let result = self
            .registrations
            .output()
            .stream_finished(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    /// Appends one composable presentation step (FIG-3420). Steps run in the
    /// order `with_presentation_step` calls list them.
    pub fn with_presentation_step(mut self, key: HookKey, step: ToolPresentationStep) -> Self {
        let result = self
            .registrations
            .tool_results()
            .presentation_step(key, move |_, _| Ok(Arc::clone(&step)));
        self.admit(result);
        self
    }

    pub fn with_runtime_event(mut self, key: HookKey, hook: PluginLifecycleEventHook) -> Self {
        let result = self
            .registrations
            .lifecycle()
            .on_event(key, move |_, _| Ok(Arc::clone(&hook)));
        self.admit(result);
        self
    }

    pub(crate) fn with_plugin_query(
        mut self,
        spec: PluginOperationSpec,
        handler: PluginQueryHandler,
    ) -> Self {
        let result = self
            .registrations
            .operations()
            .register(PluginOperationRegistration::query(spec, handler));
        self.admit(result);
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
        let result = self
            .registrations
            .operations()
            .register(PluginOperationRegistration::command(spec, handler));
        self.admit(result);
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
        let result = self
            .registrations
            .operations()
            .register(PluginOperationRegistration::task(spec, handler));
        self.admit(result);
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
        let result = HookKey::new(compactor.id()).and_then(|key| {
            self.registrations
                .context()
                .compact(key, priority, move |_, _| Ok(Arc::clone(&compactor)))
        });
        self.admit(result);
        self
    }

    pub fn with_context_pressure_hook(
        mut self,
        priority: i32,
        hook: Arc<dyn ContextPressureHook>,
    ) -> Self {
        let result = HookKey::new(hook.id()).and_then(|key| {
            self.registrations
                .context()
                .pressure(key, priority, move |_, _| Ok(Arc::clone(&hook)))
        });
        self.admit(result);
        self
    }

    pub fn with_state_reducer(
        mut self,
        name: impl Into<String>,
        reducer: super::StateReducer,
    ) -> Self {
        let result = self
            .registrations
            .state_reducer(name, move |_, _| Ok(Arc::clone(&reducer)));
        self.admit(result);
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
                content: lash_trace::TelemetryContent::Captured,
                context,
                event,
            }
        });
        self.emit(record);
    }
}

#[derive(Clone, Debug)]
pub struct PluginSessionContext {
    pub bindings: Arc<ServiceBindings>,
    pub tracing: crate::trace::TraceRuntime,
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// Who the plugin session is built for: a session, or a process runtime
    /// built from its captured execution environment.
    pub owner: crate::RuntimeOwner,
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
    /// The deployment's current policy for content in built-in telemetry.
    ///
    /// Custom trace payloads are plugin-authored and pass through Lash
    /// without inspection. A plugin must honour this policy for content it
    /// includes in those payloads. Reopening under another host uses that
    /// host's policy; this operational setting is not session config.
    pub fn telemetry_content(&self) -> lash_trace::TelemetryContent {
        self.tracing.content()
    }

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

pub use lash_core_ids::{FormatVersion, PluginId};

/// What a plugin declares about itself before any session is built
/// (FIG-4732): its id, namespace formats, and readable stream-state formats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDeclaration {
    /// The id the plugin registers under.
    pub id: PluginId,
    /// The format of stored state and config the plugin reads natively.
    pub format_version: FormatVersion,
    /// Every format the plugin can write. It contains `format_version`.
    pub writable_formats: Vec<FormatVersion>,
    /// The format written by this plugin's stream-finished callbacks.
    pub stream_state_format: FormatVersion,
    /// The stream-state formats its response callbacks can read, including native.
    pub readable_stream_state_formats: Vec<FormatVersion>,
    /// What a fork of a session does with the plugin's namespace: copy it
    /// as of the fork point, or reset it to the plugin's initial state.
    /// Recorded with the namespace, so a later fork follows it.
    pub state_fork: crate::plugin::StateFork,
}

impl PluginDeclaration {
    /// The declaration of a plugin that reads and writes only its first formats.
    pub fn initial(id: &'static str) -> Self {
        Self {
            id: PluginId::new(id),
            format_version: FormatVersion::ONE,
            writable_formats: vec![FormatVersion::ONE],
            stream_state_format: FormatVersion::ONE,
            readable_stream_state_formats: vec![FormatVersion::ONE],
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
    /// Two factories cannot own the same plugin namespace.
    #[error("plugin id `{id}` is registered twice")]
    DuplicatePluginId { id: String },
    /// The declaration names another plugin than the factory that gave it.
    #[error("plugin factory `{factory}` declares itself as `{declared}`")]
    IdMismatch { factory: String, declared: String },
    /// The plugin cannot read the stream state its callbacks write.
    #[error("plugin `{plugin}` does not read its native stream-state format {format_version}")]
    NativeStreamStateNotReadable {
        plugin: String,
        #[schemars(with = "std::num::NonZeroU32")]
        format_version: FormatVersion,
    },
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
/// registered in, which is the order fresh callback plans select hooks.
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
        let mut ids = std::collections::BTreeSet::new();
        for declaration in &declarations {
            if !ids.insert(declaration.id.as_str()) {
                return Err(PluginDeclarationError::DuplicatePluginId {
                    id: declaration.id.as_str().to_owned(),
                });
            }
            if !declaration
                .readable_stream_state_formats
                .contains(&declaration.stream_state_format)
            {
                return Err(PluginDeclarationError::NativeStreamStateNotReadable {
                    plugin: declaration.id.as_str().to_owned(),
                    format_version: declaration.stream_state_format,
                });
            }
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
    /// plugin in hook order with the highest
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
        crate::store::plugin_writers::PluginAdmission::choose(registrations.iter(), ranges)
    }
}

pub trait SessionPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn session_ready(&self, _ctx: SessionReadyContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Pure identity and format metadata shared by deployment and offline readers.
pub trait PluginMetadata {
    fn plugin_declaration(&self) -> PluginDeclaration;
}

/// Declare a recipe once; bind session owners cheaply.
///
/// [`recipe`](Self::recipe), taking `self: Arc<Self>`, declares contributions
/// and their binders in a [`PluginRecipe`](super::PluginRecipe) before a session
/// owner exists. The recipe also carries configuration, host extensions and
/// process engines. Use [`PluginRecipe::bound`](super::PluginRecipe::bound)
/// for plain session data, [`PluginRecipe::new`](super::PluginRecipe::new) for
/// a [`SessionPlugin`] with a readiness callback, or
/// [`PluginRecipe::from_registrar`](super::PluginRecipe::from_registrar) when
/// entries need no session-local data.
///
/// Owner and contribution binders **must be cheap**: the recipe binds them
/// whenever an owner is materialized, including creation, children, forks and
/// reopen. Binders must not:
/// - perform any I/O (disk reads, HTTP calls, DB queries),
/// - compile regexes, templates, or schemas,
/// - open network connections or initialize connection pools,
/// - load models, parse large config files, or allocate large buffers,
/// - block the current thread for non-trivial work.
///
/// Expensive state belongs on the `PluginFactory` struct itself,
/// wrapped in `Arc` so recipe binders can cheaply clone it into session owners
/// and contributions. The embedder constructs the factory once; its shared
/// pools, caches and compiled artifacts outlive individual owners and are
/// reused on reopen. Factories that own host-visible resources also
/// participate in the explicit post-intake lifecycle through
/// [`shutdown`](Self::shutdown).
#[async_trait::async_trait]
pub trait PluginFactory: PluginMetadata + Send + Sync {
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

    /// The [`Backend::binding_identity`](crate::Backend::binding_identity) of
    /// the backend whose stores this factory keeps state in, when it keeps
    /// any: the RLM protocol keeps its Lash VM module artifacts there.
    ///
    /// A runtime built over one backend refuses a factory bound to another,
    /// so no plugin writes state into a substrate the runtime does not reopen
    /// or sweep (ADR 0102, D2). The default binds to none. A factory that
    /// wraps another forwards the inner factory's answer.
    fn bound_backend(&self) -> Option<&str> {
        None
    }

    /// Declare contributions and their binders without constructing a session owner.
    /// Configuration, host extensions and process engines belong to this same recipe.
    fn recipe(self: Arc<Self>) -> std::result::Result<super::PluginRecipe, PluginError>;
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
    extensions: PluginExtensions,
    plugin_host: &'a super::PluginHost,
    backend: &'a crate::Backend,
    trace_runtime: &'a crate::trace::TraceRuntime,
    process_lifecycle_available: bool,
}

impl<'a> ProcessEngineContributionContext<'a> {
    pub fn new(
        plugin_host: &'a super::PluginHost,
        backend: &'a crate::Backend,
        trace_runtime: &'a crate::trace::TraceRuntime,
        process_lifecycle_available: bool,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            extensions: plugin_host.extensions()?,
            plugin_host,
            backend,
            trace_runtime,
            process_lifecycle_available,
        })
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    /// The host whose factories supply creation-time process resource grants.
    pub fn plugin_host(&self) -> &super::PluginHost {
        self.plugin_host
    }

    /// The backend the runtime is built over: an engine keeps what its
    /// processes run in this backend's stores, so a process resumed after a
    /// restart finds it and the backend's cleanup sweeps it.
    pub fn backend(&self) -> &crate::Backend {
        self.backend
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

#[derive(Clone)]
pub struct StaticPluginFactory {
    declaration: PluginDeclaration,
    spec: PluginSpec,
}
impl StaticPluginFactory {
    pub fn new(declaration: PluginDeclaration, mut spec: PluginSpec) -> Self {
        for error in &mut spec.errors {
            if let PluginError::Registration(message) = error {
                *message = format!("{message} for plugin `{}`", declaration.id);
            }
        }
        Self { declaration, spec }
    }
    /// The scoped address of a provider in this factory's declared entries.
    pub fn provider_handle(
        &self,
        key: super::ToolProviderKey,
    ) -> Result<super::ToolProviderHandle, PluginError> {
        if let Some(error) = self.spec.errors.first() {
            return Err(error.clone());
        }
        self.spec.registrations.provider_handle(self.id(), key)
    }
    pub(crate) fn spec(&self) -> &PluginSpec {
        &self.spec
    }
}
impl PluginFactory for StaticPluginFactory {
    fn id(&self) -> &'static str {
        self.declaration.id.as_str()
    }
    fn recipe(self: Arc<Self>) -> std::result::Result<super::PluginRecipe, PluginError> {
        if let Some(error) = self.spec.errors.first() {
            return Err(error.clone());
        }
        Ok(super::PluginRecipe::from_registrar(
            self.id(),
            self.spec.registrations.clone(),
        ))
    }
}
impl PluginMetadata for StaticPluginFactory {
    fn plugin_declaration(&self) -> PluginDeclaration {
        self.declaration.clone()
    }
}
