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
        incarnation: crate::ProcessIncarnation,
        attempt: Option<u32>,
        child_entry_name: Option<String>,
    ) {
        self.context
            .emit_child_process_started(process_id, incarnation, attempt, child_entry_name);
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
            return Box::pin(
                runtime
                    .with_batch_parent_call_id(self.context.tool_call_id.clone())
                    .call_tool_batch(calls),
            )
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
        let dispatch = if self.context.orchestrating_sinks.is_some() {
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

/// The body's reply for a nested call a controller refused.
///
/// The calls are already in flight together, so the body hears only a
/// failure for the call that was refused. The refusal itself is kept in the
/// child's sinks: its driver refuses the child with it, exactly as the group
/// path refuses a child whose own attempt was refused, so whatever the body
/// makes of the failure never settles as the child's result (FIG-3679).
fn refused_nested_reply(
    sinks: Option<&crate::tool_dispatch::OrchestratingChildSinks>,
    error: crate::RuntimeEffectControllerError,
) -> crate::ToolInvocationReply {
    if let Some(sinks) = sinks {
        sinks.refuse(error.clone());
    }
    crate::ToolInvocationReply::from_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "tool_call_controller_aborted",
            error.to_string(),
        ),
    ))
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
/// Replies come back in source order. The body is replayable workflow code,
/// and a redrive must meet the same journaled attempts it already committed,
/// so the batch mints no batch envelope of its own: every call is prepared the
/// way its authority demands — the Tool Catalog for an ordinary call, the
/// grant for a granted one — and then coordinated exactly the way a live leaf
/// call is, journaled `ToolAttempt` effects under the body's own admitted
/// controller, parented to the lineage the dispatch carries. A call that
/// defers is awaited through the same journaled await the child's driver
/// parks on, under the call id the body named it with.
///
/// Whether the calls overlap is the body's controller's to decide, through
/// [`drive_independent_effect_work`](crate::RuntimeEffectController::drive_independent_effect_work).
/// A controller that finds each recorded attempt by its replay key runs them
/// concurrently, because a redrive re-derives the same keys and reads the
/// recorded attempts whatever order they committed in. One that replays its
/// journal by position runs them one at a time in source order. Concurrent
/// calls would commit in whatever order they reached that journal, the redrive
/// would reissue them in another, and the replay would refuse the mismatch
/// (FIG-3671).
async fn coordinate_nested_tool_batch<'run>(
    body_context: &ToolContext<'run>,
    dispatch: &Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    calls: Vec<crate::ToolInvocation>,
) -> Vec<crate::ToolInvocationReply> {
    // Shared across the calls: the cancellation trio the child's driver
    // computed from the recorded cancellation authority — carried on the body
    // context for this purpose. Deriving one per call from the scope alone
    // (`ScopedEffectController::turn_cancel_wait` always yields an observing
    // trio) would wire a child admitted with no cooperative authority to the
    // host's turn-cancel gate, so a signalled gate could cancel waits that
    // must ignore it.
    let turn_cancel_wait = body_context.turn_cancel_wait().cloned().unwrap_or_else(|| {
        crate::runtime::TurnCancelWait::unobserved(
            body_context
                .cancellation_token()
                .cloned()
                .unwrap_or_default(),
        )
    });
    let cancellation_token = body_context.cancellation_token().cloned();
    let enclosing_process = body_context
        .enclosing_process()
        .map(|process_id| ProcessId::from(process_id.to_string()));
    let parent_invocation = dispatch.parent_invocation.clone();
    // The body call these calls nest under — the parent link their
    // `ToolCallStarted`/`ToolCallCompleted` activities carry, the same
    // `batch_parent_call_id` a turn-dispatched batch stamps.
    let parent_call_id = body_context.tool_call_id.clone();
    // Where a refused nested call is kept for the child's driver, which
    // refuses the child with it (FIG-3679).
    let sinks = body_context.orchestrating_sinks.clone();

    let run_call = {
        let dispatch = Arc::clone(dispatch);
        move |(index, mut call): (usize, crate::ToolInvocation)| {
            let dispatch = Arc::clone(&dispatch);
            let sinks = sinks.clone();
            let turn_cancel_wait = turn_cancel_wait.clone();
            let cancellation_token = cancellation_token.clone();
            let enclosing_process = enclosing_process.clone();
            let parent_invocation = parent_invocation.clone();
            let parent_call_id = parent_call_id.clone();
            async move {
                let call_id = call.id.clone();
                // The nested call's own observation key: the body's dispatch
                // base is its child's parent invocation, so a body's repeated
                // `call_id`s — and calls nested under another keyed call —
                // still mint distinct observations (ADR 0105 §1).
                let dispatch = Arc::new(dispatch.observation_keyed(format!("{index}:{call_id}")));
                // Observation-only: the nested call's wall-clock window,
                // published on the Completed activity and never journaled.
                let call_started = dispatch.clock.now();
                let call_args = call.args.clone();
                let call_tool_id = call.tool_id.to_string();
                let activity_id = crate::session::tool_execution::tool_activity_id(&call_id);
                let (reply, tool_name) = 'reply: {
                    let grant = call.execution_grant.take();
                    let tool_name = match &grant {
                        Some(grant) => grant.manifest().name.clone(),
                        None => match crate::tool_dispatch::resolve_callable_manifest_by_id(
                            dispatch.as_ref(),
                            &call.tool_id,
                        ) {
                            Some(manifest) => manifest.name,
                            None => {
                                break 'reply (
                                    crate::ToolInvocationReply::from_output(
                                        crate::ToolCallOutput::failure(
                                            crate::ToolFailure::runtime(
                                                crate::ToolFailureClass::Unavailable,
                                                "tool_unavailable",
                                                format!(
                                                    "Tool id `{}` is unavailable in this session",
                                                    call.tool_id
                                                ),
                                            ),
                                        ),
                                    ),
                                    None,
                                );
                            }
                        },
                    };
                    // A body-driven call publishes the same stream-start and
                    // activity pair a turn-dispatched call emits, parented to
                    // the body that named it, through the dispatch's lent
                    // observation sink (ADR 0105 §1).
                    let mut cursor = dispatch.observation_cursor("nested:start");
                    cursor.observe(
                        dispatch.observer.as_ref(),
                        crate::engine::ObservedEvent::Session(
                            crate::SessionStreamEvent::ToolCallStart {
                                call_id: Some(call_id.clone()),
                                name: tool_name.clone(),
                                args: call_args.clone(),
                            },
                        ),
                    );
                    cursor.observe(
                        dispatch.observer.as_ref(),
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(activity_id.clone()),
                            event: crate::TurnEvent::ToolCallStarted {
                                call_id: Some(call_id.clone()),
                                name: tool_name.clone(),
                                args: call_args.clone(),
                                graph_key: None,
                                parent_call_id: parent_call_id.clone(),
                            },
                        },
                    );
                    let pending = crate::sansio::PendingToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
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
                        crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                            *prepared
                        }
                        crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                            break 'reply (dispatch_outcome_reply(*outcome), Some(tool_name));
                        }
                    };

                    let tool_context = ToolContext::from_dispatch(Arc::clone(&dispatch))
                        .prepared_call(&prepared)
                        .cancellation_token(cancellation_token)
                        .turn_cancel_wait(turn_cancel_wait.clone())
                        .enclosing_process(enclosing_process)
                        .parent_invocation(parent_invocation.clone())
                        .child_execution_trace_hook(call.child_execution_trace_hook.clone())
                        .build();

                    // An orchestrating registration runs its body inline — it
                    // declares no attempt frame (a group child's invocation driver
                    // routes it the same way), so a journaled attempt for it would
                    // park on a completion nobody resolves. A granted call names
                    // leaf authority by construction and cannot be orchestrating.
                    if grant.is_none() && dispatch.is_orchestrating_tool(&prepared.tool_id) {
                        // Boxed: the orchestrating body's future crosses
                        // clippy's large-future threshold.
                        let mut outcome =
                            Box::pin(crate::tool_dispatch::execute_orchestrating_tool(
                                dispatch.as_ref(),
                                prepared,
                                tool_context,
                            ))
                            .await;
                        for trigger in std::mem::take(&mut outcome.triggers) {
                            dispatch.trigger_outcomes.enqueue(trigger);
                        }
                        break 'reply (dispatch_outcome_reply(outcome), Some(tool_name));
                    }

                    let retry_policy = crate::tool_dispatch::resolve_retry_policy(
                        dispatch.as_ref(),
                        &prepared.tool_id,
                        grant.as_deref(),
                    );
                    let executor_dispatch = Arc::clone(&dispatch);
                    let executor_context = tool_context.clone();
                    // Boxed: the coordinated attempt's future crosses
                    // clippy's large-future threshold.
                    let coordinated = Box::pin(crate::tool_dispatch::coordinate_tool_invocation(
                        dispatch.as_ref(),
                        prepared,
                        grant,
                        retry_policy,
                        // A nested call is a live admission, not a retained fact:
                        // whether it may defer is read from the live registry,
                        // exactly as any live caller's would be.
                        None,
                        crate::tool_dispatch::ToolAttemptEffectIdentity::Scalar {
                            parent: parent_invocation,
                        },
                        &turn_cancel_wait,
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
                    // The journaled attempts' triggers ride the outcome; they
                    // belong to the body's own buffer, which its settlement
                    // carries.
                    (
                        match coordinated.launch {
                            crate::tool_dispatch::ToolCallLaunch::Done(mut outcome) => {
                                for trigger in std::mem::take(&mut outcome.triggers) {
                                    dispatch.trigger_outcomes.enqueue(trigger);
                                }
                                dispatch_outcome_reply(*outcome)
                            }
                            crate::tool_dispatch::ToolCallLaunch::Pending(pending) => {
                                match crate::runtime::effect::await_journaled_tool_completion(
                                    dispatch.as_ref(),
                                    dispatch.parent_invocation.as_ref(),
                                    &call_id,
                                    *pending,
                                    &turn_cancel_wait,
                                )
                                .await
                                {
                                    Ok(mut outcome) => {
                                        for trigger in std::mem::take(&mut outcome.triggers) {
                                            dispatch.trigger_outcomes.enqueue(trigger);
                                        }
                                        dispatch_outcome_reply(outcome)
                                    }
                                    Err(error) => refused_nested_reply(sinks.as_ref(), error),
                                }
                            }
                            crate::tool_dispatch::ToolCallLaunch::ControllerAborted(error) => {
                                refused_nested_reply(sinks.as_ref(), error)
                            }
                        },
                        Some(tool_name),
                    )
                };
                // The completion pair to the start emitted above: the record
                // when the call produced one, the reply's output otherwise.
                {
                    let duration_ms = dispatch
                        .clock
                        .now()
                        .duration_since(call_started)
                        .as_millis() as u64;
                    let completed = match reply.record.as_ref() {
                        Some(record) => crate::TurnEvent::ToolCallCompleted {
                            call_id: record.call_id.clone().or_else(|| Some(call_id.clone())),
                            name: record.tool.clone(),
                            args: record.args.clone(),
                            output: record.output.clone(),
                            duration_ms,
                            graph_key: None,
                            parent_call_id: parent_call_id.clone(),
                        },
                        None => crate::TurnEvent::ToolCallCompleted {
                            call_id: Some(call_id.clone()),
                            name: tool_name.unwrap_or(call_tool_id),
                            args: call_args,
                            output: reply.output.clone(),
                            duration_ms,
                            graph_key: None,
                            parent_call_id,
                        },
                    };
                    dispatch.observation_cursor("nested:complete").observe(
                        dispatch.observer.as_ref(),
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(activity_id),
                            event: completed,
                        },
                    );
                }
                reply
            }
        }
    };
    // The calls go to the body's controller as independent work, which runs
    // them concurrently where the order they commit in cannot be misread and
    // one at a time, in source order, where it can. Either way every call
    // runs to completion, and each reply lands in its call's own slot.
    let slots: Vec<std::sync::Mutex<Option<crate::ToolInvocationReply>>> =
        calls.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let work = calls
        .into_iter()
        .enumerate()
        .zip(&slots)
        .map(|((index, call), slot)| {
            let reply = run_call((index, call));
            Box::pin(async move {
                let reply = reply.await;
                *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply);
            }) as crate::IndependentEffectWork<'_>
        })
        .collect();
    dispatch
        .effect_controller
        .controller()
        .drive_independent_effect_work(work)
        .await;
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_else(|| {
                    crate::ToolInvocationReply::from_output(crate::ToolCallOutput::failure(
                        crate::ToolFailure::runtime(
                            crate::ToolFailureClass::Internal,
                            "nested_call_not_driven",
                            "the controller returned before driving this nested call",
                        ),
                    ))
                })
        })
        .collect()
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

    #[cfg(any(test, feature = "testing"))]
    pub fn new(implementation: Arc<dyn OrchestratingToolImplementation>) -> Self {
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
