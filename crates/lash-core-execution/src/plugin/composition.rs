//! Immutable execution contributions shared by session and process dispatch.
use super::prompt::{PromptCatalog, PromptRegistry};
use super::*;
use lash_core_store::tool_run::{AttributedVerdict, CheckRecord};
use std::sync::Arc;

/// Every executable slot, with registration identities scoped by its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContributionKind {
    ToolProvider,
    ToolCatalogContributor,
    ToolArgsTransform,
    ToolArgsCheck,
    ToolResultTransform,
    ToolResultCheck,
    Presenter,
    PresentationStep,
    PromptSection,
}

/// Built once per materialization, in owner and registration order.
pub struct ExecutionComposition {
    tool_providers: Vec<RegisteredHook<Arc<dyn ToolProvider>>>,
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

    pub fn contributions(
        &self,
    ) -> impl Iterator<Item = (ContributionKind, &PluginCallbackIdentity)> {
        use ContributionKind::*;
        self.tool_providers
            .iter()
            .map(|h| (ToolProvider, &h.identity))
            .chain(
                self.tool_catalog_contributors
                    .iter()
                    .map(|h| (ToolCatalogContributor, &h.identity)),
            )
            .chain(
                self.tool_args_transforms
                    .iter()
                    .map(|h| (ToolArgsTransform, &h.identity)),
            )
            .chain(
                self.tool_args_checks
                    .iter()
                    .map(|h| (ToolArgsCheck, &h.identity)),
            )
            .chain(
                self.tool_result_transforms
                    .iter()
                    .map(|h| (ToolResultTransform, &h.identity)),
            )
            .chain(
                self.tool_result_checks
                    .iter()
                    .map(|h| (ToolResultCheck, &h.identity)),
            )
            .chain(self.presenter.iter().map(|h| (Presenter, &h.identity)))
            .chain(
                self.presentation_steps
                    .iter()
                    .map(|h| (PresentationStep, &h.identity)),
            )
            .chain(
                self.prompt
                    .identities()
                    .map(|identity| (PromptSection, identity)),
            )
    }

    pub(super) fn tool_catalog_contributors(&self) -> &[RegisteredHook<ToolCatalogContributor>] {
        &self.tool_catalog_contributors
    }

    fn unavailable_callback(&self, callback: &PluginCallbackIdentity) -> PluginError {
        PluginError::Runtime(
            RecordedCallbackUnavailable {
                recorded: callback.clone(),
                available: self
                    .contributions()
                    .map(|(_, identity)| identity.clone())
                    .collect(),
            }
            .into_runtime_error(),
        )
    }

    /// The one provider identity a call of `tool` binds to and dispatches
    /// through, before admission: the provider its grant names, else the
    /// provider the catalog binds the tool to. It must be registered here.
    pub fn tool_provider(
        &self,
        tool: &crate::ToolId,
        granted: Option<&PluginCallbackIdentity>,
    ) -> Result<PluginCallbackIdentity, PluginError> {
        let provider = granted
            .cloned()
            .or_else(|| self.tool_registry.execution_provider(tool))
            .ok_or_else(|| {
                PluginError::Registration(format!("tool `{tool}` has no registered provider"))
            })?;
        if !self
            .tool_providers
            .iter()
            .any(|registered| registered.identity == provider)
        {
            return Err(PluginError::Registration(format!(
                "tool `{tool}` names provider `{}:{}`, which is not registered",
                provider.owner, provider.key
            )));
        }
        Ok(provider)
    }

    pub(crate) fn tool_run_binding(
        &self,
        tool: &crate::ToolId,
        granted: Option<&PluginCallbackIdentity>,
    ) -> Result<crate::tool_run::AdmittedBinding, PluginError> {
        let executable = self.tool_provider(tool, granted)?;
        Ok(crate::tool_run::AdmittedBinding {
            preparation: executable.clone(),
            executable,
            presentation: self.tool_presentation_plan(),
        })
    }

    /// Bind recorded provider identities to this worker's capabilities.
    pub fn resolve_context_tool_bindings(
        &self,
        bindings: &[crate::plugin::PluginCallbackIdentity],
    ) -> Result<Vec<Arc<dyn crate::ToolProvider>>, PluginError> {
        bindings
            .iter()
            .map(|binding| {
                self.tool_providers
                    .iter()
                    .find(|registered| &registered.identity == binding)
                    .map(|registered| Arc::clone(&registered.hook))
                    .ok_or_else(|| {
                        PluginError::Session(format!(
                            "recorded context tool binding {}:{} is unavailable",
                            binding.owner, binding.key,
                        ))
                    })
            })
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
            current = (registered.hook)(ToolArgsTransformInput {
                context: context.clone(),
                original: Arc::clone(&original),
                current,
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
            let verdict = (registered.hook)(ToolArgsCheckInput {
                context: context.clone(),
                original_args: Arc::clone(original_args),
                prepared: prepared.clone(),
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
            current = (registered.hook)(ToolResultTransformInput {
                context: context.clone(),
                occurrence,
                prepared: prepared.clone(),
                original: Arc::clone(original),
                current,
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
            let reply = (registered.hook)(ToolResultCheckInput {
                context: context.clone(),
                occurrence,
                prepared: prepared.clone(),
                original: Arc::clone(original),
                final_result: Arc::clone(final_result),
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
        let contributions = self;
        let presenter = plan
            .presenter
            .as_ref()
            .map(|callback| {
                contributions
                    .presenter
                    .as_ref()
                    .filter(|registered| &registered.identity == callback)
                    .ok_or_else(|| self.unavailable_callback(callback))
            })
            .transpose()?;
        let steps = plan
            .steps
            .iter()
            .map(|callback| {
                contributions
                    .presentation_steps
                    .iter()
                    .find(|registered| &registered.identity == callback)
                    .ok_or_else(|| self.unavailable_callback(callback))
            })
            .collect::<Result<Vec<_>, _>>()?;
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
    use crate::prompt_sections::{PromptPlacement, PromptPurpose, PromptSectionKey};
    use std::collections::BTreeSet;

    struct EveryContribution;

    impl PluginDefinition for EveryContribution {
        fn declaration() -> PluginDeclaration {
            PluginDeclaration::initial("composition-identities")
        }
    }

    impl PluginFactory for EveryContribution {
        fn id(&self) -> &'static str {
            "composition-identities"
        }

        fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(EveryContribution))
        }
    }

    impl SessionPlugin for EveryContribution {
        fn id(&self) -> &'static str {
            "composition-identities"
        }

        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.tools().provider(
                crate::hook_key!("empty"),
                Arc::new(crate::testing::EmptyToolProvider),
            )?;
            reg.tool_catalog().contribute(
                crate::hook_key!("catalog"),
                Arc::new(|_| Ok(Default::default())),
            )?;
            reg.tool_calls().transform_args(
                crate::hook_key!("args"),
                Arc::new(|input| Box::pin(async move { Ok(input.current) })),
            )?;
            reg.tool_calls().check_args(
                crate::hook_key!("args"),
                Arc::new(|_| Box::pin(async { Ok(BeforeToolDecision::Allow) })),
            )?;
            reg.tool_calls().transform_result(
                crate::hook_key!("result"),
                Arc::new(|input| Box::pin(async move { Ok(input.current) })),
            )?;
            reg.tool_calls().check_result(
                crate::hook_key!("result"),
                Arc::new(|_| Box::pin(async { Ok(AfterToolContributions::default()) })),
            )?;
            reg.tool_results().presenter(Arc::new(|input| {
                Box::pin(async move { Ok(input.previous) })
            }))?;
            reg.tool_results().presentation_step(
                crate::hook_key!("presentation"),
                Arc::new(|input| Box::pin(async move { Ok(input.previous) })),
            )?;
            reg.prompt().section(
                PromptSectionSpec::new(
                    PromptSectionKey::new("direct").expect("section key"),
                    PromptPlacement::InitialInstructions,
                )
                .purposes([PromptPurpose::Direct {
                    name: "probe".into(),
                }]),
                Arc::new(|_: &PromptInput<'_>| Ok(SectionText::text("probe"))),
            )
        }
    }

    /// COMPOSITION-IDENTITIES (D3): every executable contribution exposes
    /// its registering owner and its unique callback key.
    #[test]
    fn composition_identities() {
        let session = crate::testing::test_plugin_host(vec![Arc::new(EveryContribution)])
            .build_session(PluginSessionRequest::creation(
                "composition-identities-session",
                SessionAuthorityContext::ambient_fixture(),
            ))
            .expect("materialized session");
        let composition = session.composition();
        let contributions = composition
            .contributions()
            .filter(|(_, identity)| identity.owner == "composition-identities")
            .collect::<Vec<_>>();
        let expected = BTreeSet::from([
            ContributionKind::ToolProvider,
            ContributionKind::ToolCatalogContributor,
            ContributionKind::ToolArgsTransform,
            ContributionKind::ToolArgsCheck,
            ContributionKind::ToolResultTransform,
            ContributionKind::ToolResultCheck,
            ContributionKind::Presenter,
            ContributionKind::PresentationStep,
            ContributionKind::PromptSection,
        ]);
        assert_eq!(contributions.len(), expected.len());
        assert_eq!(
            contributions
                .iter()
                .map(|(kind, _)| *kind)
                .collect::<BTreeSet<_>>(),
            expected
        );
        let mut keys = BTreeSet::new();
        for (_, identity) in contributions {
            assert_eq!(identity.owner, "composition-identities");
            assert!(
                keys.insert(identity.key.clone()),
                "duplicate contribution key"
            );
        }
        let owners = composition
            .prompt
            .owners()
            .filter(|(owner, _)| owner == "composition-identities")
            .collect::<Vec<_>>();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].1.key.as_str(), "direct");
        assert!(Arc::ptr_eq(&composition, &session.composition()));
    }

    const KEYED_TOOL: &str = "tool:keyed";

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
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            if self.advertises {
                vec![Self::definition().manifest()]
            } else {
                Vec::new()
            }
        }

        fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
            (id.as_str() == KEYED_TOOL).then(|| Self::definition().manifest())
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "keyed").then(|| Arc::new(Self::definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!(self.label)).into()
        }
    }

    /// The advertising provider under `served`, then the id-only one under
    /// `hidden`, or the reverse.
    fn keyed_providers(served_first: bool) -> Vec<(HookKey, Arc<dyn ToolProvider>)> {
        let served = (
            crate::hook_key!("served"),
            Arc::new(Keyed {
                label: "served",
                advertises: true,
            }) as Arc<dyn ToolProvider>,
        );
        let hidden = (
            crate::hook_key!("hidden"),
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
    /// and what dispatch through `tools` executes.
    async fn bound_and_executed(
        composition: &ExecutionComposition,
        tools: &dyn ToolProvider,
    ) -> (crate::tool_run::AdmittedBinding, serde_json::Value) {
        let binding = composition
            .tool_run_binding(&crate::ToolId::from(KEYED_TOOL), None)
            .expect("the catalog binds the tool");
        let crate::ToolAttemptOutcome::Done { result, .. } =
            crate::testing::run_tool(tools, "keyed", &serde_json::json!({})).await
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
                SessionAuthorityContext::ambient_fixture(),
            ))
            .expect("materialized session")
    }

    fn keyed_plugin(
        id: &'static str,
        providers: Vec<(HookKey, Arc<dyn ToolProvider>)>,
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
            let (binding, executed) =
                bound_and_executed(&session.composition(), session.tools().as_ref()).await;
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
            let (binding, executed) =
                bound_and_executed(&session.composition(), session.tools().as_ref()).await;
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
                            PluginSpec {
                                tool_providers: keyed_providers(served_first),
                                ..PluginSpec::default()
                            },
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
            let (binding, executed) =
                bound_and_executed(&composition.composition, composition.registry.as_ref()).await;
            assert_eq!(
                binding.executable,
                PluginCallbackIdentity::tool_provider("keyed-component", "served"),
                "served first: {served_first}"
            );
            assert_eq!(executed, serde_json::json!("served"));
        }
    }

    /// A duplicate provider key is refused at registration, in a session
    /// plugin and in a process component alike, and so is a key that does
    /// not begin with a letter.
    #[test]
    fn a_duplicate_or_positional_provider_key_is_refused_at_registration() {
        let twice = || {
            vec![
                (
                    crate::hook_key!("search"),
                    Arc::new(crate::testing::EmptyToolProvider) as Arc<dyn ToolProvider>,
                ),
                (
                    crate::hook_key!("search"),
                    Arc::new(crate::testing::EmptyToolProvider) as Arc<dyn ToolProvider>,
                ),
            ]
        };
        let refused = crate::testing::test_plugin_host(vec![keyed_plugin("twice", twice())])
            .build_session(PluginSessionRequest::creation(
                "duplicate-key",
                SessionAuthorityContext::ambient_fixture(),
            ))
            .err()
            .expect("a duplicate key refuses the session's registration");
        assert!(
            matches!(&refused, PluginError::Registration(message)
                if message.contains("duplicate hook key `tool_provider:search`")),
            "{refused}"
        );

        let positional = crate::testing::test_plugin_host(vec![keyed_plugin(
            "positional",
            vec![(
                crate::hook_key!("0"),
                Arc::new(crate::testing::EmptyToolProvider) as Arc<dyn ToolProvider>,
            )],
        )])
        .build_session(PluginSessionRequest::creation(
            "positional-key",
            SessionAuthorityContext::ambient_fixture(),
        ))
        .err()
        .expect("a positional key refuses the session's registration");
        assert!(
            matches!(&positional, PluginError::Registration(message)
                if message.contains("must begin with a letter")),
            "{positional}"
        );

        let key = crate::ProcessConfigKey::<()>::new("twice-component", crate::FormatVersion::ONE);
        let mut components = ProcessComponentRegistry::new();
        components
            .register(
                Arc::new(
                    StaticPluginFactory::new(
                        PluginDeclaration::initial("twice-component"),
                        PluginSpec {
                            tool_providers: twice(),
                            ..PluginSpec::default()
                        },
                    )
                    .as_component(key.format())
                    .expect("tool providers are a process component"),
                ),
                key,
            )
            .expect("the component registers");
        let refused = components
            .compose(
                &crate::testing::component_env("twice-component", key.format()),
                None,
            )
            .err()
            .expect("a duplicate key refuses the component's registration");
        assert!(
            refused
                .to_string()
                .contains("duplicate hook key `tool_provider:search`"),
            "{refused}"
        );
    }
}
