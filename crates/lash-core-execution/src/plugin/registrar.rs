use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::tool_run::CallbackSlot;

use super::*;

#[derive(Clone)]
pub(crate) struct RegisteredHook<T> {
    pub(crate) identity: PluginCallbackIdentity,
    pub(crate) hook: T,
}

pub(crate) type RegisteredExclusiveHook<T> = RegisteredHook<T>;

/// Registers a routed definition (a tool provider) under its per-plugin
/// ordinal: providers dispatch by tool authority, not by callback order.
fn push_ordinal_hook<T>(
    hooks: &mut Vec<RegisteredHook<T>>,
    owner: &PluginRevision,
    slot: CallbackSlot,
    hook: T,
) {
    let ordinal = hooks
        .iter()
        .filter(|registered| registered.identity.owner.plugin == owner.plugin)
        .count();
    hooks.push(RegisteredHook {
        identity: PluginCallbackIdentity {
            owner: owner.clone(),
            key: format!("{}:{ordinal}", slot.key_prefix()),
        },
        hook,
    });
}

/// The identity of `owner`'s callback named `key` in `slot`.
fn keyed_identity(
    owner: &PluginRevision,
    slot: CallbackSlot,
    key: HookKey,
) -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: owner.clone(),
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
            identity.key, identity.owner.plugin
        )));
    }
    Ok(())
}

fn push_keyed_hook<T>(
    hooks: &mut Vec<RegisteredHook<T>>,
    owner: &PluginRevision,
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
    owner: &PluginRevision,
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
    owner: &PluginRevision,
    hook_kind: &str,
    callback_slot: CallbackSlot,
    hook: H,
) -> Result<(), PluginError> {
    if let Some(existing) = slot {
        return Err(PluginError::Registration(format!(
            "duplicate {hook_kind} for `{}`: `{}` conflicts with `{}`",
            callback_slot.key_prefix(),
            owner.plugin,
            existing.identity.owner.plugin,
        )));
    }
    *slot = Some(RegisteredHook {
        identity: PluginCallbackIdentity {
            owner: owner.clone(),
            key: callback_slot.key_prefix().into(),
        },
        hook,
    });
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct PluginContributions {
    /// Plugins that kept their state view past registration (FIG-3712).
    pub(crate) state_retaining_plugins: Vec<String>,
    /// Each plugin's pure state reducers, by name (K10).
    pub(crate) state_reducers: BTreeMap<String, BTreeMap<String, super::StateReducer>>,
    pub(crate) tool_providers: Vec<RegisteredHook<Arc<dyn ToolProvider>>>,
    pub(crate) triggers: Vec<crate::TriggerEvent>,
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
    pub(crate) transcript_row_projectors:
        Vec<RegisteredHook<Arc<dyn TranscriptRowProjectorPlugin>>>,
    pub(crate) assistant_prose_projector:
        Option<RegisteredExclusiveHook<Arc<dyn AssistantProseProjectorPlugin>>>,
    /// Prompt sections and wrappers, in registration order (ADR 0133).
    pub(crate) prompt: super::prompt::PromptRegistry,
}

pub struct PluginRegistrar {
    pub(super) state: Option<super::PluginStateView>,
    pub(crate) contributions: PluginContributions,
    pub(crate) owner: PluginRevision,
    pub(super) tool_names: BTreeSet<String>,
}

pub struct ToolRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl ToolRegistrations<'_> {
    pub fn provider(self, provider: Arc<dyn ToolProvider>) -> Result<(), PluginError> {
        self.reg.add_tool_provider(provider)
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

pub struct ToolCatalogRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
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

pub struct TurnRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
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

pub struct ToolCallRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
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

pub struct OutputRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

impl OutputRegistrations<'_> {
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

    pub fn transcript_projector(
        self,
        key: HookKey,
        projector: Arc<dyn TranscriptRowProjectorPlugin>,
    ) -> Result<(), PluginError> {
        push_keyed_hook(
            &mut self.reg.contributions.transcript_row_projectors,
            &self.reg.owner,
            CallbackSlot::TranscriptProjector,
            key,
            projector,
        )
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

pub struct SessionRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
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

pub struct PluginOperationRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
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

pub struct ContextRegistrations<'a> {
    reg: &'a mut PluginRegistrar,
}

/// Context hooks are keyed by their trait's `id()`. Higher priority runs
/// first; equal priorities keep recorded registration order.
impl ContextRegistrations<'_> {
    /// Attachment-omission history policies (ADR 0133): each names the
    /// attachments of the turn's projected history its request omits; core
    /// omits the union.
    pub fn attachment_omissions(
        self,
        priority: i32,
        policy: Arc<dyn super::AttachmentOmissionPolicy>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.attachment_omission_policies,
            &self.reg.owner,
            CallbackSlot::AttachmentOmission,
            policy.id(),
            priority,
            policy,
        )
    }

    /// The first compactor that returns a nonempty compaction decides.
    pub fn compact(
        self,
        priority: i32,
        compactor: Arc<dyn ContextCompactor>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.context_compactors,
            &self.reg.owner,
            CallbackSlot::ContextCompactor,
            compactor.id(),
            priority,
            compactor,
        )
    }

    /// Record contributions accumulate; the first hook that opens a frame is
    /// the last one called for that turn.
    pub fn pressure(
        self,
        priority: i32,
        hook: Arc<dyn ContextPressureHook>,
    ) -> Result<(), PluginError> {
        push_prioritized_keyed_hook(
            &mut self.reg.contributions.context_pressure_hooks,
            &self.reg.owner,
            CallbackSlot::ContextPressure,
            hook.id(),
            priority,
            hook,
        )
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
    /// A read-only view of the host-owned namespace bound to the registering
    /// plugin. A plugin changes its namespace only by returning
    /// [`StateCommands`](super::StateCommands) from a tool body or a
    /// before-turn, after-turn, checkpoint or after-tool callback.
    #[expect(
        clippy::expect_used,
        reason = "`PluginRegistrar::new` is private and every registrar reaching a plugin is bound by `bind_state` first"
    )]
    pub fn state(&self) -> super::PluginStateView {
        self.state
            .clone()
            .expect("registrar is bound during registration")
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
            .entry(self.owner.plugin.clone())
            .or_default();
        if reducers.contains_key(&name) {
            return Err(PluginError::Registration(format!(
                "duplicate state reducer `{name}` for plugin `{}`",
                self.owner.plugin
            )));
        }
        reducers.insert(name, reducer);
        Ok(())
    }

    pub(crate) fn new(owner: PluginRevision) -> Self {
        Self {
            state: None,
            contributions: PluginContributions::default(),
            owner,
            tool_names: BTreeSet::new(),
        }
    }

    pub fn tools(&mut self) -> ToolRegistrations<'_> {
        ToolRegistrations { reg: self }
    }

    pub fn triggers(&mut self) -> TriggerEventRegistrations<'_> {
        TriggerEventRegistrations { reg: self }
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
        push_ordinal_hook(
            &mut self.contributions.tool_providers,
            &self.owner,
            CallbackSlot::ToolProvider,
            provider,
        );
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
                    response.key, response.owner.plugin, finished.key
                )));
            }
        }
        Ok(())
    }
}
