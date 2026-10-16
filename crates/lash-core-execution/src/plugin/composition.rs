//! Immutable execution contributions shared by session and process dispatch.
use super::prompt::{PromptCatalog, PromptRegistry};
use super::*;

use lash_core_store::tool_run::{AttributedVerdict, CheckRecord};
use std::sync::Arc;

/// Built once per materialization, in owner and registration order.
pub struct ExecutionComposition {
    inventory: Option<Arc<InstalledInventory>>,
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
    cell_failure_feedback: Vec<RegisteredHook<CellFailureFeedbackHook>>,
    tool_registry: Arc<crate::ToolRegistry>,
}

impl ExecutionComposition {
    pub(super) fn new(
        contributions: &mut PluginContributions,
        tool_registry: Arc<crate::ToolRegistry>,
    ) -> Self {
        Self {
            inventory: contributions.inventory.clone(),
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
            cell_failure_feedback: Vec::new(),
        }
    }

    /// Session-only contributions: process assembly leaves this slot unconsumed.
    pub(super) fn with_cell_failure_feedback(
        mut self,
        contributions: &mut PluginContributions,
    ) -> Self {
        self.cell_failure_feedback = std::mem::take(&mut contributions.cell_failure_feedback);
        self
    }

    pub(crate) fn cell_failure_repairs(
        &self,
        diagnostic: &CellFailureDiagnostic<'_>,
    ) -> Vec<crate::CellFailureRepair> {
        self.cell_failure_feedback
            .iter()
            .map(|entry| crate::CellFailureRepair {
                plugin_id: entry.identity.owner.clone(),
                callback: entry.identity.key.clone(),
                repairs: (entry.hook)(diagnostic),
            })
            .collect()
    }

    pub fn prompt_catalog(&self) -> PromptCatalog {
        PromptCatalog::new(
            self.prompt.clone(),
            self.inventory
                .as_deref()
                .map(super::prompt::PromptCatalogSnapshot::from_inventory)
                .unwrap_or_default(),
        )
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
                    crate::trace::extension::run_bound(
                        &context.trace,
                        &registered.identity,
                        |trace| {
                            (registered.hook)(ToolArgsTransformInput {
                                context: context.with_trace(trace),
                                original: Arc::clone(&original),
                                current,
                            })
                        },
                    )
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
                    crate::trace::extension::run_bound(
                        &context.trace,
                        &registered.identity,
                        |trace| {
                            (registered.hook)(ToolArgsCheckInput {
                                context: context.with_trace(trace),
                                original_args: Arc::clone(original_args),
                                prepared: prepared.clone(),
                            })
                        },
                    )
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
                    crate::trace::extension::run_bound(
                        &context.trace,
                        &registered.identity,
                        |trace| {
                            (registered.hook)(ToolResultTransformInput {
                                context: context.with_trace(trace),
                                occurrence,
                                prepared: prepared.clone(),
                                original: Arc::clone(original),
                                current,
                            })
                        },
                    )
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
                    crate::trace::extension::run_bound(
                        &context.trace,
                        &registered.identity,
                        |trace| {
                            (registered.hook)(ToolResultCheckInput {
                                context: context.with_trace(trace),
                                occurrence,
                                prepared: prepared.clone(),
                                original: Arc::clone(original),
                                final_result: Arc::clone(final_result),
                            })
                        },
                    )
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
    /// of losing the whole presentation. The folded return is measured against the
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
mod tests;
