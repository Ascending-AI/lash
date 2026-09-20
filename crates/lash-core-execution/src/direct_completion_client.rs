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
    ) -> Result<crate::DirectCompletion, crate::PluginError>;

    async fn complete_llm(
        &self,
        request: crate::LlmRequest,
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        caused_by: Option<crate::CausalRef>,
    ) -> Result<crate::DirectLlmCompletion, crate::PluginError>;
}

/// Runtime-backed direct completion source.
///
/// Carries everything needed to plan and journal a direct LLM effect against
/// the owning session manager.
#[derive(Clone)]
struct RuntimeDirectSource<'run> {
    service: Arc<dyn DirectCompletionService>,
    effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
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
}

impl<'run> DirectCompletionClient<'run> {
    pub fn runtime(
        service: Arc<dyn DirectCompletionService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
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
        }
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
                source
                    .service
                    .complete(
                        request,
                        usage_source,
                        source.effect_controller.scoped(),
                        source.turn_id.as_ref(),
                        position,
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
                        source.effect_controller.scoped(),
                        source.turn_id.as_ref(),
                        self.position(None),
                        caused_by,
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
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            source: DirectCompletionSource::Unavailable(message.into()),
            parent_invocation: None,
            inside_tool_attempt: false,
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
        }
    }

    /// Test seam for the raw `LlmRequest` lane used by callers (such as
    /// rolling-history compaction) that build the provider request themselves.
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
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectExecutionPosition {
    #[default]
    Independent,
    ToolAttempt,
}
