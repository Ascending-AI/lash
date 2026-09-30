use std::sync::Arc;

#[async_trait::async_trait]
pub trait DirectCompletionService: Send + Sync {
    async fn complete(
        &self,
        request: crate::DirectRequest,
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        usage_run: Option<&crate::UsageRun>,
    ) -> Result<crate::DirectCompletion, crate::PluginError>;

    #[expect(
        clippy::too_many_arguments,
        reason = "the service boundary receives the controller, lineage, causal link, and usage run separately because each answers from a different authority"
    )]
    async fn complete_llm(
        &self,
        request: crate::LlmRequest,
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        caused_by: Option<crate::CausalRef>,
        usage_run: Option<&crate::UsageRun>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError>;

    /// Rebinds this service to a tool child's recorded authority, when the
    /// implementation carries authority of its own.
    ///
    /// A `DirectCompletionService` is the live completion *transport* a group
    /// child borrows from its opener. The transport is lent; everything that
    /// decides whose call it is — the session the provider call resolves its
    /// policy under, the environment it was admitted with — must answer from
    /// the child's *recorded* facts, and the default answers `None` because a
    /// service that cannot prove it executes under those facts is refused
    /// rather than lent the opener's (ADR 0099 §3).
    ///
    /// `owner` is who the child's work runs for and `execution_env_spec` is
    /// the environment resolved from the child's recorded
    /// `ProcessExecutionEnvRef` — an implementation bound to a different
    /// owner returns `None`, and a returned service must resolve policy under
    /// `execution_env_spec`, not whatever the opener is running.
    fn bind_tool_child(
        self: Arc<Self>,
        owner: &crate::RuntimeOwner,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Arc<dyn DirectCompletionService>> {
        let _ = (owner, execution_env_spec);
        None
    }

    /// Where a tool attempt's nested completions through this service are
    /// accounted (ADR 0125): the ledger the attempt's
    /// [`UsageRun`](crate::UsageRun) is admitted to and settled in. `None`
    /// means this service dispatches no provider call of its own.
    fn usage_accounting(&self) -> Option<crate::UsageAccountingBinding> {
        None
    }
}

/// Runtime-backed direct completion source.
///
/// Carries everything needed to plan and journal a direct LLM effect against
/// the owning session manager.
#[derive(Clone)]
struct RuntimeDirectSource<'run> {
    service: Arc<dyn DirectCompletionService>,
    effect_controller: crate::runtime::ScopedEffectController<'run>,
    turn_id: Option<crate::TurnId>,
}

#[cfg(any(test, feature = "testing"))]
type TestDirectFn = Arc<
    dyn Fn(crate::DirectRequest, String) -> Result<crate::DirectCompletion, crate::PluginError>
        + Send
        + Sync,
>;

#[cfg(any(test, feature = "testing"))]
type TestDirectLlmFn = Arc<
    dyn Fn(crate::LlmRequest, String) -> Result<crate::DirectLlmCompletion, crate::PluginError>
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
    #[cfg(any(test, feature = "testing"))]
    TestLlmFn(TestDirectLlmFn),
}

#[derive(Clone)]
pub struct DirectCompletionClient<'run> {
    source: DirectCompletionSource<'run>,
    /// The effect this client was minted inside, when the minting site knows
    /// it. A client stamped with an open `ToolAttempt` must never journal: the
    /// controller already owns one entry for the whole attempt. Boxed because
    /// this client is captured by the deep tool-dispatch futures.
    parent_invocation: Option<Box<crate::RuntimeInvocation>>,
    inside_tool_attempt: bool,
    /// The usage run of the `ToolAttempt` effect this client was minted
    /// inside (ADR 0125). A completion inside a recorded attempt never
    /// journals its own effect, so every provider call it dispatches is a
    /// call of the attempt's run; without one it is refused before dispatch.
    usage_run: Option<crate::UsageRun>,
}

impl<'run> DirectCompletionClient<'run> {
    pub(crate) fn runtime(
        service: Arc<dyn DirectCompletionService>,
        effect_controller: crate::runtime::ScopedEffectController<'run>,
        turn_id: Option<crate::TurnId>,
    ) -> Self {
        Self {
            source: DirectCompletionSource::Runtime(RuntimeDirectSource {
                service,
                effect_controller,
                turn_id,
            }),
            parent_invocation: None,
            inside_tool_attempt: false,
            usage_run: None,
        }
    }

    /// Binds the usage run of the `ToolAttempt` effect this client now runs
    /// inside. Taken by value and returned, so an attempt installs it on the
    /// clone it runs with and the caller's own client is untouched.
    #[must_use]
    pub fn with_usage_run(mut self, usage_run: Option<crate::UsageRun>) -> Self {
        self.usage_run = usage_run;
        self
    }

    /// Where a tool attempt's nested completions through this client are
    /// accounted: the ledger behind its runtime service. `None` for a client
    /// no runtime service backs.
    pub(crate) fn usage_accounting(&self) -> Option<crate::UsageAccountingBinding> {
        match &self.source {
            DirectCompletionSource::Runtime(source) => source.service.usage_accounting(),
            #[cfg(any(test, feature = "testing"))]
            _ => None,
        }
    }

    /// Rebinds this client to a tool child's recorded authority (ADR 0099 §3).
    ///
    /// What is lent is the live completion transport; what is rebound is
    /// everything that decides whose call it is:
    ///
    /// * `owner` — who the child's work runs for: the session and frame it
    ///   recorded, or its process;
    /// * `execution_env_spec` — the environment resolved from the child's
    ///   recorded `ProcessExecutionEnvRef`, which a runtime-backed service
    ///   must rebind its policy resolution to or be refused;
    /// * `effect_controller` — the child's own admitted controller, so the
    ///   direct effect is journaled under the child's claim scope;
    /// * `turn_id` and `parent_invocation` — the recorded lineage, so the
    ///   effect's causal parent is the child's, not the opener's current one.
    ///
    /// No usage run is lent: each of the child's attempts is its own spending
    /// effect and installs its own run.
    ///
    /// A service that cannot prove it executes under the recorded owner and
    /// environment makes this a typed refusal rather than a silent authority
    /// leak.
    pub(crate) fn bind_tool_child<'child>(
        &self,
        owner: &crate::RuntimeOwner,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
        effect_controller: crate::runtime::ScopedEffectController<'child>,
        turn_id: Option<crate::TurnId>,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> Result<DirectCompletionClient<'child>, crate::runtime::RuntimeEffectControllerError> {
        let source = match &self.source {
            DirectCompletionSource::Runtime(source) => {
                let service = source
                    .service
                    .clone()
                    .bind_tool_child(owner, execution_env_spec)
                    .ok_or_else(|| {
                        crate::runtime::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                            format!(
                                "the opener's direct-completion service cannot prove it executes \
                                 for `{owner}` and the child's recorded \
                                 environment; a managed-LLM call is refused rather than journaled \
                                 under the opener's authority"
                            ),
                        )
                    })?;
                DirectCompletionSource::Runtime(RuntimeDirectSource {
                    service,
                    effect_controller,
                    turn_id,
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
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestLlmFn(invoke) => {
                DirectCompletionSource::TestLlmFn(Arc::clone(invoke))
            }
        };
        Ok(DirectCompletionClient {
            source,
            parent_invocation: parent_invocation.map(Box::new),
            inside_tool_attempt: self.inside_tool_attempt,
            usage_run: None,
        })
    }

    pub(crate) fn to_static(&self) -> Option<DirectCompletionClient<'static>> {
        let source = match &self.source {
            DirectCompletionSource::Runtime(source) => {
                DirectCompletionSource::Runtime(RuntimeDirectSource {
                    service: Arc::clone(&source.service),
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
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestLlmFn(invoke) => {
                DirectCompletionSource::TestLlmFn(Arc::clone(invoke))
            }
        };
        Some(DirectCompletionClient {
            source,
            parent_invocation: self.parent_invocation.clone(),
            inside_tool_attempt: self.inside_tool_attempt,
            usage_run: self.usage_run.clone(),
        })
    }

    /// This client taken to `'static` with its controller slot lent
    /// `effect_controller` — the same conversion as [`Self::to_static`], but
    /// for an opener whose own controller cannot be taken static (a Restate
    /// handler's context-bound one) and so lends the deployment host's owned
    /// controller for its admitted scope instead.
    ///
    /// The lent controller never executes the child: the group-child driver
    /// rebinds `direct_completions` through [`Self::bind_tool_child`] with the
    /// child's own recorded authority before any call can ride it.
    pub(crate) fn lend_static(
        &self,
        effect_controller: crate::runtime::ScopedEffectController<'static>,
    ) -> DirectCompletionClient<'static> {
        let source = match &self.source {
            DirectCompletionSource::Runtime(source) => {
                DirectCompletionSource::Runtime(RuntimeDirectSource {
                    service: Arc::clone(&source.service),
                    effect_controller: effect_controller.clone(),
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
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestLlmFn(invoke) => {
                DirectCompletionSource::TestLlmFn(Arc::clone(invoke))
            }
        };
        DirectCompletionClient {
            source,
            parent_invocation: self.parent_invocation.clone(),
            inside_tool_attempt: self.inside_tool_attempt,
            usage_run: self.usage_run.clone(),
        }
    }

    /// Classifies where a direct call sits relative to the journal.
    ///
    /// A caller-supplied parent wins when it names an attempt; otherwise the
    /// invocation this client was minted inside decides. Either answer must be
    /// `ToolAttempt` for the journal-free branch, because a recorded attempt
    /// replays without re-entering its body.
    fn position(
        &self,
        _parent_invocation: Option<&crate::RuntimeInvocation>,
    ) -> DirectExecutionPosition {
        if self.inside_tool_attempt {
            DirectExecutionPosition::ToolAttempt
        } else {
            DirectExecutionPosition::Independent
        }
    }

    pub fn with_tool_attempt_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.parent_invocation = Some(Box::new(parent_invocation));
        self.inside_tool_attempt = true;
        self
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
                // The attempt's run rides into the service, whose dispatch
                // takes a call from it: a completion inside a recorded
                // attempt journals nothing of its own.
                source
                    .service
                    .complete(
                        request,
                        usage_source,
                        source.effect_controller.clone(),
                        source.turn_id.as_ref(),
                        position,
                        self.usage_run.as_ref(),
                    )
                    .await
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::Unavailable(message) => {
                Err(crate::PluginError::Session(message.clone()))
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestFn(invoke) => invoke(request, usage_source.to_string()),
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestLlmFn(_) => Err(crate::PluginError::Session(
                "text direct completions are unavailable in this test context".to_string(),
            )),
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
        self.direct_llm_completion_caused_by(request, usage_source, None)
            .await
    }

    /// Same as [`Self::direct_llm_completion`], but records `caused_by` as the
    /// call's causal trace linkage and folds it into the replay lane.
    pub async fn direct_llm_completion_caused_by(
        &self,
        request: crate::LlmRequest,
        usage_source: &str,
        caused_by: Option<crate::CausalRef>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError> {
        match &self.source {
            DirectCompletionSource::Runtime(source) => {
                source
                    .service
                    .complete_llm(
                        request,
                        usage_source,
                        source.effect_controller.clone(),
                        source.turn_id.as_ref(),
                        self.position(None),
                        caused_by,
                        self.usage_run.as_ref(),
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
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestLlmFn(invoke) => invoke(request, usage_source.to_string()),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            source: DirectCompletionSource::Unavailable(message.into()),
            parent_invocation: None,
            inside_tool_attempt: false,
            usage_run: None,
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
            inside_tool_attempt: false,
            usage_run: None,
        }
    }

    /// Test seam for the raw `LlmRequest` lane used by callers (such as
    /// context compaction) that build the provider request themselves.
    #[cfg(any(test, feature = "testing"))]
    pub fn from_llm_fn<F>(invoke: F) -> Self
    where
        F: Fn(crate::LlmRequest, String) -> Result<crate::DirectLlmCompletion, crate::PluginError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            source: DirectCompletionSource::TestLlmFn(Arc::new(invoke)),
            parent_invocation: None,
            inside_tool_attempt: false,
            usage_run: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectExecutionPosition {
    #[default]
    Independent,
    ToolAttempt,
}

/// The direct-completion client a runtime lends a turn or process: completions
/// run through `service` under `effect_controller`'s admitted scope.
///
/// The runtime's construction seam; `core_internal` re-exports it and the
/// `lash` facade does not.
pub fn runtime_direct_completion_client<'run>(
    service: Arc<dyn DirectCompletionService>,
    effect_controller: crate::runtime::ScopedEffectController<'run>,
    turn_id: Option<crate::TurnId>,
) -> DirectCompletionClient<'run> {
    DirectCompletionClient::runtime(service, effect_controller, turn_id)
}
