//! A turn's model call composes its prompt sections before admission (ADR
//! 0133 §4, §6; FIG-5255). It is the turn's one composition point: the
//! execution-environment sync builds the tool surface and composes nothing.
//!
//! The cut is the turn's committed state at the call: the admitted plan and
//! plugin config of the running run (FIG-4589), the machine's prompt view
//! (the previous round's outcomes included) as the session view and history
//! statistics, the committed usage, the protocol's facts, the surface the
//! iteration's sync installed, and every plugin's namespace as published,
//! frozen under one lock.
//! Everything the turn published since its last commit commits with the
//! call's `model.start`, so the admitted request never shows state that is
//! not durable. The composed text is lowered into the request the machine
//! waits on: `InitialInstructions` as its instructions, `CurrentContext` as
//! one User message after the conversation, outside its history. A resend
//! sends the admitted body and composes nothing.

use std::sync::Arc;

use super::RuntimeTurnDriver;
use super::tool_catalog::SyncFailure;
use crate::plugin::prompt::{
    ComposedPrompt, OfferedTools, ProjectedHistoryStats, PromptCall, PromptCatalog,
    PromptCompositionError, PromptCutParts, PromptModel, PromptRenderPool,
};
use crate::prompt_sections::{PromptPlan, PromptPurpose};
use crate::sansio::ExecutionEnvironmentSyncFailureKind as SyncFailureKind;
use crate::{
    FailureCode, LlmCallError, LlmRequest, RuntimeError, RuntimeErrorCode, TurnFailureCode,
};
use lash_core_execution::core_internal::{compose_prompt, prompt_cut};

impl RuntimeTurnDriver<'static> {
    /// Compose turn call `call`'s prompt over the turn's committed cut and
    /// lower it into `request`: the request and the composition its
    /// admission records. The inner `Err` is the typed settlement of a call
    /// whose prompt did not compose; the outer one a live fault of this
    /// activation, which admits nothing, so its resume composes again. A
    /// session with no sections keeps `request` as it is and composes
    /// nothing.
    pub(super) async fn compose_call(
        &self,
        iteration: usize,
        call: u32,
        messages: crate::MessageSequence,
        request: Arc<LlmRequest>,
        has_current_context_prefix: bool,
    ) -> Result<Result<(Arc<LlmRequest>, Option<ComposedPrompt>), LlmCallError>, RuntimeError> {
        let plugins = Arc::clone(self.session.plugins());
        let catalog = plugins.prompt_catalog();
        if catalog.sections().is_empty() {
            return Ok(Ok((request, None)));
        }
        // The turn's prompt view at this call: the last commit, with the
        // previous round's outcomes and the checkpoint deliveries after it.
        let history = u32::try_from(<[crate::Message]>::len(&messages)).unwrap_or(u32::MAX);
        let view = self.checkpoint_state_view(messages, iteration);
        let protocol_facts = match self.protocol_prompt_facts(&view).await {
            Ok(facts) => facts,
            Err(error) => {
                return match SyncFailure::of_session_error(SyncFailureKind::ProtocolFacts, error) {
                    SyncFailure::Live(fault) => Err(fault),
                    SyncFailure::Recorded(failure) => {
                        Ok(Err(refused(failure.code, failure.message, None)))
                    }
                };
            }
        };
        let offered_catalog = match self.session.installed_tool_catalog() {
            Ok(offered_catalog) => offered_catalog,
            Err(error) => {
                return match SyncFailure::of_plugin_error(SyncFailureKind::ToolSurface, error) {
                    SyncFailure::Live(fault) => Err(fault),
                    SyncFailure::Recorded(failure) => {
                        Ok(Err(refused(failure.code, failure.message, None)))
                    }
                };
            }
        };
        let state = self.turn_pipeline.state();
        let offered = plugins.protocol_driver().prompt_tools(offered_catalog);
        let profile = self.policy.llm_profile_config();
        let cut = prompt_cut(
            PromptCutParts {
                call: PromptCall {
                    session_id: self.session_id.clone(),
                    frame: state.current_frame_node_id.clone(),
                    run: request.scope.turn.as_ref().map(|turn| turn.run.clone()),
                    turn: Some(self.turn_id.clone()),
                    iteration: u32::try_from(iteration).unwrap_or(u32::MAX),
                    call,
                    purpose: PromptPurpose::Turn,
                },
                config: plugins.admitted_plugin_config(),
                session: Some(view),
                offered: offered.clone(),
                model: PromptModel {
                    profile: Some(profile.key().clone()),
                    context_window_tokens: Some(profile.context_window_tokens() as u64),
                    committed_usage: state.last_prompt_usage.clone(),
                },
                history: ProjectedHistoryStats {
                    messages: history,
                    estimated_tokens: u64::from(request.estimated_tokens()),
                },
                namespaces: plugins.committed_namespaces(),
            },
            protocol_facts,
        );
        let plan = &state.authority.prompt_plan;
        let started = std::time::Instant::now();
        let composed = match compose_prompt(
            &catalog,
            plan,
            &PromptPurpose::Turn,
            Arc::new(cut),
            PromptRenderPool::shared(),
        )
        .await
        {
            Ok(composed) => composed,
            // A full shared queue is this process's load, not the call's
            // outcome: the activation ends unadmitted and resumes.
            Err(error @ PromptCompositionError::RenderersBusy { .. }) => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::PromptRenderersBusy,
                    error.to_string(),
                ));
            }
            Err(error) => {
                let failed = FailedComposition {
                    catalog: &catalog,
                    plan,
                    offered: &offered,
                    error: &error,
                    elapsed: started.elapsed(),
                };
                self.trace_prompt_failed(iteration, &failed);
                return Ok(Err(refused(
                    FailureCode::lash(TurnFailureCode::PromptCompositionFailed),
                    error.to_string(),
                    serde_json::to_string(&error).ok(),
                )));
            }
        };
        self.trace_prompt_built(iteration, &composed);
        Ok(Ok((
            lower(request, &composed, has_current_context_prefix),
            Some(composed),
        )))
    }

    /// The protocol's facts for its prompt sections, derived from its
    /// committed execution state under the run's recorded render. The text
    /// they feed commits with the call's admission, so a resend sends it
    /// rather than asking again (FIG-3538).
    async fn protocol_prompt_facts(
        &self,
        history: &crate::SessionReadView,
    ) -> Result<Option<crate::plugin::prompt::ProtocolPromptFacts>, crate::SessionError> {
        let protocol_session = Arc::clone(self.session.plugins().protocol_session());
        let recorded_render = self
            .turn_pipeline
            .state()
            .authority
            .run_view()
            .and_then(|view| view.run.render.as_ref());
        let mut context = crate::plugin::ProtocolSessionContext::new(
            &self.session_id,
            self.session.fleet_format(),
        )
        .with_prompt_history(history);
        if let Some(recorded_render) = recorded_render {
            context = context.with_recorded_render(recorded_render);
        }
        protocol_session.prompt_facts(context).await
    }

    /// The decision evidence of a composition that failed closed: the
    /// resolved plan it ran, the plan's limits, the typed error with its
    /// site and measured bytes, and the time it took.
    fn trace_prompt_failed(&self, protocol_iteration: usize, failed: &FailedComposition<'_>) {
        if !self.trace.is_observed() {
            return;
        }
        let resolved = failed
            .catalog
            .preview(failed.plan, &PromptPurpose::Turn, failed.offered)
            .ok()
            .and_then(|resolved| serde_json::to_value(resolved).ok());
        let limits = serde_json::to_value(failed.plan.limits).unwrap_or_default();
        let error = serde_json::to_value(failed.error).unwrap_or_default();
        let elapsed_ms = u64::try_from(failed.elapsed.as_millis()).unwrap_or(u64::MAX);
        self.trace.observe(|| {
            (
                self.trace_context(protocol_iteration),
                lash_trace::TraceEvent::PromptCompositionFailed {
                    plan: resolved.clone(),
                    limits: limits.clone(),
                    error: error.clone(),
                    elapsed_ms,
                },
            )
        });
    }

    fn trace_prompt_built(
        &self,
        protocol_iteration: usize,
        composed: &crate::plugin::prompt::ComposedPrompt,
    ) {
        if !self.trace.is_observed() {
            return;
        }
        let system_prompt = composed.initial_instructions.as_deref().unwrap_or("");
        let prompt_hash = lash_trace::sha256_hex(system_prompt.as_bytes());
        let prompt_chars = system_prompt.chars().count();
        let components = composed
            .snapshot
            .sections
            .iter()
            .filter_map(|section| match &section.value {
                crate::prompt_sections::RecordedSectionText::Text { text } => {
                    Some(lash_trace::TracePromptComponent {
                        id: section.section.to_string(),
                        kind: match section.placement {
                            crate::prompt_sections::PromptPlacement::InitialInstructions => {
                                "initial_instructions".to_string()
                            }
                            crate::prompt_sections::PromptPlacement::CurrentContext => {
                                "current_context".to_string()
                            }
                            crate::prompt_sections::PromptPlacement::Excluded => {
                                "excluded".to_string()
                            }
                        },
                        hash: text.blob.0.clone(),
                        chars: composed
                            .texts
                            .get(&text.blob)
                            .map(|text| text.chars().count()),
                    })
                }
                crate::prompt_sections::RecordedSectionText::Omitted => None,
            })
            .collect::<Vec<_>>();
        self.trace.observe(|| {
            (
                self.trace_context(protocol_iteration),
                lash_trace::TraceEvent::PromptBuilt {
                    prompt_hash: prompt_hash.clone(),
                    prompt_chars,
                    components: components.clone(),
                },
            )
        });
    }
}

/// What a composition that failed closed ran over and how long it took.
struct FailedComposition<'a> {
    catalog: &'a PromptCatalog,
    plan: &'a PromptPlan,
    offered: &'a OfferedTools,
    error: &'a PromptCompositionError,
    elapsed: std::time::Duration,
}

/// The settlement of a call whose prompt did not compose: never sent, typed
/// by `code`, `raw` holding the attributed error.
pub(super) fn refused(code: FailureCode, message: String, raw: Option<String>) -> LlmCallError {
    LlmCallError {
        message,
        retryable: false,
        kind: crate::ProviderFailureKind::Unknown,
        raw,
        code: Some(code),
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: None,
    }
}

/// `request` with `composed` lowered onto it ([`crate::sansio::place_prompt`]).
/// An empty composition leaves `request` as it is.
pub(in crate::runtime) fn lower(
    request: Arc<LlmRequest>,
    composed: &ComposedPrompt,
    has_current_context_prefix: bool,
) -> Arc<LlmRequest> {
    if composed.initial_instructions.is_none() && composed.current_context.is_none() {
        return request;
    }
    let mut request = LlmRequest::clone(&request);
    crate::sansio::place_prompt(
        &mut request,
        composed.initial_instructions.as_deref().map(Arc::from),
        composed.current_context.as_deref().map(Arc::from),
        has_current_context_prefix,
    );
    Arc::new(request)
}
