use super::*;
use crate::TurnId;
use crate::direct_completion_client::{DirectCompletionService, DirectExecutionPosition};
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;

impl RuntimeSessionServices {
    fn direct_invocation_context<'a>(
        &'a self,
        effect_controller: crate::ActorContext,
        turn_id: Option<&'a crate::TurnId>,
        position: DirectExecutionPosition,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> DirectInvocationContext<'a> {
        DirectInvocationContext {
            current: &self.current,
            effect_controller,
            turn_id,
            position,
            replay_ordinals: self.direct_replay_ordinals.as_ref(),
            unkeyed_in_flight: self.direct_unkeyed_in_flight.as_ref(),
            effect_attempt,
        }
    }
}

impl RuntimeSessionServices {}

#[async_trait::async_trait]
impl DirectCompletionService for RuntimeSessionServices {
    async fn complete(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
        effect_controller: crate::ActorContext,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        effect_attempt: Option<&crate::EffectAttempt>,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        self.direct
            .invoke_direct_completion(
                self.direct_invocation_context(
                    effect_controller,
                    turn_id,
                    position,
                    effect_attempt.cloned(),
                ),
                request,
                usage_source,
            )
            .await
    }

    async fn complete_llm(
        &self,
        request: crate::LlmRequest,
        purpose: crate::prompt_sections::PromptPurpose,
        facts: Option<Arc<dyn std::any::Any + Send + Sync>>,
        usage_source: &str,
        effect_controller: crate::ActorContext,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        caused_by: Option<crate::CausalRef>,
        effect_attempt: Option<&crate::EffectAttempt>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        self.direct
            .invoke_direct_llm_completion(
                self.direct_invocation_context(
                    effect_controller,
                    turn_id,
                    position,
                    effect_attempt.cloned(),
                ),
                request,
                purpose,
                facts,
                usage_source,
                caused_by,
            )
            .await
    }
}

pub(in crate::runtime::session_manager) struct DirectInvocationContext<'a> {
    current: &'a CurrentOwnerCapability,
    effect_controller: crate::ActorContext,
    turn_id: Option<&'a TurnId>,
    position: DirectExecutionPosition,
    replay_ordinals: &'a std::sync::Mutex<BTreeMap<String, u64>>,
    unkeyed_in_flight: &'a std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// The fault latch of the tool attempt this completion runs inside.
    /// A bind fault aborts that opaque attempt before it can record an outcome.
    effect_attempt: Option<crate::EffectAttempt>,
}

impl DirectInvocationContext<'_> {
    fn replay_lane(caused_by: Option<&crate::CausalRef>, usage_source: &str) -> String {
        let cause = caused_by
            .map(crate::runtime::causal::causal_replay_discriminator)
            .unwrap_or_else(|| "independent".to_string());
        format!(
            "{}:{cause}{}:{usage_source}",
            cause.len(),
            usage_source.len()
        )
    }

    fn next_replay_ordinal(
        &self,
        caused_by: Option<&crate::CausalRef>,
        usage_source: &str,
    ) -> Result<u64, crate::PluginError> {
        let lane = Self::replay_lane(caused_by, usage_source);
        let mut ordinals = self.replay_ordinals.lock_recover();
        let ordinal = ordinals.entry(lane).or_default();
        *ordinal = ordinal.checked_add(1).ok_or_else(|| {
            crate::PluginError::Session("direct replay ordinal exhausted".to_string())
        })?;
        Ok(*ordinal)
    }

    fn claim_unkeyed_lane(
        &self,
        caused_by: Option<&crate::CausalRef>,
        usage_source: &str,
    ) -> Result<DirectUnkeyedGuard<'_>, crate::PluginError> {
        let lane = Self::replay_lane(caused_by, usage_source);
        let mut in_flight = self.unkeyed_in_flight.lock_recover();
        if !in_flight.insert(lane.clone()) {
            return Err(crate::PluginError::Session(
                "concurrent direct completions require distinct explicit replay keys".to_string(),
            ));
        }
        Ok(DirectUnkeyedGuard {
            in_flight: self.unkeyed_in_flight,
            lane,
        })
    }
}

struct DirectUnkeyedGuard<'a> {
    in_flight: &'a std::sync::Mutex<std::collections::BTreeSet<String>>,
    lane: String,
}

impl Drop for DirectUnkeyedGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.lock_recover().remove(&self.lane);
    }
}

#[derive(Clone, Copy)]
struct DirectReplayPosition<'a> {
    replay: Option<&'a crate::RuntimeReplay>,
    caused_by: Option<&'a crate::CausalRef>,
    ordinal: u64,
}

impl CurrentOwnerCapability {
    /// What an owned call of these services composes its prompt from: the
    /// session's recorded plan, config and committed view, or, for a
    /// process, its captured config under the default plan.
    fn owned_prompt(
        &self,
        facts: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> crate::runtime::owned_call::OwnedPrompt {
        let config = self.plugins.admitted_plugin_config();
        match self.session() {
            Some(session) => {
                let state = session.snapshot.to_runtime_state();
                crate::runtime::owned_call::OwnedPrompt {
                    facts,
                    plugins: Arc::clone(&self.plugins),
                    plan: state.authority.prompt_plan.clone(),
                    config,
                    frame: state.current_frame_node_id.clone(),
                    session: Some(crate::SessionReadView::from_runtime_state(
                        &state,
                        state.effective_policy().clone(),
                        state.effective_protocol_turn_options(),
                    )),
                }
            }
            None => crate::runtime::owned_call::OwnedPrompt {
                facts,
                plugins: Arc::clone(&self.plugins),
                plan: Default::default(),
                config,
                frame: None,
                session: None,
            },
        }
    }

    /// The session an owned call's admission belongs to: the owning
    /// session, or the process's own record namespace.
    fn owned_call_session(&self) -> SessionId {
        match self.runtime_owner() {
            crate::RuntimeOwner::Session(session) => session,
            crate::RuntimeOwner::Process(process) => {
                SessionId::prefixed("process:", process.as_str())
            }
        }
    }
}

/// The purpose and derived inputs a call's section composition uses.
struct DirectPromptInput {
    purpose: crate::prompt_sections::PromptPurpose,
    facts: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl DirectCompletionCapability {
    /// Admit one direct call (ADR 0133 §8) and run its send across the
    /// effect boundary, yielding the raw provider response. The call is
    /// identified by its effect address under the owner's scope; its purpose
    /// selects the sections it composes. The recorded model result carries
    /// usage and sealed attempt history.
    async fn run_direct_call(
        &self,
        context: &DirectInvocationContext<'_>,
        binding: crate::LlmProfileBinding,
        request: crate::LlmRequest,
        prompt: DirectPromptInput,
        usage_source: &str,
        replay_position: DirectReplayPosition<'_>,
    ) -> Result<(crate::LlmResponse, crate::LlmUsage, crate::LlmCallRecord), crate::PluginError>
    {
        let DirectPromptInput { purpose, facts } = prompt;
        let current = context.current;
        let DirectReplayPosition {
            replay,
            caused_by,
            ordinal,
        } = replay_position;
        let discriminator =
            crate::runtime::causal::direct_request_discriminator(replay, caused_by, ordinal);
        let invocation = crate::runtime::causal::direct_effect_invocation(
            context.effect_controller.execution_scope(),
            &current.runtime_owner(),
            usage_source,
            discriminator,
            context.turn_id,
            caused_by.cloned(),
        );
        let key = crate::runtime::owned_call::owned_call_key(
            current.owned_call_session(),
            context.effect_controller.execution_scope(),
            invocation.effect_replay_key(),
        );
        let admit = crate::runtime::owned_call::OwnedCall {
            cx: &context.effect_controller,
            key,
            purpose,
            prompt: current.owned_prompt(facts),
            request,
            binding: binding.clone(),
            attachment_store: Arc::clone(&current.host.core.durability.attachment_store),
            fetch_horizon: current.host.core.providers.delivery_fetch_horizon,
            budgets: current.host.core.control.execution_budgets.clone(),
        }
        .admit();
        let admission = Box::pin(admit).await?;
        let (request, admitted) = match admission {
            crate::runtime::owned_call::OwnedAdmission::Send { request, admitted } => {
                (*request, admitted)
            }
            crate::runtime::owned_call::OwnedAdmission::Unsent(error) => {
                return super::direct_outcome::apply_direct_outcome(
                    crate::RuntimeEffectOutcome::Direct {
                        result: Box::new(Err(error)),
                        call_record: None,
                    },
                );
            }
        };
        let request_spec = crate::LlmRequestSpec::from_request(&request);
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::Direct {
                request: Box::new(request_spec),
                usage_source: usage_source.to_string(),
            },
        );
        let tracing = &current.host.core.tracing;
        let replay_trace = crate::RuntimeEffectReplayTrace::for_divergence(
            tracing,
            context.effect_controller.trace_scope().cloned(),
            crate::trace::trace_context_from_effect_invocation(&envelope.invocation),
        );
        let local_executor = crate::RuntimeEffectLocalExecutor::direct(
            binding,
            current.policy.charge_safety.clone(),
            current.host.core.control.execution_budgets.clone(),
            admitted,
            current.runtime_owner(),
            tracing.clone(),
            replay_trace,
        );
        let outcome = match context.position {
            DirectExecutionPosition::Independent => {
                context
                    .effect_controller
                    .turn_effect(envelope, local_executor)
                    .await?
            }
            DirectExecutionPosition::ToolAttempt => {
                local_executor
                    .execute_within_attempt(
                        envelope,
                        context.effect_attempt.clone(),
                        &context.effect_controller,
                    )
                    .await?
            }
        };
        super::direct_outcome::apply_direct_outcome(outcome)
    }

    pub(in crate::runtime::session_manager) async fn invoke_direct_completion(
        &self,
        context: DirectInvocationContext<'_>,
        request: crate::DirectRequest,
        usage_source: &str,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        let policy = context.current.resolve_policy()?;
        let binding = policy.binding().clone();
        let model = policy.llm_profile_config().clone();
        let replay = request.replay.clone();
        let caused_by = request.caused_by.clone();
        let keyed = replay.as_ref().is_some_and(|replay| !replay.key.is_empty());
        let _unkeyed_guard = if context.position == DirectExecutionPosition::Independent && !keyed {
            Some(context.claim_unkeyed_lane(caused_by.as_ref(), usage_source)?)
        } else {
            None
        };
        // Concurrent callers must provide explicit replay keys; this ordinal
        // represents sequential program order only: the lane's within these
        // services, or, inside a tool attempt, the attempt scope's own.
        let replay_ordinal = if keyed {
            0
        } else if context.position == DirectExecutionPosition::ToolAttempt {
            u64::from(context.effect_controller.next_completion_ordinal())
        } else {
            context.next_replay_ordinal(caused_by.as_ref(), usage_source)?
        };
        let mut normalized = crate::direct::build_llm_request(request, model)
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        // A durable direct completion renders attachments under its session's
        // recorded acceptance rules, never a caller's.
        normalized.attachment_acceptance =
            std::sync::Arc::clone(&context.current.policy.attachment_acceptance);
        let (response, usage, llm_call) = self
            .run_direct_call(
                &context,
                binding,
                normalized,
                DirectPromptInput {
                    purpose: crate::prompt_sections::PromptPurpose::Direct {
                        name: usage_source.to_string(),
                    },
                    facts: None,
                },
                usage_source,
                DirectReplayPosition {
                    replay: replay.as_ref(),
                    caused_by: caused_by.as_ref(),
                    ordinal: replay_ordinal,
                },
            )
            .await?;
        Ok(crate::DirectCompletion {
            text: response.full_text(),
            usage,
            llm_call,
        })
    }

    pub(in crate::runtime::session_manager) async fn invoke_direct_llm_completion(
        &self,
        context: DirectInvocationContext<'_>,
        mut request: crate::LlmRequest,
        purpose: crate::prompt_sections::PromptPurpose,
        facts: Option<Arc<dyn std::any::Any + Send + Sync>>,
        usage_source: &str,
        caused_by: Option<crate::CausalRef>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        let policy = context.current.resolve_policy()?;
        let binding = policy.binding().clone();
        if request.scope.request_id.trim().is_empty() {
            return Err(crate::PluginError::Session(
                "direct LLM completion request_id must be non-empty for durable replay".to_string(),
            ));
        }
        request.model = policy.llm_profile_config().clone();
        request.attachment_acceptance = Arc::clone(&context.current.policy.attachment_acceptance);
        let replay = crate::RuntimeReplay {
            key: request.scope.request_id.clone(),
            attribution: None,
        };
        let (response, usage, llm_call) = self
            .run_direct_call(
                &context,
                binding,
                request,
                DirectPromptInput { purpose, facts },
                usage_source,
                DirectReplayPosition {
                    replay: Some(&replay),
                    caused_by: caused_by.as_ref(),
                    ordinal: 0,
                },
            )
            .await?;
        Ok(crate::DirectLlmCompletion {
            response,
            usage,
            llm_call,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::helpers::standard_test_policy;

    const SESSION_ID: &str = "direct-completion-rebind-session";

    /// Real session services over a live runtime: the law asserts on the
    /// production rebind, not a fixture's idea of it.
    async fn session_services() -> (Arc<RuntimeSessionServices>, crate::SessionPolicy) {
        let env = crate::RuntimeEnvironment::builder(crate::RuntimeHostConfig::new(
            crate::testing::sqlite_memory_store_backend().await,
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::ToolSourcePolicy::Tolerate,
            crate::ExecutionBudgets::recommended(),
            crate::runtime::DeltaCoalescing::recommended(),
            crate::DataRetentionConfig::standard(),
        ))
        .with_plugin_host(Arc::new(crate::PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
            crate::ExecutionBudgets::recommended(),
            crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
        )))
        .build();
        let policy = standard_test_policy();
        let runtime = crate::LashRuntime::from_environment(
            &env,
            policy.clone(),
            crate::RuntimeSessionState {
                session_id: crate::SessionId::fixture(SESSION_ID.to_string()),
                policy: policy.clone(),
                ..crate::RuntimeSessionState::ambient_fixture(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                ))
            },
            None,
            crate::testing::runtime_lease_owner(),
        )
        .await
        .expect("a runtime for the direct-completion rebind law");
        (
            runtime
                .runtime_session_services()
                .expect("runtime session services"),
            policy,
        )
    }

    /// FIG-4531, FIG-4404: off the turn path, a recorded model this worker
    /// cannot bind is the same typed, retryable `LlmProfileUnavailable` the turn
    /// path answers. A direct completion and a process owner resolve their
    /// policy without binding; the body of the unjournaled call binds, and
    /// this deployment registers no models, so the recorded binding has no
    /// transport.
    #[tokio::test]
    async fn an_unbindable_llm_profile_off_the_turn_path_is_typed_llm_profile_unavailable() {
        let (services, policy) = session_services().await;
        let resolved = services
            .current
            .resolve_policy()
            .expect("resolving a policy binds nothing");
        let fault = resolved
            .binding()
            .bind_for_unjournaled_call()
            .expect_err("a deployment with no models cannot bind the recorded model");
        assert_eq!(fault.code, crate::RuntimeErrorCode::LlmProfileUnavailable);
        assert_eq!(
            fault.profile_key(),
            policy.model.as_ref().map(crate::LlmProfileConfig::key),
            "the fault names the recorded key typed: {fault:?}"
        );
        assert!(
            fault.is_attempt_fault(),
            "the fault is the attempt's, never the call's recorded result"
        );
        let error = crate::PluginError::RuntimeEffectController(fault);
        assert!(error.is_retryable() && !error.is_terminal());
    }
}
