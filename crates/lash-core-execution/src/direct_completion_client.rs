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
        usage_sink: Option<&crate::runtime::ToolUsageLedger>,
    ) -> Result<crate::DirectCompletion, crate::PluginError>;

    #[expect(
        clippy::too_many_arguments,
        reason = "the service boundary receives the controller, lineage, causal link, and usage sink separately because each answers from a different authority"
    )]
    async fn complete_llm(
        &self,
        request: crate::LlmRequest,
        usage_source: &str,
        effect_controller: crate::ScopedEffectController<'_>,
        turn_id: Option<&crate::TurnId>,
        position: DirectExecutionPosition,
        caused_by: Option<crate::CausalRef>,
        usage_sink: Option<&crate::runtime::ToolUsageLedger>,
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
    /// `session_id` is the session the child attributes its work to and
    /// `execution_env_spec` is the environment resolved from the child's
    /// recorded `ProcessExecutionEnvRef` — an implementation bound to a
    /// different session returns `None`, and a returned service must resolve
    /// policy under `execution_env_spec`, not whatever the opener is running.
    fn bind_tool_child(
        self: Arc<Self>,
        session_id: &crate::SessionId,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Option<Arc<dyn DirectCompletionService>> {
        let _ = (session_id, execution_env_spec);
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
    /// The usage accumulator this client's spends are *also* recorded into
    /// (ADR 0099 §13, FIG-2266).
    ///
    /// Two install sites, both deliberate:
    ///
    /// * a **per-attempt sink** (`ToolUsageLedger::for_attempt`) is installed
    ///   by every `ToolAttempt` runner, so the attempt's journaled capture
    ///   carries exactly the spend that attempt made and a replay restores it;
    /// * a **child aggregate** is installed by the group-child driver's rebind,
    ///   so a child whose address space is not its opener's carries its usage
    ///   on its settlement instead of losing it.
    ///
    /// This never replaces the opener's ledger and never changes what is
    /// merged into it: it is a second reader only.
    usage_ledger: Option<crate::runtime::ToolUsageLedger>,
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
            usage_ledger: None,
        }
    }

    /// Binds the usage ledger this client's spends are also recorded into.
    ///
    /// Taken by value and returned, so the driver installs it on the clone it
    /// rebinds — a child aggregate on the child's context, a per-attempt sink
    /// on an attempt's — and the caller's own client is untouched.
    #[must_use]
    pub fn with_usage_ledger(mut self, ledger: crate::runtime::ToolUsageLedger) -> Self {
        self.usage_ledger = Some(ledger);
        self
    }

    /// The ledger this client's spends are also recorded into, when one is
    /// installed.
    ///
    /// Read by the attempt boundary to journal the attempt's captured usage
    /// and by the coordinator to merge a journaled capture back into the child
    /// aggregate it was restored for.
    pub(crate) fn usage_ledger(&self) -> Option<&crate::runtime::ToolUsageLedger> {
        self.usage_ledger.as_ref()
    }

    /// Records a completed nested call's sealed spend against the bound
    /// ledger, when this client has one.
    ///
    /// Only the test sources need the client's help — a runtime source feeds
    /// its sink inside the service, before the outcome is projected.
    #[cfg(any(test, feature = "testing"))]
    fn record_usage(&self, call_record: &crate::LlmCallRecord) {
        if let Some(ledger) = self.usage_ledger.as_ref() {
            ledger.record(call_record);
        }
    }

    /// Rebinds this client to a tool child's recorded authority (ADR 0099 §3).
    ///
    /// What is lent is the live completion transport; what is rebound is
    /// everything that decides whose call it is:
    ///
    /// * `session_id` — the session the child's work is attributed to, which a
    ///   process opener's child need not share with its opener;
    /// * `execution_env_spec` — the environment resolved from the child's
    ///   recorded `ProcessExecutionEnvRef`, which a runtime-backed service
    ///   must rebind its policy resolution to or be refused;
    /// * `effect_controller` — the child's own admitted controller, so the
    ///   direct effect is journaled under the child's claim scope;
    /// * `turn_id` and `parent_invocation` — the recorded lineage, so the
    ///   effect's causal parent is the child's, not the opener's current one;
    /// * `usage_ledger` — the child's own accumulator, so every provider
    ///   attempt's spend lands on the child's settlement.
    ///
    /// A service that cannot prove it executes under the recorded session and
    /// environment makes this a typed refusal rather than a silent authority
    /// leak.
    pub fn bind_tool_child<'child>(
        &self,
        session_id: &crate::SessionId,
        execution_env_spec: &crate::ProcessExecutionEnvSpec,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'child>,
        turn_id: Option<crate::TurnId>,
        parent_invocation: Option<crate::RuntimeInvocation>,
        usage_ledger: crate::runtime::ToolUsageLedger,
    ) -> Result<DirectCompletionClient<'child>, crate::runtime::RuntimeEffectControllerError> {
        let source = match &self.source {
            DirectCompletionSource::Runtime(source) => {
                let service = source
                    .service
                    .clone()
                    .bind_tool_child(session_id, execution_env_spec)
                    .ok_or_else(|| {
                        crate::runtime::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                            format!(
                                "the opener's direct-completion service cannot prove it executes \
                                 under session `{session_id}` and the child's recorded \
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
            usage_ledger: Some(usage_ledger),
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
            usage_ledger: self.usage_ledger.clone(),
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
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'static>,
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
            usage_ledger: self.usage_ledger.clone(),
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
                // The sink rides into the service so the sealed call record is
                // captured before its outcome is projected — a failed or
                // aborted call's billed provider attempts are usage facts too,
                // and they exist nowhere else once the record is dropped.
                source
                    .service
                    .complete(
                        request,
                        usage_source,
                        source.effect_controller.scoped(),
                        source.turn_id.as_ref(),
                        position,
                        self.usage_ledger.as_ref(),
                    )
                    .await
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::Unavailable(message) => {
                Err(crate::PluginError::Session(message.clone()))
            }
            #[cfg(any(test, feature = "testing"))]
            DirectCompletionSource::TestFn(invoke) => {
                let completion = invoke(request, usage_source.to_string())?;
                // The test source answers the call the runtime source would
                // have made, so it feeds the bound usage ledger the same way:
                // a fixture asserting capture of managed-LLM spend exercises
                // the real recording path rather than a second one.
                self.record_usage(&completion.llm_call);
                Ok(completion)
            }
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
                        self.usage_ledger.as_ref(),
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
            DirectCompletionSource::TestLlmFn(invoke) => {
                let completion = invoke(request, usage_source.to_string())?;
                self.record_usage(&completion.llm_call);
                Ok(completion)
            }
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            source: DirectCompletionSource::Unavailable(message.into()),
            parent_invocation: None,
            inside_tool_attempt: false,
            usage_ledger: None,
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
            usage_ledger: None,
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
            usage_ledger: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectExecutionPosition {
    #[default]
    Independent,
    ToolAttempt,
}
