use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use sha2::{Digest, Sha256};

use super::*;

mod directives;
mod tools;

async fn collect_owned_async<C, O, H, F>(
    hooks: &[RegisteredHook<H>],
    ctx: C,
    hook_kind: &'static str,
    phase_probe: Option<&Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    invoke: F,
) -> Result<Vec<PluginOwned<O>>, PluginError>
where
    C: Clone,
    F: Fn(&H, C) -> PluginFuture<Vec<O>>,
{
    let mut out = Vec::new();
    for registered in hooks {
        let phase_name = plugin_hook_phase_name(hook_kind, &registered.plugin_id);
        if let Some(probe) = phase_probe {
            probe.begin_named(&phase_name);
        }
        let result = invoke(&registered.hook, ctx.clone()).await;
        if let Some(probe) = phase_probe {
            probe.end_named(&phase_name);
        }
        for value in result? {
            out.push(PluginOwned {
                plugin_id: registered.plugin_id.clone(),
                value,
            });
        }
    }
    Ok(out)
}

fn plugin_hook_phase_name(hook_kind: &str, plugin_id: &str) -> String {
    format!("plugin_hook.{hook_kind}.{plugin_id}")
}

fn lifecycle_event_hook_kind(event: &PluginLifecycleEvent<'_>) -> &'static str {
    match event {
        PluginLifecycleEvent::TurnFinalized(_) => "turn_finalized",
        PluginLifecycleEvent::TurnPersisted(_) => "turn_persisted",
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
        scoped_effect_controller: crate::ScopedEffectController<'static>,
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

    fn into_context(self, session_id: Option<String>) -> PluginOperationContext {
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

pub(crate) fn plugin_lifecycle_hook_issue(error: PluginError) -> crate::runtime::TurnIssue {
    let error = match error {
        PluginError::SessionExecutionLeaseLost { session_id } => {
            return crate::runtime::TurnIssue {
                kind: "runtime".to_string(),
                code: Some(
                    crate::RuntimeErrorCode::SessionExecutionLeaseLost
                        .as_str()
                        .to_string(),
                ),
                terminal_reason: None,
                message: format!(
                    "session execution lease for session `{session_id}` was lost before commit"
                ),
                raw: None,
                retryable: Some(false),
                provider_failure_kind: None,
            };
        }
        error => error,
    };
    crate::runtime::TurnIssue {
        kind: "plugin".to_string(),
        code: Some("lifecycle_hook_failed".to_string()),
        terminal_reason: None,
        message: error.to_string(),
        raw: None,
        retryable: None,
        provider_failure_kind: None,
    }
}

fn collect_owned_sync<C, O, H, F>(
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
            plugin_id: registered.plugin_id.clone(),
            value: invoke(&registered.hook, ctx.clone())?,
        });
    }
    Ok(out)
}

struct EmptySnapshotReader;

impl SnapshotReader for EmptySnapshotReader {
    fn read_blob(&self, _name: &str) -> Option<&[u8]> {
        None
    }
}

pub struct PluginSession {
    pub(super) host: PluginHost,
    pub(super) session_id: String,
    pub(super) plugins: Vec<Arc<dyn SessionPlugin>>,
    pub(super) tools: Arc<dyn ToolProvider>,
    pub(super) tool_registry: Arc<crate::ToolRegistry>,
    pub(super) tool_catalog_overlay: ToolCatalogContribution,
    pub(super) tool_access: SessionToolAccess,
    pub(super) subagent: Option<SubagentSessionContext>,
    pub(super) extensions: PluginExtensions,
    pub(super) triggers: crate::TriggerEventCatalog,
    pub(super) contributions: PluginContributions,
}
impl PluginSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn tool_access(&self) -> &SessionToolAccess {
        &self.tool_access
    }

    pub fn subagent_context(&self) -> Option<&SubagentSessionContext> {
        self.subagent.as_ref()
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    pub fn triggers(&self) -> &crate::TriggerEventCatalog {
        &self.triggers
    }

    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    pub fn tools(&self) -> Arc<dyn ToolProvider> {
        Arc::clone(&self.tools)
    }

    pub fn tool_registry(&self) -> Arc<crate::ToolRegistry> {
        Arc::clone(&self.tool_registry)
    }

    pub(crate) fn protocol_session(&self) -> &Arc<dyn ProtocolSessionPlugin> {
        &self
            .contributions
            .protocol_session
            .as_ref()
            .expect("plugin session must have a protocol session")
            .hook
    }

    pub(crate) fn code_executor(&self) -> Option<Arc<dyn CodeExecutorPlugin>> {
        self.contributions
            .code_executor
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
    }

    pub(crate) fn assistant_prose_projector(
        &self,
    ) -> Option<Arc<dyn AssistantProseProjectorPlugin>> {
        self.contributions
            .assistant_prose_projector
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
    }

    pub fn protocol_driver(&self) -> Arc<dyn ProtocolDriverPlugin> {
        self.contributions
            .protocol_driver
            .as_ref()
            .map(|entry| Arc::clone(&entry.hook))
            .expect("plugin session must have a protocol driver")
    }

    pub fn plugin_operations(&self) -> Vec<PluginOperationDef> {
        self.contributions
            .plugin_operations
            .values()
            .map(|op| op.def().clone())
            .collect()
    }

    /// Whether any plugin registered an assistant-response hook.
    ///
    /// With none registered the runtime skips phase 2 of the staged LLM-call
    /// effect boundary entirely, so a hook-free session journals exactly one
    /// entry per call as before.
    pub(crate) fn has_assistant_response_hooks(&self) -> bool {
        !self.contributions.assistant_response_hooks.is_empty()
    }

    pub fn has_assistant_stream_hooks(&self) -> bool {
        !self.contributions.assistant_stream_hooks.is_empty()
    }

    pub fn has_assistant_stream_finished_hooks(&self) -> bool {
        !self
            .contributions
            .assistant_stream_finished_hooks
            .is_empty()
    }

    /// Chain registered turn-context transforms, piping each one's output
    /// into the next in priority order.
    pub async fn prepare_turn_context(
        &self,
        ctx: &TurnTransformContext<'_>,
        input: crate::session_model::context::PreparedContext,
        phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<crate::session_model::context::PreparedContext, ContextError> {
        let mut current = input;
        for (_, registered) in &self.contributions.turn_context_transforms {
            let phase_name =
                plugin_hook_phase_name("context_transform", registered.plugin_id.as_str());
            if let Some(probe) = phase_probe.as_ref() {
                probe.begin_named(&phase_name);
            }
            let result = registered.hook.transform(ctx, current).await;
            if let Some(probe) = phase_probe.as_ref() {
                probe.end_named(&phase_name);
            }
            current = result?;
        }
        Ok(current)
    }

    /// Ask registered compactors for seed nodes for a new compaction frame.
    pub async fn compact_context(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        for (_, registered) in &self.contributions.context_compactors {
            if let Some(compaction) = registered.hook.compact(ctx).await?
                && !compaction.is_empty()
            {
                return Ok(Some(compaction));
            }
        }
        Ok(None)
    }

    pub async fn collect_prompt_contributions(
        &self,
        ctx: PromptHookContext,
    ) -> Result<Vec<PromptContribution>, PluginError> {
        let mut out = collect_owned_async(
            &self.contributions.prompt_contributors,
            ctx,
            "prompt_contributor",
            None,
            |hook, ctx| hook(ctx),
        )
        .await?
        .into_iter()
        .map(|owned| owned.value)
        .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        out.retain(|contribution| {
            seen.insert((
                format!("{:?}", contribution.slot),
                contribution.priority,
                contribution.content.trim().to_string(),
            ))
        });
        out.sort_by(|a, b| {
            format!("{:?}", a.slot)
                .cmp(&format!("{:?}", b.slot))
                .then(a.priority.cmp(&b.priority))
        });
        Ok(out)
    }

    pub async fn before_turn(
        &self,
        ctx: TurnHookContext,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        self.before_turn_with_phase_probe(ctx, None).await
    }

    async fn before_turn_with_phase_probe(
        &self,
        ctx: TurnHookContext,
        phase_probe: Option<&Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        collect_owned_async(
            &self.contributions.before_turn_hooks,
            ctx,
            "before_turn",
            phase_probe,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    pub async fn before_tool_call(
        &self,
        mut ctx: ToolCallHookContext,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        let mut out = Vec::new();
        for (index, registered) in self.contributions.before_tool_call_hooks.iter().enumerate() {
            let directives = (registered.hook)(ctx.clone()).await?;
            for directive in directives {
                let replaces_args = matches!(directive, PluginDirective::ReplaceToolArgs { .. });
                if let PluginDirective::ReplaceToolArgs { args } = &directive {
                    ctx.args = args.clone();
                }
                out.push(PluginOwned {
                    plugin_id: registered.plugin_id.clone(),
                    value: directive,
                });
                if replaces_args {
                    for earlier in &self.contributions.before_tool_call_hooks[..index] {
                        let repeated = (earlier.hook)(ctx.clone()).await?;
                        for directive in repeated {
                            if matches!(directive, PluginDirective::ReplaceToolArgs { .. }) {
                                return Err(PluginError::BeforeToolCallReplacementConflict {
                                    replacing_plugin_id: registered.plugin_id.clone(),
                                    repeated_plugin_id: earlier.plugin_id.clone(),
                                });
                            }
                            let is_terminal_restriction =
                                directive.terminal_strength().is_some_and(|strength| {
                                    strength >= PluginTerminalStrength::DeniedShortCircuit
                                });
                            if is_terminal_restriction {
                                out.push(PluginOwned {
                                    plugin_id: earlier.plugin_id.clone(),
                                    value: directive,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn after_tool_call(
        &self,
        mut ctx: ToolResultHookContext,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        let mut out = Vec::new();
        let mut effective_replacement: Option<ToolOutcome> = None;
        for (index, registered) in self.contributions.after_tool_call_hooks.iter().enumerate() {
            let directives = (registered.hook)(ctx.clone()).await?;
            for directive in directives {
                let replacement = match &directive {
                    PluginDirective::ShortCircuitTool { output } if output.is_success() => {
                        Some(ToolOutcome::from_output(output.clone()))
                    }
                    _ => None,
                };
                out.push(PluginOwned {
                    plugin_id: registered.plugin_id.clone(),
                    value: directive,
                });
                if let Some(replacement) = replacement {
                    ctx.result = replacement.clone();
                    for earlier in &self.contributions.after_tool_call_hooks[..index] {
                        let repeated = (earlier.hook)(ctx.clone()).await?;
                        for directive in repeated {
                            if matches!(
                                &directive,
                                PluginDirective::ShortCircuitTool { output } if output.is_success()
                            ) {
                                return Err(PluginError::AfterToolCallReplacementConflict {
                                    replacing_plugin_id: registered.plugin_id.clone(),
                                    repeated_plugin_id: earlier.plugin_id.clone(),
                                });
                            }
                            let is_terminal_restriction =
                                directive.terminal_strength().is_some_and(|strength| {
                                    strength >= PluginTerminalStrength::DeniedShortCircuit
                                });
                            if is_terminal_restriction {
                                out.push(PluginOwned {
                                    plugin_id: earlier.plugin_id.clone(),
                                    value: directive,
                                });
                            }
                        }
                    }
                    if let Some(effective_replacement) = &effective_replacement {
                        ctx.result = effective_replacement.clone();
                    } else {
                        effective_replacement = Some(replacement);
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn after_turn(
        &self,
        ctx: TurnResultHookContext,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        self.after_turn_with_phase_probe(ctx, None).await
    }

    async fn after_turn_with_phase_probe(
        &self,
        ctx: TurnResultHookContext,
        phase_probe: Option<&Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        collect_owned_async(
            &self.contributions.after_turn_hooks,
            ctx,
            "after_turn",
            phase_probe,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    pub async fn at_checkpoint(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<Vec<PluginOwned<PluginDirective>>, PluginError> {
        collect_owned_async(
            &self.contributions.checkpoint_hooks,
            ctx,
            "checkpoint",
            None,
            |hook, ctx| hook(ctx),
        )
        .await
    }

    pub async fn transform_assistant_stream(
        &self,
        session_id: &str,
        chunk: String,
    ) -> Result<Vec<PluginOwned<AssistantStreamTransform>>, PluginError> {
        let mut current = chunk;
        let mut transforms = Vec::new();
        for registered in &self.contributions.assistant_stream_hooks {
            let transform = (registered.hook)(AssistantStreamHookContext {
                session_id: session_id.to_string(),
                chunk: current.clone(),
            })
            .await?;
            current = transform.chunk.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.plugin_id.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    pub async fn transform_assistant_response(
        &self,
        session_id: &str,
        response: crate::llm::types::LlmResponse,
    ) -> Result<Vec<PluginOwned<AssistantResponseTransform>>, PluginError> {
        let mut current = response;
        let mut transforms = Vec::new();
        for registered in &self.contributions.assistant_response_hooks {
            let transform = (registered.hook)(AssistantResponseHookContext {
                session_id: session_id.to_string(),
                response: current.clone(),
            })
            .await?;
            current = transform.response.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.plugin_id.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    pub async fn finish_assistant_stream(
        &self,
        session_id: &str,
        reason: AssistantStreamFinishReason,
    ) -> Result<(), PluginError> {
        for registered in &self.contributions.assistant_stream_finished_hooks {
            (registered.hook)(AssistantStreamFinishedContext {
                session_id: session_id.to_string(),
                reason,
            })
            .await?;
        }
        Ok(())
    }

    pub async fn project_tool_result(
        &self,
        ctx: ToolResultProjectionContext,
    ) -> Result<crate::ModelToolReturn, PluginError> {
        let Some(projector) = &self.contributions.tool_result_projector else {
            return Ok(crate::ModelToolReturn::from_output(
                ctx.call_id.clone(),
                ctx.tool_name.clone(),
                &ctx.output,
            ));
        };
        (projector.hook)(ctx).await
    }

    pub async fn emit_runtime_event(
        &self,
        event: PluginLifecycleEvent<'_>,
    ) -> Result<(), PluginError> {
        self.emit_runtime_event_with_phase_probe(event, None).await
    }

    pub async fn emit_runtime_event_with_phase_probe(
        &self,
        event: PluginLifecycleEvent<'_>,
        phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<(), PluginError> {
        let hook_kind = lifecycle_event_hook_kind(&event);
        let mut pending = FuturesUnordered::new();
        for registered in &self.contributions.runtime_event_hooks {
            let hook = Arc::clone(&registered.hook);
            let plugin_id = registered.plugin_id.clone();
            let phase_name = plugin_hook_phase_name(hook_kind, registered.plugin_id.as_str());
            let event = event.clone();
            let phase_probe = phase_probe.clone();
            pending.push(async move {
                if let Some(probe) = phase_probe.as_ref() {
                    probe.begin_named(&phase_name);
                }
                let result = hook(event).await;
                if let Some(probe) = phase_probe.as_ref() {
                    probe.end_named(&phase_name);
                }
                (plugin_id, result)
            });
        }
        let mut failures = Vec::new();
        while let Some((plugin_id, result)) = pending.next().await {
            if let Err(error) = result {
                failures.push((plugin_id, error));
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        // Execution-lane loss is runtime authority evidence, not an ordinary
        // plugin hook failure. Preserve it through the fan-out aggregation so
        // the turn boundary can retain the loud typed failure.
        if let Some(error @ PluginError::SessionExecutionLeaseLost { .. }) = failures
            .iter()
            .map(|(_, error)| error)
            .find(|error| matches!(error, PluginError::SessionExecutionLeaseLost { .. }))
        {
            return Err(error.clone());
        }
        failures.sort_by(|(left_id, left_error), (right_id, right_error)| {
            left_id
                .cmp(right_id)
                .then_with(|| left_error.to_string().cmp(&right_error.to_string()))
        });
        let details = failures
            .into_iter()
            .map(|(plugin_id, error)| format!("plugin `{plugin_id}`: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        Err(PluginError::Session(format!(
            "plugin runtime event hooks failed: {details}"
        )))
    }

    pub fn has_runtime_event_hooks(&self) -> bool {
        !self.contributions.runtime_event_hooks.is_empty()
    }

    pub async fn mutate_session_config(
        &self,
        ctx: SessionConfigChangedContext,
        mut policy: SessionPolicy,
    ) -> SessionPolicy {
        for hook in &self.contributions.session_config_mutators {
            match hook(ctx.clone(), policy.clone()).await {
                Ok(next_policy) => policy = next_policy,
                Err(err) => tracing::warn!("plugin config mutator failed: {err}"),
            }
        }
        policy
    }

    pub fn snapshot(&self) -> Result<PluginSessionSnapshot, PluginError> {
        let mut plugins = BTreeMap::new();
        for plugin in &self.plugins {
            let mut writer = InMemorySnapshotWriter::default();
            let meta = plugin.snapshot(&mut writer)?;
            plugins.insert(
                plugin.id().to_string(),
                PluginSnapshotEntry {
                    meta,
                    artifacts: writer.finish(),
                },
            );
        }
        Ok(PluginSessionSnapshot { plugins })
    }

    pub fn snapshot_revision_fingerprint(&self) -> u64 {
        let mut hasher = Sha256::new();
        for plugin in &self.plugins {
            hasher.update(plugin.id().as_bytes());
            hasher.update([0]);
            hasher.update(plugin.version().as_bytes());
            hasher.update([0]);
            hasher.update(plugin.snapshot_revision().to_le_bytes());
            hasher.update([0xff]);
        }
        let digest = hasher.finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("digest prefix"))
    }

    pub fn restore(&self, snapshot: &PluginSessionSnapshot) -> Result<(), PluginError> {
        for plugin in &self.plugins {
            if let Some(entry) = snapshot.plugins.get(plugin.id()) {
                let reader = InMemorySnapshotReader { entry };
                plugin.restore(&entry.meta, &reader)?;
            } else {
                plugin.restore(
                    &PluginSnapshotMeta {
                        plugin_id: plugin.id().to_string(),
                        plugin_version: plugin.version().to_string(),
                        revision: plugin.snapshot_revision(),
                        state: None,
                    },
                    &EmptySnapshotReader,
                )?;
            }
        }
        Ok(())
    }

    pub fn fork_for_session(
        &self,
        session_id: impl Into<String>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let snapshot = self.snapshot()?;
        self.host.build_session_with_overlay(
            session_id,
            Some(&snapshot),
            self.tool_catalog_overlay.clone(),
            Some(self.tool_registry.export_state()),
        )
    }

    pub fn fork_for_child_session(
        &self,
        session_id: impl Into<String>,
        parent_session_id: Option<String>,
        authority: super::SessionAuthorityContext,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let snapshot = self.snapshot()?;
        self.host.build_session_with_parent_and_overlay(
            session_id,
            parent_session_id,
            Some(&snapshot),
            self.tool_catalog_overlay.clone(),
            Some(self.tool_registry.export_state()),
            authority,
        )
    }

    pub fn fork_for_session_with_tool_catalog(
        &self,
        session_id: impl Into<String>,
        tool_catalog_overlay: ToolCatalogContribution,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let snapshot = self.snapshot()?;
        self.host.build_session_with_overlay(
            session_id,
            Some(&snapshot),
            tool_catalog_overlay,
            Some(self.tool_registry.export_state()),
        )
    }

    fn effective_operation_session(
        &self,
        name: &str,
        session_param: SessionParam,
        session_id: Option<String>,
        default_to_current_session: bool,
    ) -> Result<Option<String>, PluginOperationInvokeError> {
        let effective_session = session_id.or_else(|| {
            if default_to_current_session && !self.session_id.is_empty() {
                Some(self.session_id.clone())
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
        session_id: Option<String>,
        default_to_current_session: bool,
        invocation: PluginOperationInvocation,
    ) -> Result<(String, ErasedPluginOperationOutcome), PluginOperationInvokeError> {
        let Some(operation) = self.contributions.plugin_operations.get(name).cloned() else {
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
            .map_err(|err| PluginOperationInvokeError::Failed(err.to_string()))?;
        Ok((operation.plugin_id().to_string(), outcome))
    }

    pub(crate) async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
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
    pub async fn run_plugin_command_value(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
    ) -> Result<(String, PluginOperationOutcome<serde_json::Value>), PluginOperationInvokeError>
    {
        let (plugin_id, outcome) = self
            .run_plugin_command(
                name,
                args,
                session_id,
                default_to_current_session,
                sessions,
                session_lifecycle,
                session_graph,
                processes,
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
        reason = "plugin command invocation carries the runtime mutation services exposed to commands"
    )]
    pub(crate) async fn run_plugin_command(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
    ) -> Result<(String, ErasedPluginOperationOutcome), PluginOperationInvokeError> {
        self.invoke_plugin_operation(
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
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "plugin task invocation carries mutation services plus the scoped effect boundary"
    )]
    pub async fn run_plugin_task_value(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        scoped_effect_controller: crate::ScopedEffectController<'static>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<(String, PluginOperationOutcome<serde_json::Value>), PluginOperationInvokeError>
    {
        let (plugin_id, outcome) = self
            .run_plugin_task(
                name,
                args,
                session_id,
                default_to_current_session,
                sessions,
                session_lifecycle,
                session_graph,
                processes,
                scoped_effect_controller,
                cancellation_token,
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
    pub(crate) async fn run_plugin_task(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
        default_to_current_session: bool,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        scoped_effect_controller: crate::ScopedEffectController<'static>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<(String, ErasedPluginOperationOutcome), PluginOperationInvokeError> {
        self.invoke_plugin_operation(
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
        .await
    }
}
