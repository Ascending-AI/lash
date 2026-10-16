use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::tool_run::CallbackSlot;

use super::*;

#[derive(Clone)]
pub(crate) struct RegisteredHook<T> {
    pub(crate) identity: PluginCallbackIdentity,
    pub(crate) hook: T,
}

pub(crate) type RegisteredExclusiveHook<T> = RegisteredHook<T>;
#[derive(Clone)]
pub(crate) struct RegisteredProvider<T> {
    pub(crate) identity: ToolProviderHandle,
    pub(crate) hook: T,
}

/// The identity of `owner`'s callback named `key` in `slot`.
fn keyed_identity(owner: &str, slot: CallbackSlot, key: HookKey) -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: owner.to_owned(),
        key: format!("{}:{key}", slot.key_prefix()),
    }
}

fn refuse_duplicate_key<'a>(
    mut existing: impl Iterator<Item = &'a PluginCallbackIdentity>,
    identity: &PluginCallbackIdentity,
) -> Result<(), PluginError> {
    if existing.any(|registered| registered == identity) {
        return Err(PluginError::Registration(format!(
            "duplicate hook key `{}` for plugin `{}`",
            identity.key, identity.owner
        )));
    }
    Ok(())
}

fn push_keyed_hook<T>(
    hooks: &mut Vec<RegisteredHook<T>>,
    owner: &str,
    slot: CallbackSlot,
    key: HookKey,
    hook: T,
) -> Result<(), PluginError> {
    let identity = keyed_identity(owner, slot, key);
    refuse_duplicate_key(
        hooks.iter().map(|registered| &registered.identity),
        &identity,
    )?;
    hooks.push(RegisteredHook { identity, hook });
    Ok(())
}

/// Registers a priority-ordered context hook under the key its trait's
/// `id()` declares.
fn push_prioritized_keyed_hook<T>(
    hooks: &mut Vec<(i32, RegisteredHook<T>)>,
    owner: &str,
    slot: CallbackSlot,
    id: &'static str,
    priority: i32,
    hook: T,
) -> Result<(), PluginError> {
    let identity = keyed_identity(owner, slot, HookKey::new(id)?);
    refuse_duplicate_key(
        hooks.iter().map(|(_, registered)| &registered.identity),
        &identity,
    )?;
    hooks.push((priority, RegisteredHook { identity, hook }));
    Ok(())
}

fn register_singleton_hook<H>(
    slot: &mut Option<RegisteredExclusiveHook<H>>,
    owner: &str,
    hook_kind: &str,
    callback_slot: CallbackSlot,
    hook: H,
) -> Result<(), PluginError> {
    if let Some(existing) = slot {
        return Err(PluginError::Registration(format!(
            "duplicate {hook_kind} for `{}`: `{}` conflicts with `{}`",
            callback_slot.key_prefix(),
            owner,
            existing.identity.owner,
        )));
    }
    *slot = Some(RegisteredHook {
        identity: PluginCallbackIdentity {
            owner: owner.to_owned(),
            key: callback_slot.key_prefix().into(),
        },
        hook,
    });
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct PluginContributions {
    pub(crate) inventory: Option<Arc<InstalledInventory>>,
    pub(crate) extensions: Vec<PluginExtensionContribution>,
    /// Plugins that kept their state view past registration (FIG-3712).
    pub(crate) state_retaining_plugins: Vec<String>,
    /// Each plugin's pure state reducers, by name (K10).
    pub(crate) state_reducers: BTreeMap<String, BTreeMap<String, super::StateReducer>>,
    pub(crate) tool_providers: Vec<RegisteredProvider<Arc<dyn ToolProvider>>>,
    /// The readers of the providers whose `Read` tools a cell reads, each
    /// under its provider's identity.
    pub(crate) tool_reads: Vec<RegisteredProvider<Arc<dyn crate::ToolReads>>>,
    /// The one deferred provider, registered among `tool_providers` too.
    pub(crate) deferred_provider: Option<RegisteredProvider<Arc<dyn crate::DeferredToolProvider>>>,
    pub(crate) tool_catalog_contributors: Vec<RegisteredHook<ToolCatalogContributor>>,
    pub(crate) before_turn_hooks: Vec<RegisteredHook<BeforeTurnHook>>,
    pub(crate) tool_args_transforms: Vec<RegisteredHook<ToolArgsTransformHook>>,
    pub(crate) tool_args_checks: Vec<RegisteredHook<ToolArgsCheckHook>>,
    pub(crate) tool_result_transforms: Vec<RegisteredHook<ToolResultTransformHook>>,
    pub(crate) tool_result_checks: Vec<RegisteredHook<ToolResultCheckHook>>,
    pub(crate) after_turn_hooks: Vec<RegisteredHook<AfterTurnHook>>,
    pub(crate) checkpoint_hooks: Vec<RegisteredHook<CheckpointHook>>,
    pub(crate) assistant_stream_hooks: Vec<RegisteredHook<AssistantStreamHook>>,
    pub(crate) assistant_response_hooks: Vec<RegisteredHook<AssistantResponseHook>>,
    pub(crate) assistant_stream_finished_hooks: Vec<RegisteredHook<AssistantStreamFinishedHook>>,
    /// Each response callback that reads a stream-finished state, with the
    /// one same-plugin finished callback it names.
    pub(crate) assistant_stream_state_pairs: Vec<(PluginCallbackIdentity, PluginCallbackIdentity)>,
    /// Presentation steps compose in registration order (FIG-3420); no
    /// exclusive `model_observation` ownership exists anymore.
    pub(crate) presentation_steps: Vec<RegisteredHook<ToolPresentationStep>>,
    pub(crate) presentation_presenter: Option<RegisteredExclusiveHook<ToolPresentationPresenter>>,
    pub(crate) runtime_event_hooks: Vec<RegisteredHook<PluginLifecycleEventHook>>,
    pub(crate) plugin_operations: BTreeMap<String, RegisteredPluginOperation>,
    pub(crate) attachment_omission_policies: Vec<(
        i32,
        RegisteredHook<Arc<dyn super::AttachmentOmissionPolicy>>,
    )>,
    pub(crate) context_compactors: Vec<(i32, RegisteredHook<Arc<dyn ContextCompactor>>)>,
    pub(crate) context_pressure_hooks: Vec<(i32, RegisteredHook<Arc<dyn ContextPressureHook>>)>,
    pub(crate) protocol_session: Option<RegisteredExclusiveHook<Arc<dyn ProtocolSessionPlugin>>>,
    pub(crate) protocol_driver: Option<RegisteredExclusiveHook<Arc<dyn ProtocolDriverPlugin>>>,
    pub(crate) code_executor: Option<RegisteredExclusiveHook<Arc<dyn CodeExecutorPlugin>>>,
    pub(crate) cell_failure_feedback: Vec<RegisteredHook<CellFailureFeedbackHook>>,
    pub(crate) assistant_prose_projector:
        Option<RegisteredExclusiveHook<Arc<dyn AssistantProseProjectorPlugin>>>,
    /// Prompt sections and wrappers, in registration order (ADR 0133).
    pub(crate) prompt: super::prompt::PromptRegistry,
}

impl PluginContributions {
    /// The callback slots these bound contributions hold, by the collection
    /// each slot binds into. Every collection is named, so a new one cannot
    /// go unaccounted.
    #[cfg(test)]
    pub(crate) fn callback_slots(&self) -> std::collections::BTreeSet<CallbackSlot> {
        let Self {
            inventory: _,
            extensions: _,
            state_retaining_plugins: _,
            state_reducers: _,
            tool_providers,
            tool_reads,
            deferred_provider,
            tool_catalog_contributors,
            before_turn_hooks,
            tool_args_transforms,
            tool_args_checks,
            tool_result_transforms,
            tool_result_checks,
            after_turn_hooks,
            checkpoint_hooks,
            assistant_stream_hooks,
            assistant_response_hooks,
            assistant_stream_finished_hooks,
            assistant_stream_state_pairs: _,
            presentation_steps,
            presentation_presenter,
            runtime_event_hooks,
            plugin_operations,
            attachment_omission_policies,
            context_compactors,
            context_pressure_hooks,
            protocol_session,
            protocol_driver,
            code_executor,
            cell_failure_feedback,
            assistant_prose_projector,
            prompt: _,
        } = self;
        [
            (
                !tool_providers.is_empty() || deferred_provider.is_some(),
                CallbackSlot::ToolProvider,
            ),
            (!tool_reads.is_empty(), CallbackSlot::ToolReads),
            (
                !tool_catalog_contributors.is_empty(),
                CallbackSlot::ToolCatalog,
            ),
            (!before_turn_hooks.is_empty(), CallbackSlot::BeforeTurn),
            (
                !tool_args_transforms.is_empty(),
                CallbackSlot::ToolArgsTransform,
            ),
            (!tool_args_checks.is_empty(), CallbackSlot::ToolArgsCheck),
            (
                !tool_result_transforms.is_empty(),
                CallbackSlot::ToolResultTransform,
            ),
            (
                !tool_result_checks.is_empty(),
                CallbackSlot::ToolResultCheck,
            ),
            (!after_turn_hooks.is_empty(), CallbackSlot::AfterTurn),
            (!checkpoint_hooks.is_empty(), CallbackSlot::Checkpoint),
            (
                !assistant_stream_hooks.is_empty(),
                CallbackSlot::AssistantStream,
            ),
            (
                !assistant_response_hooks.is_empty(),
                CallbackSlot::AssistantResponse,
            ),
            (
                !assistant_stream_finished_hooks.is_empty(),
                CallbackSlot::AssistantStreamFinished,
            ),
            (
                !presentation_steps.is_empty(),
                CallbackSlot::PresentationStep,
            ),
            (
                presentation_presenter.is_some(),
                CallbackSlot::PresentationPresenter,
            ),
            (!runtime_event_hooks.is_empty(), CallbackSlot::RuntimeEvent),
            (!plugin_operations.is_empty(), CallbackSlot::Operation),
            (
                !attachment_omission_policies.is_empty(),
                CallbackSlot::AttachmentOmission,
            ),
            (
                !context_compactors.is_empty(),
                CallbackSlot::ContextCompactor,
            ),
            (
                !context_pressure_hooks.is_empty(),
                CallbackSlot::ContextPressure,
            ),
            (protocol_session.is_some(), CallbackSlot::ProtocolSession),
            (protocol_driver.is_some(), CallbackSlot::ProtocolDriver),
            (code_executor.is_some(), CallbackSlot::CodeExecutor),
            (
                !cell_failure_feedback.is_empty(),
                CallbackSlot::CellFailureFeedback,
            ),
            (
                assistant_prose_projector.is_some(),
                CallbackSlot::AssistantProseProjector,
            ),
        ]
        .into_iter()
        .filter_map(|(held, slot)| held.then_some(slot))
        .collect()
    }
}

pub(crate) struct BoundRegistrar {
    registry: RegistryKind,
    pub(crate) contributions: PluginContributions,
    pub(crate) owner: String,
}

pub(crate) struct ToolRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl ToolRegistrations<'_> {
    /// Register `provider` under `key`, its static identity within this
    /// plugin: every call bound to one of its tools records and dispatches
    /// through that identity, whatever order the plugin registers its
    /// providers in. A key begins with a letter; a duplicate key is refused.
    pub fn provider(
        self,
        key: ToolProviderKey,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), PluginError> {
        self.reg.add_tool_provider(key, provider)
    }

    /// Register `registration`'s provider under `key`, with the reader that
    /// answers its `Read` tools.
    ///
    /// # Errors
    ///
    /// A duplicate key, or a registration that supplies no reader.
    pub fn provider_with_reads(
        self,
        key: ToolProviderKey,
        registration: crate::ToolProviderRegistration,
    ) -> Result<(), PluginError> {
        let identity = ToolProviderHandle::new(self.reg.registry, self.reg.owner.clone(), key);
        let Some(reads) = registration.reads else {
            return Err(PluginError::Registration(format!(
                "provider `{identity}` was declared with reads, and its registration supplies no reader"
            )));
        };
        self.reg.add_tool_provider(key, registration.provider)?;
        self.reg.contributions.tool_reads.push(RegisteredProvider {
            identity,
            hook: reads,
        });
        Ok(())
    }

    /// Register `provider` under `key` as the deferred provider: a tool
    /// provider that advertises nothing and resolves the call paths a
    /// surface does not offer. A session or process env has at most one; a
    /// second, from any plugin, is refused.
    pub fn deferred(
        self,
        key: ToolProviderKey,
        registration: crate::DeferredToolProviderRegistration,
    ) -> Result<(), PluginError> {
        let identity = ToolProviderHandle::new(self.reg.registry, self.reg.owner.clone(), key);
        if let Some(existing) = &self.reg.contributions.deferred_provider {
            return Err(PluginError::Registration(format!(
                "deferred tool provider `{identity}` conflicts with `{}`; a session or process \
                 env has at most one",
                existing.identity
            )));
        }
        let provider = registration.provider;
        self.reg
            .add_tool_provider(key, Arc::clone(&provider) as Arc<dyn ToolProvider>)?;
        self.reg.contributions.deferred_provider = Some(RegisteredProvider {
            identity,
            hook: provider,
        });
        Ok(())
    }
}

pub(crate) struct ToolCatalogRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl ToolCatalogRegistrations<'_> {
    /// Catalog contributions only remove members; the union of every
    /// contributor's removals applies, so their order does not matter.
    pub fn contribute(
        self,
        key: HookKey,
        contributor: ToolCatalogContributor,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.tool_catalog_contributors,
            &self.reg.owner,
            CallbackSlot::ToolCatalog,
            key,
            contributor,
        )
    }
}

pub(crate) struct TurnRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

/// Turn observers run sequentially in recorded registration order. Each
/// returns declared contributions the turn applies at its owned boundary;
/// an observer has no veto over the turn it observes.
impl TurnRegistrations<'_> {
    pub fn before(self, key: HookKey, hook: BeforeTurnHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.before_turn_hooks,
            &self.reg.owner,
            CallbackSlot::BeforeTurn,
            key,
            hook,
        )
    }

    pub fn after(self, key: HookKey, hook: AfterTurnHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.after_turn_hooks,
            &self.reg.owner,
            CallbackSlot::AfterTurn,
            key,
            hook,
        )
    }

    /// Runs at the machine's checkpoints, between a round's outcomes and the
    /// next model call. Its decisions commit with that call's `model.start`
    /// (ADR 0133 §6): a crash before it runs the hook again, so the hook must
    /// be repeat-safe until the call is admitted; after it, nothing runs it
    /// again.
    pub fn checkpoint(self, key: HookKey, hook: CheckpointHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.checkpoint_hooks,
            &self.reg.owner,
            CallbackSlot::Checkpoint,
            key,
            hook,
        )
    }
}

pub(crate) struct ToolCallRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

/// The four tool hook phases (ADR 0128). Transforms chain once in recorded
/// registration order; checks all inspect one immutable value and reduce by
/// strength, then plugin id, then callback key.
impl ToolCallRegistrations<'_> {
    /// Rewrites a call's arguments before they are validated and the
    /// provider prepares the call. Return the value even when unchanged.
    pub fn transform_args(
        self,
        key: HookKey,
        hook: ToolArgsTransformHook,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.tool_args_transforms,
            &self.reg.owner,
            CallbackSlot::ToolArgsTransform,
            key,
            hook,
        )
    }

    /// Decides over the final prepared call: allow it, serve a cached
    /// success, deny or cancel it, or stop the Run.
    pub fn check_args(self, key: HookKey, hook: ToolArgsCheckHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.tool_args_checks,
            &self.reg.owner,
            CallbackSlot::ToolArgsCheck,
            key,
            hook,
        )
    }

    /// Rewrites a completed result before the after-checks inspect it. This
    /// is where a result is normalized or recovered.
    pub fn transform_result(
        self,
        key: HookKey,
        hook: ToolResultTransformHook,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.tool_result_transforms,
            &self.reg.owner,
            CallbackSlot::ToolResultTransform,
            key,
            hook,
        )
    }

    /// Decides over the final result: allow it, deny or cancel the call, or
    /// stop the Run. A check never replaces the result.
    pub fn check_result(self, key: HookKey, hook: ToolResultCheckHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.tool_result_checks,
            &self.reg.owner,
            CallbackSlot::ToolResultCheck,
            key,
            hook,
        )
    }
}

pub(crate) struct OutputRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl OutputRegistrations<'_> {
    pub fn cell_failure_feedback(
        self,
        key: HookKey,
        hook: CellFailureFeedbackHook,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.cell_failure_feedback,
            &self.reg.owner,
            CallbackSlot::CellFailureFeedback,
            key,
            hook,
        )
    }

    /// Chunk transforms chain in recorded registration order; any hook that
    /// asks to stop the stream stops it.
    pub fn stream(self, key: HookKey, hook: AssistantStreamHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.assistant_stream_hooks,
            &self.reg.owner,
            CallbackSlot::AssistantStream,
            key,
            hook,
        )
    }

    /// Response transforms chain in recorded registration order. The hook
    /// must be idempotent: see [`AssistantResponseHook`] for the
    /// at-least-once contract it runs under.
    ///
    /// `stream_state_from` names the one stream-finished callback of this
    /// plugin whose state this callback receives; registration fails when
    /// this plugin registers no finished callback under that key.
    pub fn response(
        self,
        key: HookKey,
        stream_state_from: Option<HookKey>,
        hook: AssistantResponseHook,
    ) -> Result<(), PluginError> {
        let identity = keyed_identity(&self.reg.owner, CallbackSlot::AssistantResponse, key);
        push_keyed_hook(
            &mut self.reg.contributions.assistant_response_hooks,
            &self.reg.owner,
            CallbackSlot::AssistantResponse,
            key,
            hook,
        )?;
        if let Some(finished) = stream_state_from {
            self.reg.contributions.assistant_stream_state_pairs.push((
                identity,
                keyed_identity(
                    &self.reg.owner,
                    CallbackSlot::AssistantStreamFinished,
                    finished,
                ),
            ));
        }
        Ok(())
    }

    /// Runs once a provider stream finishes, including after a reset,
    /// cancellation or provider error. Its state reaches the response
    /// callbacks that name this key as their `stream_state_from`, recorded
    /// under each receiving callback's identity.
    pub fn stream_finished(
        self,
        key: HookKey,
        hook: AssistantStreamFinishedHook,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.assistant_stream_finished_hooks,
            &self.reg.owner,
            CallbackSlot::AssistantStreamFinished,
            key,
            hook,
        )
    }

    pub fn assistant_prose_projector(
        self,
        provider: Arc<dyn AssistantProseProjectorPlugin>,
    ) -> Result<(), PluginError> {
        self.reg.add_assistant_prose_projector(provider)
    }
}

pub(crate) struct ToolResultRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl ToolResultRegistrations<'_> {
    pub fn presenter(self, presenter: ToolPresentationPresenter) -> Result<(), PluginError> {
        self.reg.add_presentation_presenter(presenter)
    }

    /// Appends one composable presentation step; steps run in registration
    /// order inside the journaled `PresentToolResult` boundary (FIG-3420).
    pub fn presentation_step(
        self,
        key: HookKey,
        step: ToolPresentationStep,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.presentation_steps,
            &self.reg.owner,
            CallbackSlot::PresentationStep,
            key,
            step,
        )
    }
}

pub(crate) struct SessionRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl SessionRegistrations<'_> {
    /// A best-effort, read-only lifecycle observer. Observers receive each
    /// event in recorded registration order; their failures are reported and
    /// never undo what they observed.
    pub fn on_event(self, key: HookKey, hook: PluginLifecycleEventHook) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.runtime_event_hooks,
            &self.reg.owner,
            CallbackSlot::RuntimeEvent,
            key,
            hook,
        )
    }
}

pub(crate) struct PluginOperationRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl PluginOperationRegistrations<'_> {
    pub(crate) fn query(
        self,
        spec: PluginOperationSpec,
        handler: PluginQueryHandler,
    ) -> Result<(), PluginError> {
        self.reg
            .add_plugin_operation(PluginOperationRegistration::query(spec, handler))
    }

    pub(crate) fn command(
        self,
        spec: PluginOperationSpec,
        handler: PluginCommandHandler,
    ) -> Result<(), PluginError> {
        self.reg
            .add_plugin_operation(PluginOperationRegistration::command(spec, handler))
    }

    pub(crate) fn task(
        self,
        spec: PluginOperationSpec,
        handler: PluginTaskHandler,
    ) -> Result<(), PluginError> {
        self.reg
            .add_plugin_operation(PluginOperationRegistration::task(spec, handler))
    }

    pub(crate) fn register(
        self,
        operation: PluginOperationRegistration,
    ) -> Result<(), PluginError> {
        self.reg.add_plugin_operation(operation)
    }

    pub fn typed_query<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginQuery,
        F: Fn(PluginQueryContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        self.query(
            plugin_operation_spec::<Op>(),
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

    pub fn typed_command<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut:
            Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>> + Send + 'static,
    {
        self.command(
            plugin_operation_spec::<Op>(),
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
                            Ok(ErasedPluginOperationOutcome {
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

    pub fn typed_command_value<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        self.typed_command::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginOperationOutcome::new) }
        })
    }

    pub fn typed_task<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut:
            Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>> + Send + 'static,
    {
        self.task(
            plugin_operation_spec::<Op>(),
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
                            Ok(ErasedPluginOperationOutcome {
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

    pub fn typed_task_value<Op, F, Fut>(self, handler: F) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        self.typed_task::<Op, _, _>(move |ctx, args| {
            let fut = handler(ctx, args);
            async move { fut.await.map(PluginOperationOutcome::new) }
        })
    }
}

pub(crate) struct ContextRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

/// Context hooks are keyed by their trait's `id()`. Higher priority runs
/// first; equal priorities keep recorded registration order.
impl ContextRegistrations<'_> {
    /// Attachment-omission history policies (ADR 0133): each names the
    /// attachments of the turn's projected history its request omits; core
    /// omits the union.
    pub fn attachment_omissions(
        self,
        key: HookKey,
        priority: i32,
        policy: Arc<dyn super::AttachmentOmissionPolicy>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.attachment_omission_policies,
            &self.reg.owner,
            CallbackSlot::AttachmentOmission,
            key.as_str(),
            priority,
            policy,
        )
    }

    /// The first compactor that returns a nonempty compaction decides.
    pub fn compact(
        self,
        key: HookKey,
        priority: i32,
        compactor: Arc<dyn ContextCompactor>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.context_compactors,
            &self.reg.owner,
            CallbackSlot::ContextCompactor,
            key.as_str(),
            priority,
            compactor,
        )
    }

    /// Record contributions accumulate; the first hook that opens a frame is
    /// the last one called for that turn.
    pub fn pressure(
        self,
        key: HookKey,
        priority: i32,
        hook: Arc<dyn ContextPressureHook>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.context_pressure_hooks,
            &self.reg.owner,
            CallbackSlot::ContextPressure,
            key.as_str(),
            priority,
            hook,
        )
    }
}

pub(crate) struct ProtocolRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
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

pub(crate) struct ExecutionRegistrations<'a> {
    reg: &'a mut BoundRegistrar,
}

impl ExecutionRegistrations<'_> {
    pub fn code_executor(self, provider: Arc<dyn CodeExecutorPlugin>) -> Result<(), PluginError> {
        self.reg.add_code_executor(provider)
    }
}

impl BoundRegistrar {
    pub(crate) fn for_process(owner: String) -> Self {
        Self {
            registry: RegistryKind::Process,
            ..Self::new(owner)
        }
    }

    /// Register the pure reducer [`StateCommands::apply`](super::StateCommands::apply)
    /// names `name` by, for this plugin's namespace.
    pub fn state_reducer(
        &mut self,
        name: impl Into<String>,
        reducer: super::StateReducer,
    ) -> Result<(), PluginError> {
        let name = name.into();
        let reducers = self
            .contributions
            .state_reducers
            .entry(self.owner.clone())
            .or_default();
        if reducers.contains_key(&name) {
            return Err(PluginError::Registration(format!(
                "duplicate state reducer `{name}` for plugin `{}`",
                self.owner
            )));
        }
        reducers.insert(name, reducer);
        Ok(())
    }

    pub(crate) fn new(owner: String) -> Self {
        Self {
            registry: RegistryKind::Session,
            contributions: PluginContributions::default(),
            owner,
        }
    }

    pub fn tools(&mut self) -> ToolRegistrations<'_> {
        ToolRegistrations { reg: self }
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

    fn add_tool_provider(
        &mut self,
        key: ToolProviderKey,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), PluginError> {
        let identity = ToolProviderHandle::new(self.registry, self.owner.clone(), key);
        if self
            .contributions
            .tool_providers
            .iter()
            .any(|entry| entry.identity == identity)
        {
            return Err(PluginError::Registration(format!(
                "duplicate provider {identity}"
            )));
        }
        self.contributions.tool_providers.push(RegisteredProvider {
            identity,
            hook: provider,
        });
        Ok(())
    }

    fn add_assistant_prose_projector(
        &mut self,
        provider: Arc<dyn AssistantProseProjectorPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.assistant_prose_projector,
            &self.owner,
            "assistant prose projector",
            CallbackSlot::AssistantProseProjector,
            provider,
        )
    }

    fn add_presentation_presenter(
        &mut self,
        presenter: ToolPresentationPresenter,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.presentation_presenter,
            &self.owner,
            "tool presentation presenter",
            CallbackSlot::PresentationPresenter,
            presenter,
        )
    }

    fn ensure_unique_operation_name(&self, name: &str) -> Result<(), PluginError> {
        if self.contributions.plugin_operations.contains_key(name) {
            return Err(PluginError::Registration(format!(
                "duplicate plugin operation name `{name}`"
            )));
        }
        Ok(())
    }

    fn add_plugin_operation(
        &mut self,
        operation: PluginOperationRegistration,
    ) -> Result<(), PluginError> {
        self.ensure_unique_operation_name(&operation.def().name)?;
        if operation.def().error_type.is_empty() {
            return Err(PluginError::Registration(
                "plugin operation error type is empty".into(),
            ));
        }
        crate::JsonSchema::admit(operation.def().error_schema.clone()).map_err(|source| {
            PluginError::UnusableSchema {
                source: Box::new(source),
            }
        })?;
        let identity = PluginCallbackIdentity {
            owner: self.owner.clone(),
            key: format!(
                "{}:{}",
                CallbackSlot::Operation.key_prefix(),
                operation.def().name
            ),
        };
        self.contributions.plugin_operations.insert(
            operation.def().name.clone(),
            RegisteredPluginOperation::new(identity, operation),
        );
        Ok(())
    }

    fn add_protocol_session(
        &mut self,
        provider: Arc<dyn ProtocolSessionPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.protocol_session,
            &self.owner,
            "protocol session capability",
            CallbackSlot::ProtocolSession,
            provider,
        )
    }

    fn add_code_executor(
        &mut self,
        provider: Arc<dyn CodeExecutorPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.code_executor,
            &self.owner,
            "code executor capability",
            CallbackSlot::CodeExecutor,
            provider,
        )
    }

    fn add_protocol_driver(
        &mut self,
        provider: Arc<dyn ProtocolDriverPlugin>,
    ) -> Result<(), PluginError> {
        register_singleton_hook(
            &mut self.contributions.protocol_driver,
            &self.owner,
            "protocol driver capability",
            CallbackSlot::ProtocolDriver,
            provider,
        )
    }

    /// Refuses a response callback of this plugin that names a
    /// stream-finished key this plugin never registered.
    pub(crate) fn validate_stream_state_pairs(&self) -> Result<(), PluginError> {
        for (response, finished) in &self.contributions.assistant_stream_state_pairs {
            if response.owner != self.owner {
                continue;
            }
            let registered = self
                .contributions
                .assistant_stream_finished_hooks
                .iter()
                .any(|hook| &hook.identity == finished);
            if !registered {
                return Err(PluginError::Registration(format!(
                    "response callback `{}` of plugin `{}` reads stream state from `{}`, which the plugin does not register",
                    response.key, response.owner, finished.key
                )));
            }
        }
        Ok(())
    }
}
