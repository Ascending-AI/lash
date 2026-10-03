use super::*;
use crate::TurnId;
use crate::direct_completion_client::{DirectCompletionService, DirectExecutionPosition};
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;

impl RuntimeSessionServices {
    fn direct_invocation_context<'a>(
        &'a self,
        effect_controller: crate::ScopedEffectController<'a>,
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

impl RuntimeSessionServices {
    /// The concrete half of the trait rebind below: the same refusal and the
    /// same policy swap, but it returns the rebound services themselves so a
    /// caller that already holds the concrete type — and a test asserting on
    /// what was lent — keeps it.
    fn bound_tool_child_services(
        &self,
        owner: &crate::RuntimeOwner,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Self> {
        if *owner != self.current.runtime_owner() {
            return None;
        }
        let mut services = self.clone();
        services.current.policy = execution_env_spec.policy.clone();
        Some(services)
    }
}

#[async_trait::async_trait]
impl DirectCompletionService for RuntimeSessionServices {
    async fn complete(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
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
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
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
                usage_source,
                caused_by,
            )
            .await
    }

    /// Rebinds this service to a tool child's recorded authority.
    ///
    /// The transport — the managed session and the provider registry — is
    /// lent unchanged; what is rebound is everything that
    /// decides whose call it is. `current.policy` is replaced with the
    /// child's recorded environment policy so provider and budget resolution
    /// answer under the facts the child was admitted with, and a service
    /// asked to rebind to a different session refuses: the transport is
    /// session-bound, and lending it across sessions would journal the
    /// child's call under the opener's session authority (ADR 0099 §3).
    fn bind_tool_child(
        self: Arc<Self>,
        owner: &crate::RuntimeOwner,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Arc<dyn DirectCompletionService>> {
        self.bound_tool_child_services(owner, execution_env_spec)
            .map(|services| Arc::new(services) as Arc<dyn DirectCompletionService>)
    }
}

pub(in crate::runtime::session_manager) struct DirectInvocationContext<'a> {
    current: &'a CurrentOwnerCapability,
    effect_controller: crate::ScopedEffectController<'a>,
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

struct DirectEffectPlan {
    /// The lazy binding of the session's recorded model: the effect's body
    /// binds it, and only when the completion is unjournaled (FIG-4404).
    binding: crate::LlmProfileBinding,
    envelope: crate::RuntimeEffectEnvelope,
}

#[derive(Clone, Copy)]
struct DirectReplayPosition<'a> {
    replay: Option<&'a crate::RuntimeReplay>,
    caused_by: Option<&'a crate::CausalRef>,
    ordinal: u64,
}

impl DirectCompletionCapability {
    /// Plans a single direct LLM effect from a normalized [`crate::LlmRequest`].
    ///
    /// Both the text-only (`DirectRequest`) and full-output entry points feed
    /// the same effect lane; they differ only in how the caller projects the
    /// resulting [`crate::LlmResponse`].
    ///
    /// The envelope carries the recorded key of `binding`'s model, so the
    /// journaled result names the selection the completion ran under.
    async fn plan_direct_effect(
        &self,
        context: &DirectInvocationContext<'_>,
        binding: crate::LlmProfileBinding,
        request: crate::LlmRequest,
        usage_source: &str,
        replay_position: DirectReplayPosition<'_>,
    ) -> Result<DirectEffectPlan, crate::PluginError> {
        let current = context.current;
        let usage_source = usage_source.to_string();
        for source in &request.attachments() {
            current
                .host
                .core
                .attachment_source_policy
                .authorize(&crate::AttachmentProducer::Host, source)
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        }
        let request_spec = crate::LlmRequestSpec::from_request(
            &request,
            current.host.core.durability.attachment_store.as_ref(),
        )
        .await?;
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
            &usage_source,
            discriminator,
            context.turn_id,
            caused_by.cloned(),
        );
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::Direct {
                request: Box::new(request_spec),
                usage_source,
            },
        );
        Ok(DirectEffectPlan { binding, envelope })
    }

    /// Runs a planned direct effect across the journal/controller boundary and
    /// applies trace bookkeeping, yielding the raw provider response. The
    /// recorded model result carries usage and sealed attempt history.
    async fn run_direct_effect(
        &self,
        context: &DirectInvocationContext<'_>,
        plan: DirectEffectPlan,
    ) -> Result<(crate::LlmResponse, crate::TokenUsage, crate::LlmCallRecord), crate::PluginError>
    {
        let current = context.current;
        let DirectEffectPlan { binding, envelope } = plan;
        let tracing = &current.host.core.tracing;
        let replay_trace = crate::RuntimeEffectReplayTrace::for_divergence(
            tracing,
            context.effect_controller.trace_scope().cloned(),
            crate::trace::trace_context_from_effect_invocation(&envelope.invocation),
        );
        let local_executor = crate::RuntimeEffectLocalExecutor::direct(
            binding,
            current.policy.charge_safety.clone(),
            Arc::clone(&current.host.core.durability.attachment_store),
            current.runtime_owner(),
            tracing.clone(),
            replay_trace,
        );
        let outcome = match context.position {
            DirectExecutionPosition::Independent => {
                context
                    .effect_controller
                    .execute_effect(envelope, local_executor)
                    .await?
            }
            DirectExecutionPosition::ToolAttempt => {
                local_executor
                    .execute_within_attempt(envelope, context.effect_attempt.clone())
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
        let _unkeyed_guard = if context.position == DirectExecutionPosition::Independent
            && replay.as_ref().is_none_or(|replay| replay.key.is_empty())
        {
            Some(context.claim_unkeyed_lane(caused_by.as_ref(), usage_source)?)
        } else {
            None
        };
        // Concurrent callers must provide explicit replay keys; this ordinal represents
        // sequential program order only.
        let replay_ordinal = if context.position == DirectExecutionPosition::ToolAttempt
            || replay.as_ref().is_some_and(|replay| !replay.key.is_empty())
        {
            0
        } else {
            context.next_replay_ordinal(caused_by.as_ref(), usage_source)?
        };
        let mut normalized = crate::direct::build_llm_request(request, model)
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        // A durable direct completion renders attachments under its session's
        // recorded acceptance rules, never a caller's.
        normalized.attachment_acceptance =
            std::sync::Arc::clone(&context.current.policy.attachment_acceptance);
        let plan = self
            .plan_direct_effect(
                &context,
                binding,
                normalized,
                usage_source,
                DirectReplayPosition {
                    replay: replay.as_ref(),
                    caused_by: caused_by.as_ref(),
                    ordinal: replay_ordinal,
                },
            )
            .await?;
        let (response, usage, llm_call) = self.run_direct_effect(&context, plan).await?;
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
        let plan = self
            .plan_direct_effect(
                &context,
                binding,
                request,
                usage_source,
                DirectReplayPosition {
                    replay: Some(&replay),
                    caused_by: caused_by.as_ref(),
                    ordinal: 0,
                },
            )
            .await?;
        let (response, usage, llm_call) = self.run_direct_effect(&context, plan).await?;
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
        ))
        .with_plugin_host(Arc::new(crate::PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )))
        .build();
        let policy = standard_test_policy();
        let runtime = crate::LashRuntime::from_environment(
            &env,
            policy.clone(),
            crate::RuntimeSessionState {
                session_id: crate::SessionId::fixture(SESSION_ID.to_string()),
                policy: policy.clone(),
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
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

    /// ADR 0099 §3: a managed-LLM service lent to a tool child is rebound to
    /// the child's *recorded* environment — provider and policy resolution
    /// answer under the facts the child was admitted with, not whatever the
    /// opener is running now — and a bind naming a session the transport is
    /// not bound to is refused rather than lent across.
    #[tokio::test]
    async fn a_rebound_completion_service_resolves_the_childs_recorded_policy() {
        let (services, opener_policy) = Box::pin(session_services()).await;
        let mut child_policy = opener_policy.clone();
        child_policy.model = Some(crate::testing::test_llm_profile_config(
            "child-recorded-model",
            crate::LlmProfileMetadata::builder("child-recorded-model")
                .context_window_tokens(128_000)
                .build()
                .expect("valid child model"),
        ));
        let child_env = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            child_policy.clone(),
        );

        let rebound = services
            .bound_tool_child_services(
                &crate::RuntimeOwner::Session(crate::SessionId::fixture(SESSION_ID.to_string())),
                &child_env,
            )
            .expect("the transport's own session binds");
        assert_eq!(
            rebound.current.policy, child_policy,
            "provider and budget resolution answer under the recorded environment"
        );
        assert_eq!(
            services.current.policy, opener_policy,
            "the rebind clones; the opener's services keep resolving their own policy"
        );

        assert!(
            services
                .bound_tool_child_services(
                    &crate::RuntimeOwner::Session(crate::SessionId::from("a-foreign-session")),
                    &child_env,
                )
                .is_none(),
            "a session-bound transport is refused across sessions, never lent"
        );
    }
}
