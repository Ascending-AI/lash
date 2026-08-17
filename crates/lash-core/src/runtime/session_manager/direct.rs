use super::*;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;

/// Runtime-backed direct completion source.
///
/// Carries everything needed to plan and journal a direct LLM effect against
/// the owning session manager.
#[derive(Clone)]
struct RuntimeDirectSource<'run> {
    manager: Arc<RuntimeSessionServices>,
    effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    turn_id: Option<String>,
}

#[cfg(any(test, feature = "testing"))]
type TestDirectFn = Arc<
    dyn Fn(crate::DirectRequest, String) -> Result<crate::DirectCompletion, crate::PluginError>
        + Send
        + Sync,
>;

/// Source of direct (single-shot) LLM completions for plugins and tools.
///
/// In production this is always backed by the runtime session manager; the
/// test/testing variants exist only so that out-of-runtime test harnesses can
/// inject a canned completion without standing up a full runtime.
#[derive(Clone)]
enum DirectCompletionSource<'run> {
    Runtime(RuntimeDirectSource<'run>),
    #[cfg(any(test, feature = "testing"))]
    Unavailable(String),
    #[cfg(any(test, feature = "testing"))]
    TestFn(TestDirectFn),
}

#[derive(Clone)]
pub struct DirectCompletionClient<'run> {
    source: DirectCompletionSource<'run>,
    /// The effect this client was minted inside, when the minting site knows
    /// it. A client stamped with an open `ToolAttempt` must never journal: the
    /// controller already owns one entry for the whole attempt. Boxed because
    /// this client is captured by the deep tool-dispatch futures.
    parent_invocation: Option<Box<crate::RuntimeInvocation>>,
}

impl<'run> DirectCompletionClient<'run> {
    pub(super) fn runtime(
        manager: Arc<RuntimeSessionServices>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            source: DirectCompletionSource::Runtime(RuntimeDirectSource {
                manager,
                effect_controller,
                turn_id,
            }),
            parent_invocation: None,
        }
    }

    /// Binds this client to the effect that owns it, so every entry point —
    /// not just the tool-attributed one — classifies its journal position the
    /// same way. Applied where an attempt-scoped dispatch is derived.
    pub(crate) fn with_parent_invocation(
        mut self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> Self {
        self.parent_invocation = parent_invocation.map(Box::new);
        self
    }

    pub(crate) fn to_static(&self) -> Option<DirectCompletionClient<'static>> {
        let source = match &self.source {
            DirectCompletionSource::Runtime(source) => {
                DirectCompletionSource::Runtime(RuntimeDirectSource {
                    manager: Arc::clone(&source.manager),
                    effect_controller: source.effect_controller.to_static()?,
                    turn_id: source.turn_id.clone(),
                })
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::Unavailable(message) => {
                DirectCompletionSource::Unavailable(message.clone())
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestFn(invoke) => {
                DirectCompletionSource::TestFn(Arc::clone(invoke))
            }
        };
        Some(DirectCompletionClient {
            source,
            parent_invocation: self.parent_invocation.clone(),
        })
    }

    /// Classifies where a direct call sits relative to the journal.
    ///
    /// A caller-supplied parent wins when it names an attempt; otherwise the
    /// invocation this client was minted inside decides. Either answer must be
    /// `ToolAttempt` for the journal-free branch, because a recorded attempt
    /// replays without re-entering its body.
    fn position(
        &self,
        parent_invocation: Option<&crate::RuntimeInvocation>,
    ) -> DirectExecutionPosition {
        let is_attempt = |invocation: &crate::RuntimeInvocation| {
            invocation.effect_kind() == Some(crate::RuntimeEffectKind::ToolAttempt)
        };
        if parent_invocation.is_some_and(is_attempt)
            || self.parent_invocation.as_deref().is_some_and(is_attempt)
        {
            DirectExecutionPosition::ToolAttempt
        } else {
            DirectExecutionPosition::Independent
        }
    }

    pub async fn direct_completion(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        self.direct_completion_at(request, usage_source, self.position(None))
            .await
    }

    pub(crate) async fn direct_completion_for_tool(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
        parent_invocation: Option<&crate::RuntimeInvocation>,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        self.direct_completion_at(request, usage_source, self.position(parent_invocation))
            .await
    }

    async fn direct_completion_at(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
        position: DirectExecutionPosition,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        match &self.source {
            DirectCompletionSource::Runtime(source) => {
                source
                    .manager
                    .direct
                    .invoke_direct_completion(
                        source.invocation_context(position),
                        request,
                        usage_source,
                    )
                    .await
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::Unavailable(message) => {
                Err(crate::PluginError::Session(message.clone()))
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestFn(invoke) => invoke(request, usage_source.to_string()),
        }
    }

    /// Executes an already-normalized request using its non-empty
    /// `scope.request_id` as the caller-owned durable replay key.
    ///
    /// The request id must be unique for each logical direct call. Reusing it
    /// in the same session, turn, and usage source deliberately replays the
    /// first result even when the rest of the request differs.
    ///
    /// Replay is a property of the journal, so it does not apply inside a
    /// recorded tool attempt: a client bound to an open `ToolAttempt` executes
    /// locally and never presents its replay key, and the enclosing attempt
    /// entry is what redrive replays instead.
    pub async fn direct_llm_completion(
        &self,
        request: crate::LlmRequest,
        usage_source: &str,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        match &self.source {
            DirectCompletionSource::Runtime(source) => {
                source
                    .manager
                    .direct
                    .invoke_direct_llm_completion(
                        source.invocation_context(self.position(None)),
                        request,
                        usage_source,
                    )
                    .await
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::Unavailable(message) => {
                Err(crate::PluginError::Session(message.clone()))
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestFn(_) => Err(crate::PluginError::Session(
                "direct LLM completions are unavailable in this test context".to_string(),
            )),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            source: DirectCompletionSource::Unavailable(message.into()),
            parent_invocation: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn from_fn<F>(invoke: F) -> Self
    where
        F: Fn(crate::DirectRequest, String) -> Result<crate::DirectCompletion, crate::PluginError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            source: DirectCompletionSource::TestFn(Arc::new(invoke)),
            parent_invocation: None,
        }
    }
}

impl<'run> RuntimeDirectSource<'run> {
    fn invocation_context(&self, position: DirectExecutionPosition) -> DirectInvocationContext<'_> {
        DirectInvocationContext {
            current: &self.manager.current,
            usage_capability: &self.manager.usage,
            effect_controller: self.effect_controller.controller(),
            turn_id: self.turn_id.as_deref(),
            position,
            replay_ordinals: self.manager.direct_replay_ordinals.as_ref(),
            unkeyed_in_flight: self.manager.direct_unkeyed_in_flight.as_ref(),
        }
    }
}

pub(in crate::runtime::session_manager) struct DirectInvocationContext<'a> {
    current: &'a CurrentSessionCapability,
    usage_capability: &'a UsageCapability,
    effect_controller: &'a dyn crate::RuntimeEffectController,
    turn_id: Option<&'a str>,
    position: DirectExecutionPosition,
    replay_ordinals: &'a std::sync::Mutex<BTreeMap<String, u64>>,
    unkeyed_in_flight: &'a std::sync::Mutex<std::collections::BTreeSet<String>>,
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
    provider: crate::ProviderHandle,
    envelope: crate::RuntimeEffectEnvelope,
    request: Box<crate::LlmRequest>,
    usage_source: String,
}

#[derive(Clone, Copy)]
struct DirectReplayPosition<'a> {
    replay: Option<&'a crate::RuntimeReplay>,
    caused_by: Option<&'a crate::CausalRef>,
    ordinal: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DirectExecutionPosition {
    #[default]
    Independent,
    ToolAttempt,
}

impl DirectCompletionCapability {
    /// Plans a single direct LLM effect from a normalized [`crate::LlmRequest`].
    ///
    /// Both the text-only (`DirectRequest`) and full-output entry points feed
    /// the same effect lane; they differ only in how the caller projects the
    /// resulting [`crate::LlmResponse`].
    async fn plan_direct_effect(
        &self,
        context: &DirectInvocationContext<'_>,
        provider: crate::ProviderHandle,
        request: crate::LlmRequest,
        usage_source: &str,
        replay_position: DirectReplayPosition<'_>,
    ) -> Result<DirectEffectPlan, crate::PluginError> {
        let current = context.current;
        let usage_source = usage_source.to_string();
        for source in &request.attachments {
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
            &current.session_id,
            &usage_source,
            discriminator,
            context.turn_id,
            caused_by.cloned(),
        );
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::Direct {
                request: Box::new(request_spec),
                usage_source: usage_source.clone(),
            },
        );
        Ok(DirectEffectPlan {
            provider,
            envelope,
            request: Box::new(request),
            usage_source,
        })
    }

    /// Runs a planned direct effect across the journal/controller boundary and
    /// applies usage/trace bookkeeping, yielding the raw provider response.
    async fn run_direct_effect(
        &self,
        context: &DirectInvocationContext<'_>,
        plan: DirectEffectPlan,
        caused_by: Option<crate::CausalRef>,
    ) -> Result<(crate::LlmResponse, crate::TokenUsage, crate::LlmCallRecord), crate::PluginError>
    {
        let current = context.current;
        let DirectEffectPlan {
            provider,
            envelope,
            request,
            usage_source,
        } = plan;
        let tracing = &current.host.core.tracing;
        let replay_trace = crate::RuntimeEffectReplayTrace::for_divergence(
            tracing.trace_sink.as_ref(),
            tracing.trace_context.clone(),
            crate::trace::trace_context_from_invocation(&envelope.invocation),
            Arc::clone(&current.host.core.clock),
        );
        let local_executor = crate::RuntimeEffectLocalExecutor::direct(
            provider,
            Arc::clone(&current.host.core.durability.attachment_store),
            replay_trace,
        );
        let outcome = match context.position {
            DirectExecutionPosition::Independent => {
                context
                    .effect_controller
                    .execute_effect(envelope, local_executor)
                    .await?
            }
            DirectExecutionPosition::ToolAttempt => local_executor.execute(envelope).await?,
        };
        crate::runtime::effect::apply_direct_outcome(
            current,
            context.usage_capability,
            &request,
            &usage_source,
            caused_by.as_ref(),
            outcome,
        )
        .await
    }

    pub(in crate::runtime::session_manager) async fn invoke_direct_completion(
        &self,
        context: DirectInvocationContext<'_>,
        request: crate::DirectRequest,
        usage_source: &str,
    ) -> Result<crate::DirectCompletion, crate::PluginError> {
        let resolved = context.current.resolve_policy()?;
        let provider = resolved.provider().clone();
        let mut request = request;
        let model = request.model.clone();
        // Validate against the capability carried by the request and write the
        // resolved (alias-normalized) effort back before the provider sees it.
        request.model_variant = request
            .model_capability
            .validate_selection(&model, provider.kind(), &request.model_variant)
            .map_err(|error| crate::PluginError::Session(error.message))?;
        let replay = request.replay.clone();
        let caused_by = request.caused_by.clone();
        let _unkeyed_guard = if context.position == DirectExecutionPosition::Independent
            && replay.as_ref().is_none_or(|replay| replay.key.is_empty())
        {
            Some(context.claim_unkeyed_lane(caused_by.as_ref(), usage_source)?)
        } else {
            None
        };
        // Allocate the sequential fallback before request preparation performs
        // any await. Concurrent callers must provide explicit replay keys;
        // this ordinal represents sequential program order only.
        let replay_ordinal = if context.position == DirectExecutionPosition::ToolAttempt
            || replay.as_ref().is_some_and(|replay| !replay.key.is_empty())
        {
            0
        } else {
            context.next_replay_ordinal(caused_by.as_ref(), usage_source)?
        };
        let normalized = crate::direct::build_llm_request(&provider, request, model);
        let plan = self
            .plan_direct_effect(
                &context,
                provider,
                normalized,
                usage_source,
                DirectReplayPosition {
                    replay: replay.as_ref(),
                    caused_by: caused_by.as_ref(),
                    ordinal: replay_ordinal,
                },
            )
            .await?;
        let (response, usage, llm_call) = self.run_direct_effect(&context, plan, caused_by).await?;
        Ok(crate::DirectCompletion {
            text: response.full_text,
            usage,
            llm_call,
        })
    }

    pub(in crate::runtime::session_manager) async fn invoke_direct_llm_completion(
        &self,
        context: DirectInvocationContext<'_>,
        request: crate::LlmRequest,
        usage_source: &str,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        let resolved = context.current.resolve_policy()?;
        if request.scope.request_id.trim().is_empty() {
            return Err(crate::PluginError::Session(
                "direct LLM completion request_id must be non-empty for durable replay".to_string(),
            ));
        }
        let replay = crate::RuntimeReplay {
            key: request.scope.request_id.clone(),
            attribution: None,
        };
        let plan = self
            .plan_direct_effect(
                &context,
                resolved.binding.provider,
                request,
                usage_source,
                DirectReplayPosition {
                    replay: Some(&replay),
                    caused_by: None,
                    ordinal: 0,
                },
            )
            .await?;
        let (response, usage, llm_call) = self.run_direct_effect(&context, plan, None).await?;
        Ok(crate::DirectLlmCompletion {
            response,
            usage,
            llm_call,
        })
    }
}
