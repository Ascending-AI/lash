use super::{ToolContext, ToolOutcome, ToolPrepareCall};
use crate::ProcessId;
use crate::plugin::PluginError;
use crate::{PreparedToolCall, ToolContract, ToolManifest};
use std::sync::Arc;

/// Sealed process-replay environment for the rare tool body that must await
/// durable work before it can return.
///
/// The body is deterministic workflow code: it must not consult wall clock or
/// randomness, drive commands from unordered iteration, perform unjournaled
/// I/O, or leave a journaled action un-awaited.
///
/// This is a doc-hidden first-party facade-support seam. Runtime dispatch
/// constructs it only for a typed orchestrating registration and passes it
/// directly to that registration's implementation.
#[derive(Clone)]
pub struct OrchestrationContext<'run> {
    context: ToolContext<'run>,
}

impl<'run> OrchestrationContext<'run> {
    pub(crate) fn new(context: ToolContext<'run>) -> Self {
        Self { context }
    }

    pub fn session_id(&self) -> &str {
        self.context.session_id()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.context.tool_call_id()
    }

    pub fn prepared_payload(&self) -> &serde_json::Value {
        self.context.prepared_payload()
    }

    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        self.context.decode_prepared_payload()
    }

    pub fn callable_tool_manifest(&self, name: &str) -> Option<ToolManifest> {
        let dispatch = self.context.runtime_dispatch.as_ref()?;
        crate::tool_dispatch::resolve_callable_manifest(dispatch, name)
    }

    /// Session administration for a runtime-owned orchestrating body.
    ///
    /// Session reads and membership changes live in this lane; a recorded leaf
    /// attempt receives [`crate::AttemptContext`], which has no route to them.
    /// An orchestrating body that needs a related session to run a turn starts
    /// a `ProcessInput::SessionTurn` process, as `lash-subagents` does.
    pub fn sessions(&self) -> super::ToolSessionAdmin {
        self.context.sessions()
    }

    /// Emission reserves and starts deliveries through the effect controller,
    /// which a recorded leaf attempt cannot do: it declares
    /// [`crate::ToolIntent::EmitTrigger`] instead and the intent executor emits
    /// after the attempt commits.
    pub fn triggers(&self) -> super::ToolTriggerClient<'run> {
        self.context.triggers()
    }

    /// The enclosing durable parent for an orchestrated child start.
    ///
    /// The same one derivation the recorded attempt uses — the admitted scope,
    /// never a registry lookup (FIG-3417).
    pub fn child_process_parent_scope(&self) -> Result<crate::ParentScope, PluginError> {
        let scoped = self.context.effect_controller.scoped();
        let opener = crate::EffectOpener::for_scope(scoped.admitted_scope())
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(crate::ParentScope::from_owner(&opener))
    }

    pub async fn start_process(
        &self,
        request: crate::ProcessStartRequest,
    ) -> Result<crate::ProcessHandleView, PluginError> {
        self.context.process_admin().start(request).await
    }

    pub async fn await_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        self.context.process_admin().await_process(process_id).await
    }

    pub async fn cancel_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessCancelReceipt, PluginError> {
        self.context.process_admin().cancel(process_id).await
    }

    /// Emit one journaled process signal using an author-supplied stable id.
    /// Requiring the id keeps replay identity independent of randomness.
    pub async fn signal_process(
        &self,
        process_id: &ProcessId,
        signal_name: &str,
        signal_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessEvent, PluginError> {
        self.context
            .process_admin()
            .signal_with_id(process_id, signal_name, signal_id.into(), payload)
            .await
    }

    pub fn emit_child_process_started(
        &self,
        process_id: impl Into<ProcessId>,
        child_entry_name: Option<String>,
    ) {
        self.context
            .emit_child_process_started(process_id, child_entry_name);
    }

    pub async fn call_tool_batch(
        &self,
        calls: Vec<crate::ToolInvocation>,
    ) -> Vec<crate::ToolInvocationReply> {
        if let Some(runtime) = self.context.runtime_execution_context.clone() {
            // The batch carries its settlement order for callers that model
            // per-leaf completion (the language runtimes' Promise.all). This
            // front door hands providers replies in input order, so it takes
            // the replies and leaves the order to the runtime seam that needs
            // it.
            return runtime
                .with_batch_parent_call_id(self.context.tool_call_id.clone())
                .call_tool_batch(calls, crate::session::ToolBatchOccurrence::Uncounted)
                .await
                .replies;
        }
        // ADR 0099 §2/§6: a group child's orchestrating body holds no runtime
        // execution context — `RuntimeExecutionContext` is never serialized
        // and the journal lends none — but its context carries the child's
        // own rebound dispatch, which is the authority a nested call must
        // coordinate under. Each call therefore runs the same coordinator a
        // live leaf call runs: preparation, journaled attempts, retry sleeps
        // and a journaled deferred await, all on the child's admitted
        // controller rather than the opener's.
        //
        // The installed starts sink is what marks this context as a group
        // child's — only the tool-child driver sets one. Any other context
        // without a runtime execution context is the pre-cutover product
        // path, which keeps its refusal.
        let dispatch = if self.context.orchestrating_starts.is_some() {
            self.context.runtime_dispatch.clone()
        } else {
            None
        };
        let Some(dispatch) = dispatch else {
            return calls
                .into_iter()
                .map(|_| {
                    crate::ToolInvocationReply::error(serde_json::json!(
                        "tool batch orchestration is unavailable outside process replay"
                    ))
                })
                .collect();
        };
        Box::pin(coordinate_nested_tool_batch(
            &self.context,
            &dispatch,
            calls,
        ))
        .await
    }
}

/// One settled dispatch outcome as the body's reply for its call: the
/// projected output the body acts on plus the journaled record as receipt.
fn dispatch_outcome_reply(
    outcome: crate::tool_dispatch::ToolDispatchOutcome,
) -> crate::ToolInvocationReply {
    let record = outcome.record;
    crate::ToolInvocationReply::from_output(record.output.clone()).with_record(record)
}

/// Runs an orchestrating body's nested calls through the dispatch the body
/// was admitted under.
///
/// Calls run in source order. The body is replayable workflow code, and a
/// redrive must meet the same journaled attempts it already committed, so the
/// batch mints no batch envelope or concurrent schedule of its own: every
/// call is prepared the way its authority demands — the Tool Catalog for an
/// ordinary call, the grant for a granted one — and then coordinated exactly
/// the way a live leaf call is, journaled `ToolAttempt` effects under the
/// body's own admitted controller, parented to the lineage the dispatch
/// carries. A call that defers is awaited through the same journaled await
/// the child's driver parks on, under the call id the body named it with —
/// so a redrive re-derives the same replay keys and reads the recorded
/// attempt rather than running it again.
async fn coordinate_nested_tool_batch<'run>(
    body_context: &ToolContext<'run>,
    dispatch: &Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    calls: Vec<crate::ToolInvocation>,
) -> Vec<crate::ToolInvocationReply> {
    let total = calls.len();
    let mut replies = Vec::with_capacity(total);
    for mut call in calls {
        let call_id = call.id.clone();
        let grant = call.execution_grant.take();
        let tool_name = match &grant {
            Some(grant) => grant.manifest().name.clone(),
            None => match crate::tool_dispatch::resolve_callable_manifest_by_id(
                dispatch.as_ref(),
                &call.tool_id,
            ) {
                Some(manifest) => manifest.name,
                None => {
                    replies.push(crate::ToolInvocationReply::from_output(
                        crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                            crate::ToolFailureClass::Unavailable,
                            "tool_unavailable",
                            format!("Tool id `{}` is unavailable in this session", call.tool_id),
                        )),
                    ));
                    continue;
                }
            },
        };
        let pending = crate::sansio::PendingToolCall {
            call_id: call_id.clone(),
            tool_name,
            args: call.args,
            replay: None,
        };
        let preparation = match &grant {
            Some(grant) => {
                crate::tool_dispatch::prepare_granted_tool_call_with_context(
                    dispatch.as_ref(),
                    grant,
                    pending,
                    Some(call_id.clone()),
                )
                .await
            }
            None => {
                crate::tool_dispatch::prepare_tool_call_with_context(
                    dispatch.as_ref(),
                    pending,
                    Some(call_id.clone()),
                )
                .await
            }
        };
        let prepared = match preparation {
            crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => *prepared,
            crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                replies.push(dispatch_outcome_reply(*outcome));
                continue;
            }
        };

        let retry_policy = crate::tool_dispatch::resolve_retry_policy(
            dispatch.as_ref(),
            &prepared.tool_id,
            grant.as_deref(),
        );
        let turn_cancel_wait = dispatch.effect_controller.scoped().turn_cancel_wait(
            body_context
                .cancellation_token()
                .cloned()
                .unwrap_or_default(),
        );
        let tool_context = ToolContext::from_dispatch(Arc::clone(dispatch))
            .prepared_call(&prepared)
            .cancellation_token(body_context.cancellation_token().cloned())
            .enclosing_process(
                body_context
                    .enclosing_process()
                    .map(|process_id| ProcessId::from(process_id.to_string())),
            )
            .parent_invocation(dispatch.parent_invocation.clone())
            .child_execution_trace_hook(call.child_execution_trace_hook.clone())
            .build();
        let executor_dispatch = Arc::clone(dispatch);
        let executor_context = tool_context.clone();
        let coordinated = Box::pin(crate::tool_dispatch::coordinate_tool_invocation(
            dispatch.as_ref(),
            prepared,
            grant,
            retry_policy,
            // A nested call is a live admission, not a retained fact: whether
            // it may defer is read from the live registry, exactly as any
            // live caller's would be.
            None,
            crate::tool_dispatch::ToolAttemptEffectIdentity::Scalar {
                parent: dispatch.parent_invocation.clone(),
            },
            &turn_cancel_wait,
            // §5's drain gate is a batch aggregate's; a body-driven batch
            // runs its calls in source order and each drains its own intents.
            None,
            call.child_execution_trace_hook.clone(),
            move |completion_key| {
                crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                    Arc::clone(&executor_dispatch),
                    executor_context.clone(),
                    completion_key,
                )
            },
        ))
        .await;
        // The journaled attempts' triggers were drained into the outcome; they
        // belong to the body's own buffer, which its settlement carries.
        for trigger in coordinated.triggers {
            dispatch.trigger_outcomes.enqueue(trigger);
        }
        match coordinated.launch {
            crate::tool_dispatch::ToolCallLaunch::Done(outcome) => {
                replies.push(dispatch_outcome_reply(*outcome));
            }
            crate::tool_dispatch::ToolCallLaunch::Pending(pending) => {
                let outcome = crate::runtime::effect::await_journaled_tool_completion(
                    dispatch.as_ref(),
                    dispatch.parent_invocation.as_ref(),
                    &call_id,
                    *pending,
                    &turn_cancel_wait,
                )
                .await;
                replies.push(dispatch_outcome_reply(outcome));
            }
            crate::tool_dispatch::ToolCallLaunch::ControllerAborted(error) => {
                // A controller refusal poisons the batch: issuing further
                // attempts under it would only mint more refusals, so the
                // remaining calls take the same failure rather than a run.
                let failure = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "tool_call_controller_aborted",
                    error.to_string(),
                ));
                replies.push(crate::ToolInvocationReply::from_output(failure.clone()));
                while replies.len() < total {
                    replies.push(crate::ToolInvocationReply::from_output(failure.clone()));
                }
                break;
            }
        }
    }
    replies
}

/// Implementation contract carried by an [`OrchestratingToolDef`].
///
/// First-party crates keep their concrete implementation types private and
/// expose only the completed definition. Hosts can enable such a definition,
/// but leaf providers cannot be upgraded into this lane.
#[async_trait::async_trait]
pub trait OrchestratingToolImplementation: Send + Sync + 'static {
    fn manifest(&self) -> ToolManifest;

    fn contract(&self) -> Arc<ToolContract>;

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        context: &OrchestrationContext<'_>,
    ) -> ToolOutcome;
}

/// Opaque definition for a first-party orchestrating registration.
///
/// The registry accepts this completed definition as a distinct registration
/// kind. It never recognizes or upgrades a leaf provider by id or source name.
#[derive(Clone)]
pub struct OrchestratingToolDef {
    implementation: Arc<dyn OrchestratingToolImplementation>,
}

impl OrchestratingToolDef {
    /// Package an implementation supplied by an owning first-party crate.
    ///
    /// # Safety
    /// The caller must be the crate that owns the registered tool contract.
    /// This is an unsafe capability boundary so ordinary downstream Rust code cannot mint an
    /// orchestrating registration from a leaf provider.
    /// a provenance convention, not a memory-safety invariant: violating it is
    /// an unsupported capability escalation, but does not by itself cause
    /// undefined behavior.
    #[expect(
        unsafe_code,
        reason = "this fn is the unsafe capability boundary itself: only the crate that owns a tool contract may mint an orchestrating registration"
    )]
    pub unsafe fn from_first_party(
        implementation: Arc<dyn OrchestratingToolImplementation>,
    ) -> Self {
        Self { implementation }
    }

    #[cfg(test)]
    pub(crate) fn new(implementation: Arc<dyn OrchestratingToolImplementation>) -> Self {
        Self { implementation }
    }

    pub(crate) fn manifest(&self) -> ToolManifest {
        self.implementation.manifest()
    }

    pub(crate) fn contract(&self) -> Arc<ToolContract> {
        self.implementation.contract()
    }

    pub(crate) async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        self.implementation.prepare_tool_call(call).await
    }

    pub(crate) async fn execute(
        &self,
        args: &serde_json::Value,
        context: &OrchestrationContext<'_>,
    ) -> ToolOutcome {
        self.implementation.execute(args, context).await
    }
}
