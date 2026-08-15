use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::*;

#[derive(Clone)]
pub(crate) struct RegisteredHook<T> {
    pub(crate) plugin_id: String,
    pub(crate) hook: T,
}

#[derive(Clone)]
pub(crate) struct RegisteredExclusiveHook<T> {
    pub(crate) plugin_id: String,
    pub(crate) hook: T,
}

pub(crate) fn current_registration_owner(registering_plugin_id: &Option<String>) -> String {
    registering_plugin_id
        .clone()
        .unwrap_or_else(|| "__unknown__".to_string())
}

fn push_registered_hook<T>(
    hooks: &mut Vec<RegisteredHook<T>>,
    registering_plugin_id: &Option<String>,
    hook: T,
) {
    hooks.push(RegisteredHook {
        plugin_id: current_registration_owner(registering_plugin_id),
        hook,
    });
}

fn push_prioritized_registered_hook<T>(
    hooks: &mut Vec<(i32, RegisteredHook<T>)>,
    registering_plugin_id: &Option<String>,
    priority: i32,
    hook: T,
) {
    hooks.push((
        priority,
        RegisteredHook {
            plugin_id: current_registration_owner(registering_plugin_id),
            hook,
        },
    ));
}

fn exclusive_hook_owner(
    existing_owner: Option<&str>,
    registering_plugin_id: &Option<String>,
    hook_kind: &str,
    hook_name: &str,
) -> Result<String, PluginError> {
    let plugin_id = registering_plugin_id
        .clone()
        .ok_or_else(|| PluginError::Registration("missing registering plugin id".to_string()))?;
    if let Some(existing) = existing_owner {
        return Err(PluginError::Registration(format!(
            "duplicate {hook_kind} for `{hook_name}`: `{plugin_id}` conflicts with `{existing}`"
        )));
    }
    Ok(plugin_id)
}

fn register_singleton_hook<H>(
    slot: &mut Option<RegisteredExclusiveHook<H>>,
    registering_plugin_id: &Option<String>,
    hook_kind: &str,
    hook_name: &str,
    hook: H,
) -> Result<(), PluginError> {
    let plugin_id = exclusive_hook_owner(
        slot.as_ref()
            .map(|registered| registered.plugin_id.as_str()),
        registering_plugin_id,
        hook_kind,
        hook_name,
    )?;
    *slot = Some(RegisteredExclusiveHook { plugin_id, hook });
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct PluginContributions {
    pub(crate) tool_providers: Vec<RegisteredHook<Arc<dyn ToolProvider>>>,
    pub(crate) orchestrating_tools: Vec<crate::tool_provider::orchestration::OrchestratingToolDef>,
    pub(crate) triggers: Vec<crate::TriggerEvent>,
    pub(crate) prompt_contributors: Vec<RegisteredHook<PromptContributor>>,
    pub(crate) tool_catalog_contributors: Vec<RegisteredHook<ToolCatalogContributor>>,
    pub(crate) before_turn_hooks: Vec<RegisteredHook<BeforeTurnHook>>,
    pub(crate) before_tool_call_hooks: Vec<RegisteredHook<BeforeToolCallHook>>,
    pub(crate) after_tool_call_hooks: Vec<RegisteredHook<AfterToolCallHook>>,
    pub(crate) after_turn_hooks: Vec<RegisteredHook<AfterTurnHook>>,
    pub(crate) checkpoint_hooks: Vec<RegisteredHook<CheckpointHook>>,
    pub(crate) assistant_stream_hooks: Vec<RegisteredHook<AssistantStreamHook>>,
    pub(crate) assistant_response_hooks: Vec<RegisteredHook<AssistantResponseHook>>,
    pub(crate) assistant_stream_finished_hooks: Vec<RegisteredHook<AssistantStreamFinishedHook>>,
    pub(crate) tool_result_projector: Option<RegisteredExclusiveHook<ToolResultProjector>>,
    pub(crate) runtime_event_hooks: Vec<RegisteredHook<PluginLifecycleEventHook>>,
    pub(crate) session_config_mutators: Vec<SessionConfigMutator>,
    pub(crate) plugin_queries: BTreeMap<String, RegisteredPluginQuery>,
    pub(crate) plugin_commands: BTreeMap<String, RegisteredPluginCommand>,
    pub(crate) plugin_tasks: BTreeMap<String, RegisteredPluginTask>,
    pub(crate) turn_context_transforms: Vec<(i32, RegisteredHook<Arc<dyn TurnContextTransform>>)>,
    pub(crate) context_compactors: Vec<(i32, RegisteredHook<Arc<dyn ContextCompactor>>)>,
    pub(crate) protocol_session: Option<RegisteredExclusiveHook<Arc<dyn ProtocolSessionPlugin>>>,
    pub(crate) protocol_driver: Option<RegisteredExclusiveHook<Arc<dyn ProtocolDriverPlugin>>>,
    pub(crate) code_executor: Option<RegisteredExclusiveHook<Arc<dyn CodeExecutorPlugin>>>,
    pub(crate) assistant_prose_projector:
        Option<RegisteredExclusiveHook<Arc<dyn AssistantProseProjectorPlugin>>>,
}

pub struct PluginRegistrar {
    pub(crate) contributions: PluginContributions,
    pub(crate) registering_plugin_id: Option<String>,
    tool_names: BTreeSet<String>,
}

pub struct ToolRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ToolRegistrations<'_> {
    pub fn provider(self, provider: Arc<dyn ToolProvider>) -> Result<(), PluginError> {
        self.reg.add_tool_provider(provider)
    }

    #[doc(hidden)]
    pub fn orchestrating(
        self,
        definition: crate::tool_provider::orchestration::OrchestratingToolDef,
    ) -> Result<(), PluginError> {
        self.reg.add_orchestrating_tool(definition)
    }
}

pub struct TriggerEventRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl TriggerEventRegistrations<'_> {
    pub fn declare(self, event: crate::TriggerEvent) -> Result<(), PluginError> {
        self.reg.add_trigger(event)
    }
}

pub struct PromptRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl PromptRegistrations<'_> {
    pub fn contribute(self, contributor: PromptContributor) {
        self.reg.add_prompt_contributor(contributor);
    }
}

pub struct ToolCatalogRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ToolCatalogRegistrations<'_> {
    pub fn contribute(self, contributor: ToolCatalogContributor) {
        self.reg.add_tool_catalog_contributor(contributor);
    }
}

pub struct TurnRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl TurnRegistrations<'_> {
    pub fn before(self, hook: BeforeTurnHook) {
        self.reg.add_before_turn_hook(hook);
    }

    pub fn after(self, hook: AfterTurnHook) {
        self.reg.add_after_turn_hook(hook);
    }

    pub fn checkpoint(self, hook: CheckpointHook) {
        self.reg.add_checkpoint_hook(hook);
    }
}

pub struct ToolCallRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ToolCallRegistrations<'_> {
    pub fn before(self, hook: BeforeToolCallHook) {
        self.reg.add_before_tool_call_hook(hook);
    }

    pub fn after(self, hook: AfterToolCallHook) {
        self.reg.add_after_tool_call_hook(hook);
    }
}

pub struct OutputRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl OutputRegistrations<'_> {
    pub fn stream(self, hook: AssistantStreamHook) {
        self.reg.add_assistant_stream_hook(hook);
    }

    pub fn response(self, hook: AssistantResponseHook) {
        self.reg.add_assistant_response_hook(hook);
    }

    pub fn stream_finished(self, hook: AssistantStreamFinishedHook) {
        self.reg.add_assistant_stream_finished_hook(hook);
    }

    pub fn assistant_prose_projector(
        self,
        provider: Arc<dyn AssistantProseProjectorPlugin>,
    ) -> Result<(), PluginError> {
        self.reg.add_assistant_prose_projector(provider)
    }
}

pub struct ToolResultRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ToolResultRegistrations<'_> {
    pub fn projector(self, hook: ToolResultProjector) -> Result<(), PluginError> {
        self.reg.add_tool_result_projector(hook)
    }
}

pub struct SessionRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl SessionRegistrations<'_> {
    pub fn on_event(self, hook: PluginLifecycleEventHook) {
        push_registered_hook(
            &mut self.reg.contributions.runtime_event_hooks,
            &self.reg.registering_plugin_id,
            hook,
        );
    }

    pub fn config_mutator(self, hook: SessionConfigMutator) {
        self.reg.contributions.session_config_mutators.push(hook);
    }
}

pub struct PluginOperationRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl PluginOperationRegistrations<'_> {
    pub(crate) fn query(
        self,
        def: PluginOperationDef,
        handler: PluginQueryHandler,
    ) -> Result<(), PluginError> {
        self.reg.add_plugin_query(def, handler)
    }

    pub(crate) fn command(
        self,
        def: PluginOperationDef,
        handler: PluginCommandHandler,
    ) -> Result<(), PluginError> {
        self.reg.add_plugin_command(def, handler)
    }

    pub(crate) fn task(
        self,
        def: PluginOperationDef,
        handler: PluginTaskHandler,
    ) -> Result<(), PluginError> {
        self.reg.add_plugin_task(def, handler)
    }

    pub fn typed_query<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginQuery,
        F: Fn(PluginQueryContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, PluginOperationFailure>> + Send + 'static,
    {
        self.query(
            plugin_operation_def::<Op>(PluginOperationKind::Query),
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

    pub fn typed_command<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<PluginCommandOutcome<Op::Output>, PluginOperationFailure>>
            + Send
            + 'static,
    {
        self.command(
            plugin_operation_def::<Op>(PluginOperationKind::Command),
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
                            Ok(ErasedPluginCommandOutcome {
                                output,
                                events: outcome.events,
                                directives: outcome.directives,
                            })
                        }) as PluginCommandInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(PluginOperationFailure::new(format!(
                            "invalid {} args: {err}",
                            Op::NAME
                        )))
                    }) as PluginCommandInvokeFuture,
                }
            }),
        )
    }

    pub fn typed_command_value<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, PluginOperationFailure>> + Send + 'static,
    {
        self.typed_command::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginCommandOutcome::new) }
        })
    }

    pub fn typed_task<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<PluginTaskOutcome<Op::Output>, PluginOperationFailure>>
            + Send
            + 'static,
    {
        self.task(
            plugin_operation_def::<Op>(PluginOperationKind::Task),
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
                            Ok(ErasedPluginTaskOutcome {
                                output,
                                events: outcome.events,
                                directives: outcome.directives,
                            })
                        }) as PluginTaskInvokeFuture
                    }
                    Err(err) => Box::pin(async move {
                        Err(PluginOperationFailure::new(format!(
                            "invalid {} args: {err}",
                            Op::NAME
                        )))
                    }) as PluginTaskInvokeFuture,
                }
            }),
        )
    }

    pub fn typed_task_value<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, PluginOperationFailure>> + Send + 'static,
    {
        self.typed_task::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginTaskOutcome::new) }
        })
    }
}

pub struct ContextRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ContextRegistrations<'_> {
    /// Register a per-turn context transform. Higher priority runs first.
    pub fn prepare_turn(self, priority: i32, transform: Arc<dyn TurnContextTransform>) {
        push_prioritized_registered_hook(
            &mut self.reg.contributions.turn_context_transforms,
            &self.reg.registering_plugin_id,
            priority,
            transform,
        );
    }

    /// Register an explicit compaction provider. Higher priority runs first.
    pub fn compact(self, priority: i32, compactor: Arc<dyn ContextCompactor>) {
        push_prioritized_registered_hook(
            &mut self.reg.contributions.context_compactors,
            &self.reg.registering_plugin_id,
            priority,
            compactor,
        );
    }
}

pub struct ProtocolRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ProtocolRegistrations<'_> {
    pub fn session(self, provider: Arc<dyn ProtocolSessionPlugin>) -> Result<(), PluginError> {
        self.reg.add_protocol_session(provider)
    }

    /// Claim the session-wide singleton protocol-driver slot. The
    /// plugin provides a `ProtocolDriverHandle` via `build_preamble`.
    /// The active plugin stack must install exactly one protocol driver.
    pub fn protocol_driver(
        self,
        provider: Arc<dyn ProtocolDriverPlugin>,
    ) -> Result<(), PluginError> {
        self.reg.add_protocol_driver(provider)
    }
}

pub struct ExecutionRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ExecutionRegistrations<'_> {
    pub fn code_executor(self, provider: Arc<dyn CodeExecutorPlugin>) -> Result<(), PluginError> {
        self.reg.add_code_executor(provider)
    }
}

impl PluginRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            contributions: PluginContributions::default(),
            registering_plugin_id: None,
            tool_names: BTreeSet::new(),
        }
    }

    pub fn tools(&mut self) -> ToolRegistrations<'_> {
        ToolRegistrations { reg: self }
    }

    pub fn triggers(&mut self) -> TriggerEventRegistrations<'_> {
        TriggerEventRegistrations { reg: self }
    }

    pub fn prompt(&mut self) -> PromptRegistrations<'_> {
        PromptRegistrations { reg: self }
    }

    pub fn tool_catalog(&mut self) -> ToolCatalogRegistrations<'_> {
        ToolCatalogRegistrations { reg: self }
    }

    pub fn turn(&mut self) -> TurnRegistrations<'_> {
        TurnRegistrations { reg: self }
    }

    pub fn tool_calls(&mut self) -> ToolCallRegistrations<'_> {
        ToolCallRegistrations { reg: self }
    }

    pub fn output(&mut self) -> OutputRegistrations<'_> {
        OutputRegistrations { reg: self }
    }

    pub fn tool_results(&mut self) -> ToolResultRegistrations<'_> {
        ToolResultRegistrations { reg: self }
    }

    pub fn session(&mut self) -> SessionRegistrations<'_> {
        SessionRegistrations { reg: self }
    }

    pub fn operations(&mut self) -> PluginOperationRegistrations<'_> {
        PluginOperationRegistrations { reg: self }
    }

    pub fn context(&mut self) -> ContextRegistrations<'_> {
        ContextRegistrations { reg: self }
    }

    pub fn protocol(&mut self) -> ProtocolRegistrations<'_> {
        ProtocolRegistrations { reg: self }
    }

    pub fn execution(&mut self) -> ExecutionRegistrations<'_> {
        ExecutionRegistrations { reg: self }
    }

    fn add_tool_provider(&mut self, provider: Arc<dyn ToolProvider>) -> Result<(), PluginError> {
        for manifest in provider.tool_manifests() {
            if !self.tool_names.insert(manifest.name.clone()) {
                return Err(PluginError::Registration(format!(
                    "duplicate plugin tool name `{}`",
                    manifest.name
                )));
            }
        }
        push_registered_hook(
            &mut self.contributions.tool_providers,
            &self.registering_plugin_id,
            provider,
        );
        Ok(())
    }

    fn add_orchestrating_tool(
        &mut self,
        definition: crate::tool_provider::orchestration::OrchestratingToolDef,
    ) -> Result<(), PluginError> {
        self.contributions.orchestrating_tools.push(definition);
        Ok(())
    }

    fn add_trigger(&mut self, event: crate::TriggerEvent) -> Result<(), PluginError> {
        if self
            .contributions
            .triggers
            .iter()
            .any(|existing| existing.key() == event.key())
        {
            return Err(PluginError::Registration(format!(
                "duplicate trigger occurrence `{}.{}.{}`",
                event.resource_type, event.alias, event.event
            )));
        }
        self.contributions.triggers.push(event);
        Ok(())
    }

    fn add_prompt_contributor(&mut self, contributor: PromptContributor) {
        push_registered_hook(
            &mut self.contributions.prompt_contributors,
            &self.registering_plugin_id,
            contributor,
        );
    }

    fn add_tool_catalog_contributor(&mut self, contributor: ToolCatalogContributor) {
        push_registered_hook(
            &mut self.contributions.tool_catalog_contributors,
            &self.registering_plugin_id,
            contributor,
        );
    }

    fn add_before_turn_hook(&mut self, hook: BeforeTurnHook) {
        push_registered_hook(
            &mut self.contributions.before_turn_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_before_tool_call_hook(&mut self, hook: BeforeToolCallHook) {
        push_registered_hook(
            &mut self.contributions.before_tool_call_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_after_tool_call_hook(&mut self, hook: AfterToolCallHook) {
        push_registered_hook(
            &mut self.contributions.after_tool_call_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_after_turn_hook(&mut self, hook: AfterTurnHook) {
        push_registered_hook(
            &mut self.contributions.after_turn_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_checkpoint_hook(&mut self, hook: CheckpointHook) {
        push_registered_hook(
            &mut self.contributions.checkpoint_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_assistant_stream_hook(&mut self, hook: AssistantStreamHook) {
        push_registered_hook(
            &mut self.contributions.assistant_stream_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_assistant_response_hook(&mut self, hook: AssistantResponseHook) {
        push_registered_hook(
            &mut self.contributions.assistant_response_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_assistant_stream_finished_hook(&mut self, hook: AssistantStreamFinishedHook) {
        push_registered_hook(
            &mut self.contributions.assistant_stream_finished_hooks,
            &self.registering_plugin_id,
            hook,
        );
    }

    fn add_assistant_prose_projector(
        &mut self,
        provider: Arc<dyn AssistantProseProjectorPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.assistant_prose_projector,
            &self.registering_plugin_id,
            "assistant prose projector",
            "assistant_prose_projector",
            provider,
        )
    }

    fn add_tool_result_projector(&mut self, hook: ToolResultProjector) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.tool_result_projector,
            &self.registering_plugin_id,
            "tool result projector",
            "model_observation",
            hook,
        )
    }

    fn operation_owner(&self) -> Result<String, PluginError> {
        self.registering_plugin_id
            .clone()
            .ok_or_else(|| PluginError::Registration("missing registering plugin id".to_string()))
    }

    fn ensure_unique_operation_name(&self, name: &str) -> Result<(), PluginError> {
        if self.contributions.plugin_queries.contains_key(name)
            || self.contributions.plugin_commands.contains_key(name)
            || self.contributions.plugin_tasks.contains_key(name)
        {
            return Err(PluginError::Registration(format!(
                "duplicate plugin operation name `{name}`"
            )));
        }
        Ok(())
    }

    fn add_plugin_query(
        &mut self,
        def: PluginOperationDef,
        handler: PluginQueryHandler,
    ) -> Result<(), PluginError> {
        self.ensure_unique_operation_name(&def.name)?;
        let plugin_id = self.operation_owner()?;
        self.contributions.plugin_queries.insert(
            def.name.clone(),
            RegisteredPluginQuery {
                plugin_id,
                def,
                handler,
            },
        );
        Ok(())
    }

    fn add_plugin_command(
        &mut self,
        def: PluginOperationDef,
        handler: PluginCommandHandler,
    ) -> Result<(), PluginError> {
        self.ensure_unique_operation_name(&def.name)?;
        let plugin_id = self.operation_owner()?;
        self.contributions.plugin_commands.insert(
            def.name.clone(),
            RegisteredPluginCommand {
                plugin_id,
                def,
                handler,
            },
        );
        Ok(())
    }

    fn add_plugin_task(
        &mut self,
        def: PluginOperationDef,
        handler: PluginTaskHandler,
    ) -> Result<(), PluginError> {
        self.ensure_unique_operation_name(&def.name)?;
        let plugin_id = self.operation_owner()?;
        self.contributions.plugin_tasks.insert(
            def.name.clone(),
            RegisteredPluginTask {
                plugin_id,
                def,
                handler,
            },
        );
        Ok(())
    }

    fn add_protocol_session(
        &mut self,
        provider: Arc<dyn ProtocolSessionPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.protocol_session,
            &self.registering_plugin_id,
            "protocol session capability",
            "protocol_session",
            provider,
        )
    }

    fn add_code_executor(
        &mut self,
        provider: Arc<dyn CodeExecutorPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.code_executor,
            &self.registering_plugin_id,
            "code executor capability",
            "code_executor",
            provider,
        )
    }

    fn add_protocol_driver(
        &mut self,
        provider: Arc<dyn ProtocolDriverPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.protocol_driver,
            &self.registering_plugin_id,
            "protocol driver capability",
            "protocol_driver",
            provider,
        )
    }
}
