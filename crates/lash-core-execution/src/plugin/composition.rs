//! Immutable execution contributions shared by session and process dispatch.
use super::prompt::{PromptCatalog, PromptRegistry};
use super::*;

use lash_core_store::tool_run::{AttributedVerdict, CheckRecord};
use std::sync::Arc;

/// Built once per materialization, in owner and registration order.
pub struct ExecutionComposition {
    _inventory: Option<Arc<InstalledInventory>>,
    callbacks: Vec<PluginCallbackIdentity>,
    tool_providers: Vec<super::registrar::RegisteredProvider<Arc<dyn ToolProvider>>>,
    tool_catalog_contributors: Vec<RegisteredHook<ToolCatalogContributor>>,
    tool_args_transforms: Vec<RegisteredHook<ToolArgsTransformHook>>,
    tool_args_checks: Vec<RegisteredHook<ToolArgsCheckHook>>,
    tool_result_transforms: Vec<RegisteredHook<ToolResultTransformHook>>,
    tool_result_checks: Vec<RegisteredHook<ToolResultCheckHook>>,
    presenter: Option<RegisteredHook<ToolPresentationPresenter>>,
    presentation_steps: Vec<RegisteredHook<ToolPresentationStep>>,
    prompt: PromptRegistry,
    tool_registry: Arc<crate::ToolRegistry>,
}

impl ExecutionComposition {
    pub(super) fn new(
        contributions: &mut PluginContributions,
        tool_registry: Arc<crate::ToolRegistry>,
    ) -> Self {
        Self {
            _inventory: contributions.inventory.clone(),
            callbacks: contributions
                .inventory
                .as_ref()
                .map(|inventory| inventory.callbacks().cloned().collect())
                .unwrap_or_default(),
            tool_providers: std::mem::take(&mut contributions.tool_providers),
            tool_catalog_contributors: std::mem::take(&mut contributions.tool_catalog_contributors),
            tool_args_transforms: std::mem::take(&mut contributions.tool_args_transforms),
            tool_args_checks: std::mem::take(&mut contributions.tool_args_checks),
            tool_result_transforms: std::mem::take(&mut contributions.tool_result_transforms),
            tool_result_checks: std::mem::take(&mut contributions.tool_result_checks),
            presenter: contributions.presentation_presenter.take(),
            presentation_steps: std::mem::take(&mut contributions.presentation_steps),
            prompt: std::mem::take(&mut contributions.prompt),
            tool_registry,
        }
    }

    pub fn prompt_catalog(&self) -> PromptCatalog {
        PromptCatalog::new(self.prompt.clone())
    }

    pub(super) fn tool_catalog_contributors(&self) -> &[RegisteredHook<ToolCatalogContributor>] {
        &self.tool_catalog_contributors
    }

    /// The lookup recorded callback identities resolve through against
    /// this materialization's installed registrations.
    pub(crate) fn recorded(&self) -> super::recorded_lookup::RecordedLookup<'_> {
        super::recorded_lookup::RecordedLookup::new(&self.callbacks)
    }

    /// The provider a call of `tool` binds to and dispatches through: the
    /// one its surface entry or deferred match records, `provider`, which
    /// must be registered here. A tool with no entry and no match has no
    /// provider.
    pub fn tool_provider(
        &self,
        tool: &crate::ToolId,
        provider: Option<&ToolProviderHandle>,
    ) -> Result<ToolProviderHandle, PluginError> {
        let provider = provider.cloned().ok_or_else(|| {
            PluginError::Registration(format!(
                "tool `{tool}` is neither on the run's surface nor a recorded match"
            ))
        })?;
        if !self
            .tool_providers
            .iter()
            .any(|registered| registered.identity == provider)
        {
            return Err(PluginError::Registration(format!(
                "tool `{tool}` names provider `{provider}`, which is not registered"
            )));
        }
        Ok(provider)
    }

    pub(crate) fn tool_run_binding(
        &self,
        tool: &crate::ToolId,
        provider: Option<&ToolProviderHandle>,
    ) -> Result<crate::tool_run::AdmittedBinding, PluginError> {
        let executable = self.tool_provider(tool, provider)?.callback_identity();
        Ok(crate::tool_run::AdmittedBinding {
            preparation: executable.clone(),
            executable,
            presentation: self.tool_presentation_plan(),
        })
    }

    /// The deferred provider's key, when one is registered: what resolves
    /// the call paths a surface does not offer.
    pub fn deferred_key(&self) -> Option<ToolProviderHandle> {
        self.tool_registry.deferred_key().cloned()
    }

    /// Bind recorded provider identities to this worker's capabilities.
    pub fn resolve_context_tool_bindings(
        &self,
        bindings: &[crate::plugin::PluginCallbackIdentity],
    ) -> Result<Vec<Arc<dyn crate::ToolProvider>>, PluginError> {
        let recorded = self.recorded();
        bindings
            .iter()
            .map(|binding| {
                self.tool_providers
                    .iter()
                    .find(|registered| registered.identity.callback_identity() == *binding)
                    .map(|registered| Arc::clone(&registered.hook))
                    .ok_or_else(|| recorded.unavailable(binding))
            })
            .collect()
    }

    /// Every hook that sees a tool call's arguments or its result: what a
    /// call that runs no hook, as a read does, would pass by.
    pub(crate) fn tool_call_hooks(&self) -> Vec<PluginCallbackIdentity> {
        self.tool_args_transforms
            .iter()
            .map(|hook| hook.identity.clone())
            .chain(
                self.tool_args_checks
                    .iter()
                    .map(|hook| hook.identity.clone()),
            )
            .chain(
                self.tool_result_transforms
                    .iter()
                    .map(|hook| hook.identity.clone()),
            )
            .chain(
                self.tool_result_checks
                    .iter()
                    .map(|hook| hook.identity.clone()),
            )
            .collect()
    }

    /// Chain every argument transform once, in recorded registration order.
    /// A transform's failure ends the chain: there is no valid value to
    /// continue from.
    pub(crate) async fn transform_tool_args(
        &self,
        context: &ToolHookContext,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, Box<crate::ToolFailure>> {
        let original = Arc::new(args);
        let mut current = (*original).clone();
        for registered in &self.tool_args_transforms {
            current = context
                .check_phase
                .run(context.clock.as_ref(), &registered.identity, || {
                    (registered.hook)(ToolArgsTransformInput {
                        context: context.clone(),
                        original: Arc::clone(&original),
                        current,
                    })
                })
                .await
                .map_err(|error| {
                    Box::new(failed_transform(
                        ToolHookPhase::ArgsTransform,
                        &registered.identity,
                        &error,
                    ))
                })?;
        }
        Ok(current)
    }

    /// Ask every before-check, sequentially, about the one prepared call and
    /// reduce their replies. A check that fails denies; it never allows.
    pub(crate) async fn check_tool_args(
        &self,
        context: &ToolHookContext,
        original_args: &Arc<serde_json::Value>,
        prepared: &PreparedCallReadView,
    ) -> Result<CheckRecord<BeforeToolDecision>, Box<crate::ToolFailure>> {
        let mut replies = Vec::with_capacity(self.tool_args_checks.len());
        for registered in &self.tool_args_checks {
            let verdict = context
                .check_phase
                .run(context.clock.as_ref(), &registered.identity, || {
                    (registered.hook)(ToolArgsCheckInput {
                        context: context.clone(),
                        original_args: Arc::clone(original_args),
                        prepared: prepared.clone(),
                    })
                })
                .await
                .unwrap_or_else(|error| {
                    BeforeToolDecision::Deny(failed_check(
                        ToolHookPhase::ArgsCheck,
                        &registered.identity,
                        &error,
                    ))
                });
            replies.push(AttributedVerdict {
                callback: registered.identity.clone(),
                verdict,
            });
        }
        retain_expiry_evidence(&mut replies, |verdict| match verdict {
            BeforeToolDecision::Deny(failure) => Some(failure),
            _ => None,
        })?;
        Ok(CheckRecord::reduce(replies))
    }

    /// Chain every result transform once, in recorded registration order,
    /// over the immutable original and the preceding candidate.
    pub(crate) async fn transform_tool_result(
        &self,
        context: &ToolHookContext,
        occurrence: ToolHookOccurrence,
        prepared: &PreparedCallReadView,
        original: &Arc<ToolResultCandidate>,
    ) -> Result<ToolResultCandidate, Box<crate::ToolFailure>> {
        let mut current = (**original).clone();
        for registered in &self.tool_result_transforms {
            current = context
                .check_phase
                .run(context.clock.as_ref(), &registered.identity, || {
                    (registered.hook)(ToolResultTransformInput {
                        context: context.clone(),
                        occurrence,
                        prepared: prepared.clone(),
                        original: Arc::clone(original),
                        current,
                    })
                })
                .await
                .map_err(|error| {
                    Box::new(failed_transform(
                        ToolHookPhase::ResultTransform,
                        &registered.identity,
                        &error,
                    ))
                })?;
        }
        Ok(current)
    }

    /// Ask every after-check, sequentially, about the one final candidate
    /// and reduce their verdicts. A check that fails denies.
    pub(crate) async fn check_tool_result(
        &self,
        context: &ToolHookContext,
        occurrence: ToolHookOccurrence,
        prepared: &PreparedCallReadView,
        original: &Arc<ToolResultCandidate>,
        final_result: &Arc<ToolResultCandidate>,
    ) -> Result<ResultChecks, Box<crate::ToolFailure>> {
        let mut replies = Vec::with_capacity(self.tool_result_checks.len());
        let mut contributions = Vec::new();
        let mut proposals = Vec::new();
        for registered in &self.tool_result_checks {
            let reply = context
                .check_phase
                .run(context.clock.as_ref(), &registered.identity, || {
                    (registered.hook)(ToolResultCheckInput {
                        context: context.clone(),
                        occurrence,
                        prepared: prepared.clone(),
                        original: Arc::clone(original),
                        final_result: Arc::clone(final_result),
                    })
                })
                .await;
            let verdict = match reply {
                Ok(AfterToolContributions {
                    verdict,
                    events,
                    state,
                }) => {
                    if !state.is_empty() {
                        proposals.push(super::Proposal::for_callback(
                            &registered.identity,
                            StateCommandOrigin::ToolHook {
                                occurrence: Box::new(lash_core_store::tool_run::HookOccurrence {
                                    call_id: context.call_id.clone(),
                                    callback: registered.identity.clone(),
                                    phase: ToolHookPhase::ResultCheck,
                                    occurrence,
                                }),
                            },
                            state,
                        ));
                    }
                    if !events.is_empty() {
                        contributions.push(AttributedContributions {
                            plugin_id: registered.identity.owner.clone(),
                            events,
                        });
                    }
                    verdict
                }
                Err(error) => AfterToolDecision::Deny(failed_check(
                    ToolHookPhase::ResultCheck,
                    &registered.identity,
                    &error,
                )),
            };
            replies.push(AttributedVerdict {
                callback: registered.identity.clone(),
                verdict,
            });
        }
        retain_expiry_evidence(&mut replies, |verdict| match verdict {
            AfterToolDecision::Deny(failure) => Some(failure),
            _ => None,
        })?;
        Ok(ResultChecks {
            record: CheckRecord::reduce(replies),
            contributions,
            proposals,
        })
    }

    pub(crate) fn has_tool_result_hooks(&self) -> bool {
        !self.tool_result_transforms.is_empty() || !self.tool_result_checks.is_empty()
    }
}
/// FIG-6092: the refusal's recorded raw evidence retains every verdict,
/// including answered Allows and one typed expiry for each unanswered check.
fn retain_expiry_evidence<
    V: Clone + serde::Serialize + lash_core_store::tool_run::RankedVerdict,
>(
    replies: &mut [AttributedVerdict<V>],
    failure: impl Fn(&mut V) -> Option<&mut crate::ToolFailure>,
) -> Result<(), Box<crate::ToolFailure>> {
    let expires = |failure: &crate::ToolFailure| {
        matches!(
            failure.cause.as_deref(),
            Some(crate::ToolFailureCause::ControlPhaseExpired { .. })
        )
    };
    if !replies
        .iter_mut()
        .any(|reply| failure(&mut reply.verdict).is_some_and(|cause| expires(cause)))
    {
        return Ok(());
    }
    let evidence =
        serde_json::to_value(CheckRecord::reduce(replies.to_vec())).map_err(|error| {
            Box::new(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "tool_check_evidence_encode_failed",
                error.to_string(),
            ))
        })?;
    for reply in replies {
        if let Some(cause) = failure(&mut reply.verdict)
            && expires(cause)
        {
            cause.raw = Some(crate::ToolValue::untrusted_json(evidence.clone()));
        }
    }
    Ok(())
}

struct ResolvedPresentationPlan<'a> {
    presenter: Option<&'a RegisteredHook<ToolPresentationPresenter>>,
    steps: Vec<&'a RegisteredHook<ToolPresentationStep>>,
}

impl ExecutionComposition {
    /// Select the singleton renderer and ordered steps without invoking them.
    /// The owning admission or presentation boundary records this selection.
    pub fn tool_presentation_plan(&self) -> crate::runtime::PresentationBinding {
        let contributions = self;
        crate::runtime::PresentationBinding {
            presenter: contributions
                .presenter
                .as_ref()
                .map(|registered| registered.identity.clone()),
            steps: contributions
                .presentation_steps
                .iter()
                .map(|registered| registered.identity.clone())
                .collect(),
        }
    }

    /// Resolve every owed callback before presentation can invoke any of them.
    pub fn validate_tool_presentation_plan(
        &self,
        plan: &crate::runtime::PresentationBinding,
    ) -> Result<(), PluginError> {
        self.resolve_tool_presentation_plan(plan).map(drop)
    }

    fn resolve_tool_presentation_plan(
        &self,
        plan: &crate::runtime::PresentationBinding,
    ) -> Result<ResolvedPresentationPlan<'_>, PluginError> {
        let recorded = self.recorded();
        let presenter = plan
            .presenter
            .as_ref()
            .map(|callback| recorded.resolve(callback, &self.presenter))
            .transpose()?;
        let steps = recorded.resolve_all(&plan.steps, &self.presentation_steps)?;
        Ok(ResolvedPresentationPlan { presenter, steps })
    }

    /// The presentation boundary (ADR 0099 §6, FIG-3420): folds every
    /// [`ToolPresentationStep`] in the recorded plan's order, starting
    /// from `ModelToolReturn::from_output`.
    ///
    /// A retryable step error aborts the uncommitted derivation. Other optional
    /// step errors continue the fold from the
    /// recorded fallback text return (`ModelToolReturn::text(call_id, tool,
    /// err)`), so one broken step settles a refusal the model can read instead
    /// of losing the whole presentation. Attachment-materialization notices
    /// are then computed under `attachment_acceptance`, the caller's recorded
    /// environment. Last, the folded return is measured against the
    /// retention policy and its text retained if it is too long for history
    /// (FIG-1643). The refs retained through `ctx.artifacts` and the policy
    /// ride the returned [`crate::runtime::effect::ToolPresentation`] into
    /// the journal.
    ///
    /// A retention that failed anywhere in the chain — the presenter's, a
    /// step's, or the boundary's own — fails the presentation with a typed
    /// attachment-store cause: a step that turned the refusal into
    /// text does not make it the call's return.
    pub(crate) async fn present_tool_result(
        &self,
        ctx: ToolResultProjectionContext,
        facts: Arc<crate::plugin::ToolPresentationFacts>,
        plan: &crate::runtime::PresentationBinding,
        attachment_acceptance: &crate::provider::AttachmentCapabilitySnapshot,
    ) -> Result<
        crate::runtime::effect::ToolPresentation,
        crate::runtime::effect::RuntimeEffectControllerError,
    > {
        use lash_sansio::core_support::ModelToolReturnCoreSupport as _;

        let callbacks = self
            .resolve_tool_presentation_plan(plan)
            .map_err(crate::RuntimeEffectControllerError::from)?;
        let mut model_return =
            crate::ModelToolReturn::from_output(ctx.tool_name.clone(), &ctx.output);
        if let Some(presenter) = callbacks.presenter {
            model_return = (presenter.hook)(ToolPresentationInput {
                previous: model_return,
                facts: Arc::clone(&facts),
                context: ctx.clone(),
            })
            .await?;
        }
        for registered in callbacks.steps {
            let input = ToolPresentationInput {
                previous: model_return,
                facts: Arc::clone(&facts),
                context: ctx.clone(),
            };
            model_return = match (registered.hook)(input).await {
                Ok(next) => next,
                Err(err) if err.is_retryable() => {
                    return Err(crate::RuntimeEffectControllerError::from(
                        err.into_turn_failure(crate::RuntimeErrorCode::Plugin),
                    )
                    .retryable_uncommitted_derivation());
                }
                Err(err) => crate::ModelToolReturn::text(ctx.tool_name.clone(), err.to_string()),
            };
        }
        crate::session::tool_execution::surface_attachment_materialization_notices(
            attachment_acceptance,
            &ctx.output,
            &mut model_return,
        );
        let retention = ctx.artifacts.retention_policy();
        crate::runtime::effect::retain_oversized_return(
            &mut model_return,
            &ctx.call_id,
            ctx.artifacts.as_ref(),
            retention,
        )
        .await?;
        if let Some(failure) = ctx.artifacts.retention_failure() {
            return Err(failure);
        }
        Ok(crate::runtime::effect::ToolPresentation {
            model_return,
            artifacts: ctx.artifacts.retained(),
            retention,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::prompt::{PromptInput, PromptSectionSpec, SectionText};
    use crate::prompt_sections::{PromptPlacement, PromptSectionKey};
    use std::collections::BTreeSet;

    const KEYED_TOOL: &str = "tool:keyed";

    /// REGISTRATION-IS-INVENTORY: a turn callback is part of installed code.
    #[test]
    fn registration_is_inventory() {
        use super::super::prompt::{
            OfferedTools, PromptFamilySection, PromptSectionFamilySpec, PromptSectionSource,
            PromptWrapSpec, PromptWrapTarget,
        };
        use crate::prompt_sections::PromptWrapKey;
        #[derive(Clone)]
        struct Owner(Arc<std::sync::atomic::AtomicUsize>);
        impl SessionPlugin for Owner {
            fn id(&self) -> &'static str {
                "test_protocol"
            }
        }
        #[async_trait::async_trait]
        impl CodeExecutorPlugin for Owner {
            async fn execute_code(
                &self,
                _: crate::RuntimeExecutionContext<'_>,
                _: crate::ExecRequest,
            ) -> Result<crate::ExecResponse, crate::SessionError> {
                Err(crate::SessionError::Protocol(
                    "inventory fixture executes no code".into(),
                ))
            }
            async fn frame_switch_carries(
                &self,
                _: ProtocolSessionContext<'_>,
                _: &crate::FrameNodeId,
                _: &[crate::SessionAppendNode],
            ) -> Result<Vec<crate::ArtifactName>, crate::SessionError> {
                Ok(Vec::new())
            }
        }
        impl AssistantProseProjectorPlugin for Owner {
            fn project_assistant_prose(&self, text: &str) -> String {
                text.to_owned()
            }
        }
        #[async_trait::async_trait]
        impl ContextPressureHook for Owner {
            fn id(&self) -> &'static str {
                "pressure"
            }
            async fn decide(
                &self,
                _: &ContextPressureContext<'_>,
            ) -> Result<ContextPressureDecision, ContextError> {
                Ok(ContextPressureDecision::Continue)
            }
        }
        #[async_trait::async_trait]
        impl ContextCompactor for Owner {
            fn id(&self) -> &'static str {
                "compact"
            }
            async fn compact(
                &self,
                _: &CompactionContext<'_>,
            ) -> Result<Option<ContextCompaction>, ContextError> {
                Ok(None)
            }
        }
        impl AttachmentOmissionPolicy for Owner {
            fn id(&self) -> &'static str {
                "omit"
            }
            fn omissions(
                &self,
                _: &AttachmentOmissionContext,
                _: &[crate::Message],
            ) -> Result<BTreeSet<HistoryPartId>, PluginError> {
                Ok(BTreeSet::new())
            }
        }
        impl PromptSectionSource for Owner {
            fn sections(&self, _: &OfferedTools) -> Vec<PromptFamilySection> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            }
        }
        struct Config;
        impl ConfigOwner for Config {
            type Create = serde_json::Value;
            type Recorded = serde_json::Value;
            type Refusal = String;
            type RunOptions = NoRunOptions;
            fn create(
                &self,
                input: Option<Self::Create>,
            ) -> Result<Option<Self::Recorded>, Self::Refusal> {
                Ok(input)
            }
            fn validate(
                &self,
                _: &Self::Recorded,
                _: Option<&Self::Recorded>,
                _: &CandidateFacts<'_>,
            ) -> Result<(), Self::Refusal> {
                Ok(())
            }
            fn apply_run_options(
                &self,
                _: &Self::Recorded,
                options: Self::RunOptions,
            ) -> Result<Self::Recorded, Self::Refusal> {
                match options {}
            }
        }
        #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct Command;
        impl ConfigCommand for Command {
            type Owner = Config;
            type Output = ();
            const NAME: &'static str = "set";
        }
        let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let expansions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut reg = PluginRegistrar::<Owner>::new("test_protocol");
        crate::testing::register_inventory_protocol(&mut reg).unwrap();
        reg.turn()
            .before(crate::hook_key!("before"), |_, _| {
                Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
            })
            .unwrap();
        reg.turn()
            .after(crate::hook_key!("after"), |_, _| {
                Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
            })
            .unwrap();
        reg.turn()
            .checkpoint(crate::hook_key!("checkpoint"), |_, _| {
                Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
            })
            .unwrap();
        reg.output()
            .stream(crate::hook_key!("stream"), |_, _| {
                Ok(Arc::new(|ctx| {
                    Box::pin(async move {
                        Ok(AssistantStreamTransform {
                            chunk: ctx.chunk,
                            reasoning_deltas: Vec::new(),
                            events: Vec::new(),
                            abort_stream: false,
                        })
                    })
                }))
            })
            .unwrap();
        reg.output()
            .stream_finished(crate::hook_key!("stream"), |_, _| {
                Ok(Arc::new(|_| Box::pin(async { Ok(None) })))
            })
            .unwrap();
        reg.output()
            .response(
                crate::hook_key!("response"),
                Some(crate::hook_key!("stream")),
                |_, _| {
                    Ok(Arc::new(|ctx| {
                        Box::pin(async move {
                            Ok(AssistantResponseTransform {
                                response: ctx.response,
                                events: Vec::new(),
                            })
                        })
                    }))
                },
            )
            .unwrap();
        reg.output()
            .assistant_prose_projector(|owner, _| Ok(Arc::new(owner.clone())))
            .unwrap();
        reg.execution()
            .code_executor(|owner, _| Ok(Arc::new(owner.clone())))
            .unwrap();
        reg.context()
            .pressure(crate::hook_key!("pressure"), 3, |owner, _| {
                Ok(Arc::new(owner.clone()))
            })
            .unwrap();
        reg.context()
            .compact(crate::hook_key!("compact"), 2, |owner, _| {
                Ok(Arc::new(owner.clone()))
            })
            .unwrap();
        reg.context()
            .attachment_omissions(crate::hook_key!("omit"), 1, |owner, _| {
                Ok(Arc::new(owner.clone()))
            })
            .unwrap();
        reg.session()
            .on_event(crate::hook_key!("event"), |_, _| {
                Ok(Arc::new(|_| Box::pin(async { Ok(()) })))
            })
            .unwrap();
        let provider_handle = reg
            .tools()
            .provider(crate::tool_provider_key!("tools"), |_, _| {
                Ok(Arc::new(crate::testing::EmptyToolProvider))
            })
            .unwrap();
        assert!(
            matches!(
                reg.tools()
                    .provider(crate::tool_provider_key!("tools"), |_, _| Ok(Arc::new(
                        crate::testing::EmptyToolProvider
                    ))),
                Err(PluginError::Registration(_))
            ),
            "declared provider addresses are unique"
        );
        struct NoReads;
        #[async_trait::async_trait]
        impl crate::ToolReads for NoReads {
            async fn read(
                &self,
                _: crate::ReadCall<'_>,
            ) -> Result<serde_json::Value, crate::ReadError> {
                Ok(serde_json::Value::Null)
            }
        }
        reg.tools()
            .provider_with_reads(crate::tool_provider_key!("reads"), |_, _| {
                Ok(crate::ToolProviderRegistration::new(Arc::new(
                    crate::testing::EmptyToolProvider,
                ))
                .with_reads(Arc::new(NoReads)))
            })
            .unwrap();
        reg.tool_catalog()
            .contribute(crate::hook_key!("catalog"), |_, _| {
                Ok(Arc::new(|_| Ok(Default::default())))
            })
            .unwrap();
        reg.tool_calls()
            .transform_args(crate::hook_key!("args"), |_, _| {
                Ok(Arc::new(|input| Box::pin(async move { Ok(input.current) })))
            })
            .unwrap();
        reg.tool_calls()
            .check_args(crate::hook_key!("args"), |_, _| {
                Ok(Arc::new(|_| {
                    Box::pin(async { Ok(BeforeToolDecision::Allow) })
                }))
            })
            .unwrap();
        reg.tool_calls()
            .transform_result(crate::hook_key!("result"), |_, _| {
                Ok(Arc::new(|input| Box::pin(async move { Ok(input.current) })))
            })
            .unwrap();
        reg.tool_calls()
            .check_result(crate::hook_key!("result"), |_, _| {
                Ok(Arc::new(|_| {
                    Box::pin(async { Ok(AfterToolContributions::default()) })
                }))
            })
            .unwrap();
        reg.tool_results()
            .presenter(|_, _| {
                Ok(Arc::new(|input| {
                    Box::pin(async move { Ok(input.previous) })
                }))
            })
            .unwrap();
        reg.tool_results()
            .presentation_step(crate::hook_key!("step"), |_, _| {
                Ok(Arc::new(|input| {
                    Box::pin(async move { Ok(input.previous) })
                }))
            })
            .unwrap();
        reg.operations()
            .query(
                PluginOperationSpec {
                    name: "query".into(),
                    description: "fixture".into(),
                    session_param: SessionParam::Optional,
                    input_schema: serde_json::json!({}),
                    output_schema: serde_json::json!({}),
                    error_type: "fixture".into(),
                    error_version: FormatVersion::ONE,
                    error_schema: serde_json::json!({}),
                },
                |_, _| Ok(Arc::new(|_, value| Box::pin(async { Ok(value) }))),
            )
            .unwrap();
        let section = PromptSectionKey::new("section").unwrap();
        reg.prompt()
            .section(
                PromptSectionSpec::new(section.clone(), PromptPlacement::InitialInstructions),
                |_, _| {
                    Ok(Arc::new(|_: &PromptInput<'_>| {
                        Ok::<_, super::super::prompt::PromptRenderError>(SectionText::text(
                            "section",
                        ))
                    }))
                },
            )
            .unwrap();
        reg.prompt()
            .family(
                PromptSectionFamilySpec::new(
                    PromptSectionKey::new("family").unwrap(),
                    PromptPlacement::CurrentContext,
                ),
                |owner, _| Ok(Arc::new(owner.clone())),
            )
            .unwrap();
        reg.prompt()
            .wrap(
                PromptWrapSpec::new(
                    PromptWrapKey::new("wrap").unwrap(),
                    super::super::prompt::PromptSectionId::new("test_protocol", section),
                ),
                |_, _| {
                    Ok(Arc::new(
                        |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, text: SectionText| {
                            Ok::<_, super::super::prompt::PromptRenderError>(text)
                        },
                    ))
                },
            )
            .unwrap();
        reg.state_reducer("reduce", |_, _| {
            Ok(Arc::new(|reduction| Ok(reduction.current.cloned())))
        })
        .unwrap();
        reg.extension("owner-extension", |_, _| Ok(serde_json::Value::Null))
            .unwrap();
        let count = Arc::clone(&constructions);
        let family = Arc::clone(&expansions);
        let mut recipe = PluginRecipe::new("test_protocol", reg, move |_, _| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::new(Owner(Arc::clone(&family))))
        });
        let mut config = ConfigRegistrar::new("test_protocol");
        config.owner(Config).unwrap();
        config
            .command::<Command>(|recorded, _| {
                Ok(OwnerChange {
                    recorded: recorded.clone(),
                    output: (),
                })
            })
            .unwrap();
        recipe = recipe
            .with_configuration(config)
            .with_factory_extension(PluginExtensionContribution::from_value(
                "factory-extension",
                serde_json::Value::Null,
            ))
            .with_process_engine("testing-fixture", |_| {
                Ok(crate::ProcessEngineRegistration::accepting(Arc::new(
                    crate::testing::FixtureProcessEngine,
                )))
            });
        let factory = PluginRecipeFactory::new(PluginDeclaration::initial("test_protocol"), recipe);
        let host = PluginHost::new(
            vec![Arc::new(factory)],
            crate::ExecutionBudgets::recommended(),
            crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
        );
        let declared = host.installed_inventory().unwrap();
        assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(expansions.load(std::sync::atomic::Ordering::SeqCst), 0);
        let kinds = declared
            .entries
            .iter()
            .map(|entry| entry.kind.clone())
            .collect::<BTreeSet<_>>();
        for slot in CallbackSlot::ALL {
            assert!(
                kinds.contains(&ContributionKind::Callback(*slot)),
                "{slot:?} missing from inventory"
            );
        }
        for kind in [
            ContributionKind::PromptSection,
            ContributionKind::PromptFamily,
            ContributionKind::PromptWrapper,
            ContributionKind::StateReducer,
            ContributionKind::ConfigNamespace,
            ContributionKind::ConfigCommand,
            ContributionKind::Extension,
            ContributionKind::ProcessEngine,
        ] {
            assert!(kinds.contains(&kind), "{kind:?} missing from inventory");
        }
        let session = host
            .build_session(PluginSessionRequest::creation(
                "test_protocol",
                SessionAuthorityContext::fixture(),
            ))
            .unwrap();
        assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            expansions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "family members are not expanded to describe an installation"
        );
        assert_eq!(
            declared.entries.len(),
            session.installed_inventory().unwrap().entries.len()
        );
        assert!(
            session
                .installed_inventory()
                .unwrap()
                .callbacks()
                .any(|key| *key == provider_handle.callback_identity())
        );
        assert!(session.tool_registry().serves(&provider_handle));
        let before = &session.capabilities().contributions.before_turn_hooks[0];
        assert!(
            session
                .installed_inventory()
                .unwrap()
                .callbacks()
                .any(|key| key == &before.identity)
        );
        assert_eq!(before.identity.key, "before_turn:before");
        assert_eq!(
            session.session_extensions().payloads("owner-extension"),
            &[serde_json::Value::Null]
        );
        assert_eq!(
            session.extensions().payloads("factory-extension"),
            &[serde_json::Value::Null]
        );
    }

    /// PORT-PREFLIGHT-PRECEDES-BIND: missing ports construct no owners.
    #[test]
    fn port_preflight_precedes_bind() {
        let binds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&binds);
        let factory = PluginRecipeFactory::new(
            PluginDeclaration::initial("test_protocol"),
            PluginRecipe::empty("test_protocol", move |_, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        );
        let result = PluginHost::new(
            vec![Arc::new(factory)],
            crate::ExecutionBudgets::recommended(),
            crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
        )
        .build_session(PluginSessionRequest::creation(
            "missing-port",
            SessionAuthorityContext::fixture(),
        ));
        assert!(matches!(
            result,
            Err(PluginError::Preflight(PreflightIssue::MissingPort {
                port: RequiredPort::ProtocolSession
            }))
        ));
        assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
        let counter = Arc::clone(&binds);
        let mut recipe = PluginRecipe::empty("missing-service", move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        recipe
            .requirements
            .services
            .insert(ServiceKind::CurrentSessionSnapshot);
        let factory =
            PluginRecipeFactory::new(PluginDeclaration::initial("missing-service"), recipe);
        let result = crate::testing::test_plugin_host(vec![Arc::new(factory)]).build_session(
            PluginSessionRequest::creation("missing-service", SessionAuthorityContext::fixture()),
        );
        assert!(
            matches!(result, Err(PluginError::Preflight(PreflightIssue::MissingService { owner, service: ServiceKind::CurrentSessionSnapshot })) if owner == "missing-service")
        );
        assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
        struct Duplicate;
        impl SessionPlugin for Duplicate {
            fn id(&self) -> &'static str {
                "duplicate-port"
            }
        }
        let mut reg = PluginRegistrar::<Duplicate>::new("duplicate-port");
        reg.protocol()
            .session(|_, _| {
                Err(PluginError::Invoke(
                    "a refused binder cannot execute".into(),
                ))
            })
            .unwrap();
        let counter = Arc::clone(&binds);
        let recipe = PluginRecipe::new("duplicate-port", reg, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::new(Duplicate))
        });
        let factory =
            PluginRecipeFactory::new(PluginDeclaration::initial("duplicate-port"), recipe);
        let result = crate::testing::test_plugin_host(vec![Arc::new(factory)]).build_session(
            PluginSessionRequest::creation("duplicate-port", SessionAuthorityContext::fixture()),
        );
        assert!(matches!(
            result,
            Err(PluginError::Preflight(PreflightIssue::DuplicatePort {
                port: RequiredPort::ProtocolSession,
                ..
            }))
        ));
        assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A provider of one tool, `keyed`, that answers with its `label`. A
    /// provider that does not `advertise` still resolves the tool by id, as
    /// a grant-only provider does.
    struct Keyed {
        label: &'static str,
        advertises: bool,
    }

    impl Keyed {
        fn definition() -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                KEYED_TOOL,
                "keyed",
                "keyed probe",
                crate::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )
            .expect("valid declared tool schemas")
            .with_execution(std::time::Duration::from_secs(30))
        }
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for Keyed {
        fn tool_manifests(
            &self,
            _config: &crate::OwnerConfigView,
        ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
            Ok({
                if self.advertises {
                    vec![Self::definition().manifest()]
                } else {
                    Vec::new()
                }
            })
        }

        fn resolve_manifest_by_id(
            &self,
            id: &crate::ToolId,
            _config: &crate::OwnerConfigView,
        ) -> Option<crate::ToolManifest> {
            (id.as_str() == KEYED_TOOL).then(|| Self::definition().manifest())
        }

        fn resolve_contract(
            &self,
            name: &str,
            _config: &crate::OwnerConfigView,
        ) -> Option<Arc<crate::ToolContract>> {
            (name == "keyed").then(|| Arc::new(Self::definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!(self.label)).into()
        }
    }

    /// The advertising provider under `served`, then the id-only one under
    /// `hidden`, or the reverse.
    fn keyed_providers(served_first: bool) -> Vec<(ToolProviderKey, Arc<dyn ToolProvider>)> {
        let served = (
            crate::tool_provider_key!("served"),
            Arc::new(Keyed {
                label: "served",
                advertises: true,
            }) as Arc<dyn ToolProvider>,
        );
        let hidden = (
            crate::tool_provider_key!("hidden"),
            Arc::new(Keyed {
                label: "hidden",
                advertises: false,
            }) as Arc<dyn ToolProvider>,
        );
        if served_first {
            vec![served, hidden]
        } else {
            vec![hidden, served]
        }
    }

    /// What a call of `keyed` records as its binding under `composition`,
    /// matched on the surface `registry` advertises, and what dispatch
    /// through that surface executes.
    async fn bound_and_executed(
        composition: &ExecutionComposition,
        registry: Arc<crate::ToolRegistry>,
        config: crate::OwnerConfigView,
    ) -> (crate::tool_run::AdmittedBinding, serde_json::Value) {
        let entries = registry
            .advertise(&config)
            .expect("the providers advertise");
        let tools = crate::SurfaceTools::of_entries(entries, registry);
        let tool = crate::ToolId::from(KEYED_TOOL);
        let provider = &tools
            .entry(&tool)
            .expect("the surface offers the tool")
            .provider;
        let binding = composition
            .tool_run_binding(&tool, Some(provider))
            .expect("the surface binds the tool");
        let crate::ToolAttemptOutcome::Done { result, .. } =
            crate::testing::run_tool(&tools, "keyed", &serde_json::json!({})).await
        else {
            panic!("the call completes inline");
        };
        let executed = crate::ToolOutcome::from_output(result.into_output()).value_for_projection();
        (binding, executed)
    }

    fn session_of(factories: Vec<Arc<dyn PluginFactory>>) -> Arc<PluginSession> {
        crate::testing::test_plugin_host(factories)
            .build_session(PluginSessionRequest::creation(
                "keyed-providers",
                SessionAuthorityContext::fixture(),
            ))
            .expect("materialized session")
    }

    fn keyed_plugin(
        id: &'static str,
        providers: Vec<(ToolProviderKey, Arc<dyn ToolProvider>)>,
    ) -> Arc<dyn PluginFactory> {
        let spec = providers
            .into_iter()
            .fold(PluginSpec::new(), |spec, (key, provider)| {
                spec.with_tool_provider(key, provider)
            });
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(id),
            spec,
        ))
    }

    /// KEYED-PROVIDERS (FIG-6073): a call binds to and dispatches through one
    /// declared provider identity, so reordering two provider registrations
    /// (in one plugin, as builder tool plugins, or in one process component)
    /// changes nothing recorded or executed, even when another provider
    /// resolves the same tool id.
    #[tokio::test]
    async fn reordering_provider_registrations_changes_nothing_recorded_or_executed() {
        let served = PluginCallbackIdentity::tool_provider("keyed", "served");
        for served_first in [true, false] {
            let session = session_of(vec![keyed_plugin("keyed", keyed_providers(served_first))]);
            let (binding, executed) = bound_and_executed(
                &session.composition(),
                session.tool_registry(),
                crate::OwnerConfigView::default(),
            )
            .await;
            assert_eq!(binding.executable, served, "served first: {served_first}");
            assert_eq!(binding.preparation, served, "served first: {served_first}");
            assert_eq!(executed, serde_json::json!("served"));
        }

        for served_first in [true, false] {
            let plugins = keyed_providers(served_first)
                .into_iter()
                .map(|(key, provider)| keyed_plugin(key.as_str(), vec![(key, provider)]))
                .collect();
            let session = session_of(plugins);
            let (binding, executed) = bound_and_executed(
                &session.composition(),
                session.tool_registry(),
                crate::OwnerConfigView::default(),
            )
            .await;
            assert_eq!(
                binding.executable,
                PluginCallbackIdentity::tool_provider("served", "served"),
                "served first: {served_first}"
            );
            assert_eq!(executed, serde_json::json!("served"));
        }

        let key = crate::ProcessConfigKey::<()>::new("keyed-component", crate::FormatVersion::ONE);
        for served_first in [true, false] {
            let mut components = ProcessComponentRegistry::new();
            components
                .register(
                    Arc::new(
                        StaticPluginFactory::new(
                            PluginDeclaration::initial("keyed-component"),
                            keyed_providers(served_first)
                                .into_iter()
                                .fold(PluginSpec::new(), |spec, (key, provider)| {
                                    spec.with_tool_provider(key, provider)
                                }),
                        )
                        .as_component(key.format())
                        .expect("tool providers are a process component"),
                    ),
                    key,
                )
                .expect("the component registers");
            let composition = components
                .compose(
                    &crate::testing::component_env("keyed-component", key.format()),
                    None,
                )
                .expect("the components compose");
            let (binding, executed) = bound_and_executed(
                &composition.composition,
                Arc::clone(&composition.registry),
                crate::OwnerConfigView::components(Arc::new(crate::testing::component_env(
                    "keyed-component",
                    key.format(),
                ))),
            )
            .await;
            assert_eq!(
                binding.executable,
                PluginCallbackIdentity::tool_provider("keyed-component", "served"),
                "served first: {served_first}"
            );
            assert_eq!(executed, serde_json::json!("served"));
        }
    }
}
