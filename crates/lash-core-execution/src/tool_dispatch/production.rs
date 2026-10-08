//! Production A/X/D/V callbacks. Canonical preparation and body facts live
//! in their owning records; live maps carry invocation attribution and body stops.
use super::*;
use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use crate::tool_run::*;
use crate::{
    PreparedToolCall, RuntimeEffectControllerError, RuntimeExecutionContext, ToolCallOutput,
    ToolCallRecord, ToolIntents,
};
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

mod cell;
mod hooks;
mod leaf;
mod observations;
mod round;

pub use round::parked_call_output;
mod settlement;

pub use cell::{CellCall, CellHostCalls, CellMember, CellMembers, HostCall};

pub(crate) struct ProductionToolHandlers<'run> {
    context: RuntimeExecutionContext<'run>,
    environment: Option<crate::ProcessExecutionEnvRef>,
    calls: Mutex<BTreeMap<crate::ToolCallId, CallInput>>,
    prepared: Mutex<BTreeMap<crate::ToolCallId, Prepared>>,
    inline_stops: Mutex<BTreeMap<crate::ToolCallId, InlineStop>>,
    contributions: Mutex<BTreeMap<crate::ToolCallId, Vec<CheckContribution>>>,
    declarations: Mutex<BTreeMap<crate::ToolCallId, Vec<crate::ToolIntentExecutionOutcome>>>,
    /// The process each admitted isolated call's provider bound it to.
    isolated: Mutex<BTreeMap<crate::ToolCallId, IsolatedToolStart>>,
    /// The key of the completion wait a round member's round pinned for it.
    completion_key: Option<crate::PinnedKey>,
    /// Round members are traced by the round's fold, which holds their
    /// retry ladder; standalone calls observe their own terminal.
    traces_call: bool,
}

#[derive(Default)]
struct InlineStop {
    token: Option<tokio_util::sync::CancellationToken>,
    cancelled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct CallInput {
    attribution: crate::session::ToolObservationAttribution,
    definition: crate::ToolDefinition,
    pending: Option<Box<crate::sansio::PendingToolCall>>,
    grant: Option<Box<crate::ToolExecutionGrant>>,
    parent: Option<crate::RuntimeInvocation>,
    binding: AdmittedBinding,
    environment: crate::ProcessExecutionEnvRef,
    render: Option<crate::RecordedRender>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Prepared {
    input: CallInput,
    original_args: Option<serde_json::Value>,
    call: PreparedToolCall,
    failure: Option<crate::ToolFailure>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Captured {
    original: Option<ToolCallOutput>,
    output: ToolCallOutput,
    triggers: Vec<super::ToolTriggerEffectOutcome>,
    occurrence: crate::plugin::ToolHookOccurrence,
    intents: ToolIntents,
    /// The typed refusal of the start the call declared, which settled the
    /// call as this failure: reported as the call's intent outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_refusal: Option<crate::ToolIntentExecutionOutcome>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CheckContribution {
    plugin_id: String,
    events: Vec<crate::PluginRuntimeEvent>,
}

#[derive(Serialize, Deserialize)]
struct Presented {
    presentation: crate::runtime::effect::ToolPresentation,
    intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
}

fn stopped_before_completion() -> crate::ToolAttemptOutcome {
    crate::ToolOutcome::cancelled("the inline attempt stopped before its body completed").into()
}

/// Run an inline body to its own end; once `stop` fires it has the
/// budgets' `stop_grace` to observe its token and return its own outcome,
/// settling the nested work it owns, before it is dropped. Only a body that
/// ignores its stop meets this bound.
async fn until_stopped(
    execute: impl std::future::Future<Output = crate::ToolAttemptOutcome>,
    stop: &tokio_util::sync::CancellationToken,
    stop_grace: std::time::Duration,
) -> crate::ToolAttemptOutcome {
    tokio::pin!(execute);
    tokio::select! {
        biased;
        outcome = &mut execute => return outcome,
        () = stop.cancelled() => {}
    }
    tokio::time::timeout(stop_grace, execute)
        .await
        .unwrap_or_else(|_| stopped_before_completion())
}

fn encode<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| error.to_string())
}
fn cause<T: Serialize>(kind: &str, value: &T) -> HookCause {
    HookCause {
        error_type: kind.to_owned(),
        error_version: std::num::NonZeroU32::MIN,
        payload: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
    }
}

impl<'run> ProductionToolHandlers<'run> {
    pub(crate) fn new(
        context: RuntimeExecutionContext<'run>,
        environment: Option<crate::ProcessExecutionEnvRef>,
    ) -> Self {
        Self {
            context,
            environment,
            calls: Mutex::default(),
            prepared: Mutex::default(),
            inline_stops: Mutex::default(),
            contributions: Mutex::default(),
            declarations: Mutex::default(),
            isolated: Mutex::default(),
            completion_key: None,
            traces_call: true,
        }
    }
    /// Ask an isolated call's executable provider which registered engine
    /// runs it (D04). This selects data only: no preparation, hook or body
    /// runs. The start is keyed by the call's Run owner and id.
    pub(super) fn bind_isolated(
        &self,
        owner: &crate::EffectOpener,
        call_id: &crate::ToolCallId,
        tool_id: &crate::ToolId,
        args: &serde_json::Value,
        executable: &crate::plugin::PluginCallbackIdentity,
    ) -> Option<IsolatedToolStart> {
        let provider = self
            .context
            .dispatch()
            .plugins
            .resolve_context_tool_bindings(std::slice::from_ref(executable))
            .ok()?
            .pop()?;
        let binding = provider.isolated_process(crate::IsolatedProcessRequest {
            tool_id,
            call_id,
            args,
        })?;
        let provenance =
            owner
                .session_id()
                .map_or_else(crate::ProcessProvenance::host, |session_id| {
                    crate::ProcessProvenance::session(crate::SessionScope::new(session_id.clone()))
                });
        let registration = crate::ProcessStartRegistration::of_target(
            crate::ProcessInput::Engine {
                kind: binding.engine,
                payload: binding.payload,
            },
            provenance,
            crate::Lifetime::Detached,
        )
        .with_start_key(Some(
            crate::StartKeyDerivation::LASH_START_PATHS.for_isolated_call(owner, call_id),
        ));
        Some(IsolatedToolStart { registration })
    }
    fn cancel_inline_stop(&self, call_id: &crate::ToolCallId, accepted: bool) {
        let mut stops = self.inline_stops.lock_recover();
        let stop = if accepted {
            Some(stops.entry(call_id.clone()).or_default())
        } else {
            stops.get_mut(call_id)
        };
        if let Some(stop) = stop {
            stop.cancelled = true;
            if let Some(token) = &stop.token {
                token.cancel();
            }
        }
    }
    fn remember_inline_stop(
        &self,
        call_id: &crate::ToolCallId,
        token: Option<tokio_util::sync::CancellationToken>,
    ) {
        let mut stops = self.inline_stops.lock_recover();
        let stop = stops.entry(call_id.clone()).or_default();
        if stop.cancelled
            && let Some(token) = &token
        {
            token.cancel();
        }
        stop.token = token;
    }
    /// The true cause of a stop an inline body met, read inside X before its
    /// state or declarations can escape: the turn's accepted immediate stop,
    /// else the Run's own Closing cancel, else the execution's stop standing
    /// for its turn where there is no gate.
    async fn inline_stop_origin(
        &self,
        call_id: &crate::ToolCallId,
        stop: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Option<crate::CancelOrigin>, crate::RuntimeEffectControllerError> {
        if self.context.inline_turn_stop_requested().await? {
            return Ok(Some(crate::CancelOrigin::TurnStopped));
        }
        if self
            .inline_stops
            .lock_recover()
            .get(call_id)
            .is_some_and(|stop| stop.cancelled)
        {
            return Ok(Some(crate::CancelOrigin::RunClosing));
        }
        Ok(stop
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            .then_some(crate::CancelOrigin::TurnStopped))
    }
    async fn dispatch(&self, input: &CallInput) -> Result<ToolDispatchContext<'run>, String> {
        let mut dispatch = self.context.dispatch().as_ref().clone();
        dispatch.parent_invocation = input.parent.clone();
        dispatch.execution_env_spec = self
            .context
            .recorded_tool_run_env_spec(&input.environment)
            .await
            .map_err(|error| {
                let message = error.to_string();
                self.context.record_nested_effect_error(error.into());
                message
            })?;
        Ok(dispatch)
    }
    async fn capture_output(
        &self,
        prepared: Prepared,
        output: ToolCallOutput,
        occurrence: crate::plugin::ToolHookOccurrence,
        intents: ToolIntents,
        triggers: Vec<super::ToolTriggerEffectOutcome>,
    ) -> Result<Captured, String> {
        let dispatch = self.dispatch(&prepared.input).await?;
        let view = crate::plugin::PreparedCallReadView::new(prepared.call.clone());
        let hook = hooks::context(&dispatch, &prepared);
        let (original, control) = crate::plugin::ToolResultCandidate::split(output.clone());
        let transformed = dispatch
            .plugins
            .transform_tool_result(&hook, occurrence, &view, &Arc::new(original))
            .await;
        let final_output = match transformed {
            Ok(candidate) => candidate.into_output(control),
            Err(failure) => ToolCallOutput::failure(*failure),
        };
        let outcome = retry::normalized_outcome(
            &dispatch,
            &ToolCallIds::of(&prepared.call),
            prepared.call.tool_name.clone(),
            prepared.call.args.clone(),
            crate::ToolOutcome::from_output(final_output),
        )
        .await;
        Ok(Captured {
            original: (outcome.record.output != output).then_some(output),
            output: outcome.record.output,
            occurrence,
            intents,
            triggers,
            start_refusal: None,
        })
    }

    /// These handlers, handing their call the key of the completion wait
    /// its round pinned.
    pub(crate) fn with_completion_key(mut self, key: Option<crate::PinnedKey>) -> Self {
        self.completion_key = key;
        self
    }

    /// Stage the start a parked call declared, under its start key beneath
    /// the call's lineage, holding the child for the call: its launch
    /// receipt, and the rows that register it with the park.
    async fn launch_parked(
        &self,
        call_id: &crate::ToolCallId,
        start: &crate::DeclaredStart,
        cancels: bool,
    ) -> Result<
        (
            super::LaunchReceipt,
            Option<crate::runtime::actor::round::StoreLocalEffect>,
        ),
        String,
    > {
        let parent = self
            .context
            .language_runtime_invocation(&format!("run:start:{}", start.identity().replay_key));
        super::pending_resolver::launch_parked_start(
            self.context.dispatch().processes.as_ref(),
            self.context
                .process_scope_for_language_call(parent.into_runtime_invocation(), call_id),
            start,
            super::call_run::start_hold_key(call_id),
            cancels,
            None,
        )
        .await
        .map_err(|error| {
            self.context.record_nested_effect_error(error.clone());
            error.to_string()
        })
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for ProductionToolHandlers<'_> {
    fn execution_policy(&self, call: &SingletonToolCall) -> ExecutionPolicy {
        self.calls
            .lock_recover()
            .get(&call.call_id)
            .map_or(ExecutionPolicy::Once, |input| {
                input.definition.manifest.execution_policy
            })
    }

    fn cached_capture(&self, output: String) -> Result<SingletonCapture, String> {
        let capture: Captured = decode(&output)?;
        Ok(
            if matches!(capture.output.outcome, crate::ToolCallOutcome::Success(_)) {
                SingletonCapture::Done {
                    output,
                    commands: Vec::new(),
                    intents: Vec::new(),
                    stream: Default::default(),
                    start: None,
                }
            } else {
                SingletonCapture::Failed {
                    output,
                    stream: Default::default(),
                    suggested_delay_ms: None,
                }
            },
        )
    }
    fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
        Some(self.context.dispatch().plugins.clone())
    }
    fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        Some(&self.context.dispatch().process_engines)
    }
    fn isolated_start(&self, call: &SingletonToolCall) -> Option<IsolatedToolStart> {
        self.isolated.lock_recover().get(&call.call_id).cloned()
    }
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        let mut input = self
            .calls
            .lock_recover()
            .get(&call.call_id)
            .cloned()
            .ok_or("an admitted call has no preparation binding")?;
        let mut dispatch = self.dispatch(&input).await?;
        dispatch.tools = dispatch
            .plugins
            .resolve_context_tool_bindings(&[input.binding.preparation.clone()])
            .map_err(|error| {
                self.context
                    .record_nested_effect_error(error.clone().into());
                error.to_string()
            })?
            .remove(0);
        let mut pending =
            input
                .pending
                .as_deref()
                .cloned()
                .unwrap_or_else(|| crate::sansio::PendingToolCall {
                    call_id: call.call_id.clone(),
                    provider_call_id: None,
                    tool_name: call.tool_name.clone(),
                    args: call.arguments.clone(),
                    replay: None,
                });
        input.pending = None;
        let original_args = pending.args.clone();
        let hook = super::hooks::hook_context(
            &dispatch,
            &call.call_id,
            &input.definition.manifest.id,
            &call.tool_name,
            input.definition.manifest.argument_projection.clone(),
        );
        let mut failure = None;
        match dispatch
            .plugins
            .transform_tool_args(&hook, pending.args.clone())
            .await
        {
            Ok(args) => pending.args = args,
            Err(cause) => failure = Some(*cause),
        }
        if failure.is_none()
            && let Err(result) = preparation::validate_args(
                &input.definition.contract,
                &pending.args,
                "invalid_tool_args",
            )
            && let Some(ToolCallOutput {
                outcome: crate::ToolCallOutcome::Failure(cause),
                ..
            }) = result.as_done_output()
        {
            failure = Some(cause.clone());
        }
        let identity =
            PreparedToolCall::identity(input.definition.manifest.id.clone(), pending.clone());
        let prepared = if failure.is_some() {
            identity
        } else {
            match preparation::prepare_with_provider(
                &dispatch,
                &input.definition.manifest,
                &ToolCallIds::of_pending(&pending),
                input.grant.as_deref(),
                pending,
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(result) => {
                    if let Some(ToolCallOutput {
                        outcome: crate::ToolCallOutcome::Failure(cause),
                        ..
                    }) = result.as_done_output()
                    {
                        failure = Some(cause.clone());
                    }
                    identity
                }
            }
        };
        if failure.is_none()
            && let Err(result) = preparation::validate_args(
                &input.definition.contract,
                &prepared.args,
                "invalid_prepared_tool_args",
            )
            && let Some(ToolCallOutput {
                outcome: crate::ToolCallOutcome::Failure(cause),
                ..
            }) = result.as_done_output()
        {
            failure = Some(cause.clone());
        }
        let original_args = (original_args != prepared.args).then_some(original_args);
        let prepared = Prepared {
            input,
            original_args,
            call: prepared,
            failure,
        };
        let request = serde_json::to_value(&prepared).map_err(|error| error.to_string())?;
        // The admission this call runs under: what its decision, realization,
        // presentation and observations read until it ends.
        self.prepared
            .lock_recover()
            .insert(call.call_id.clone(), prepared);
        Ok(request)
    }
    async fn before_checks(
        &self,
        _call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        self.check_before(request).await
    }
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        let prepared: Prepared = serde_json::from_value(attempt.request.prepared.clone())
            .map_err(|error| error.to_string())?;
        let mut dispatch = self.dispatch(&prepared.input).await?;
        dispatch.observer = attempt.stream.clone();
        dispatch.trigger_outcomes = Default::default();
        dispatch.tools = dispatch
            .plugins
            .resolve_context_tool_bindings(std::slice::from_ref(&prepared.input.binding.executable))
            .map_err(|error| {
                self.context
                    .record_nested_effect_error(error.clone().into());
                error.to_string()
            })?
            .remove(0);
        let invocation: crate::RuntimeInvocation =
            ToolAttemptLineage::from_parent(prepared.input.parent.clone())
                .attempt_invocation(&dispatch, &prepared.call, attempt.attempt.get())
                .into();
        dispatch.parent_invocation = Some(invocation.clone());
        dispatch.observation_call_key = None;
        let effect_attempt = crate::EffectAttempt::default();
        dispatch.direct_completions = dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(invocation.clone())
            .with_effect_attempt(Some(effect_attempt.clone()));
        let dispatch = Arc::new(dispatch);
        // The attempt is the declaring attempt of any start it parks on: its
        // own invocation mints the identity that start is bound to (ADR 0116
        // §3.1).
        let declaring =
            super::intent_executor::declaring_identity(&dispatch, attempt.call_id, &invocation);
        let mut builder = crate::ToolContext::from_dispatch(dispatch.clone(), &prepared.call)
            .runtime_execution_context(
                self.context
                    .clone()
                    .with_execution_env_spec(dispatch.execution_env_spec.clone()),
            )
            // A dispatch a process owns runs its calls inside that process,
            // whether or not its execution context names it.
            .enclosing_process(
                self.context
                    .process_id()
                    .or(dispatch.owner.process_id())
                    .cloned(),
            );
        if let Some(process_id) = self.context.process_id()
            && let Some(events) = self.context.process_event_context()
        {
            builder = builder.inside_process(crate::ProcessToolCallWiring::new(
                process_id.clone(),
                events.execution_write_authority.clone(),
                events.process_work.clone(),
            ));
        }
        let mut context = builder
            .build()
            .with_attempt_dispatch(dispatch.clone(), invocation);
        context = context.with_prepared_payload(prepared.call.prepared_payload.clone());
        context.install_completion_key(self.completion_key.clone());
        let completion_context = context.clone();
        if let Some(grant) = &prepared.input.grant {
            context = context
                .with_tool_execution_binding(grant.execution_binding.clone())
                .with_granted_source_id(grant.source_id.clone());
        }
        let authority = match &prepared.input.grant {
            Some(grant) => atomic_attempt::AttemptAuthority::Granted(grant),
            None => atomic_attempt::AttemptAuthority::Catalog(Box::new(
                prepared.input.definition.manifest.clone(),
            )),
        };
        // The watch belongs inside X: replay consults X's recorded outcome,
        // rather than a live token when deciding whether A may publish state.
        let body = self.context.run_turn_step_body(|stop| {
            self.remember_inline_stop(attempt.call_id, stop.clone());
            let dispatch = &dispatch;
            let authority = &authority;
            let call = &prepared.call;
            let execution_policy = prepared.input.definition.manifest.execution_policy;
            let stop_grace = self
                .context
                .dispatch()
                .plugins
                .execution_budgets()
                .stop_grace();
            async move {
                if let Some(stop) = &stop {
                    context = context.with_step_stop(stop.clone());
                }
                let execute = Box::pin(retry::execute_leaf_tool_attempt(
                    dispatch,
                    authority,
                    call,
                    context,
                    attempt.attempt.get(),
                    execution_policy.max_attempts(),
                ));
                let outcome = match &stop {
                    Some(stop) if stop.is_cancelled() => stopped_before_completion(),
                    Some(stop) => until_stopped(execute, stop, stop_grace).await,
                    None => execute.await,
                };
                let origin = if matches!(&outcome, crate::ToolAttemptOutcome::HostFailed(_)) {
                    None
                } else {
                    self.inline_stop_origin(attempt.call_id, stop.as_ref())
                        .await?
                };
                Ok::<_, crate::RuntimeEffectControllerError>((outcome, origin))
            }
        });
        let (outcome, origin) = match futures_util::future::select(
            Box::pin(body),
            Box::pin(effect_attempt.attempt_faulted()),
        )
        .await
        {
            futures_util::future::Either::Left((Ok(outcome), _)) => outcome,
            futures_util::future::Either::Left((Err(error), _)) => {
                self.context.record_nested_effect_error(error.clone());
                return Err(error.to_string());
            }
            futures_util::future::Either::Right((error, _)) => {
                self.context.record_nested_effect_error(error.clone());
                return Err(error.to_string());
            }
        };
        if let Some(error) = effect_attempt.attempt_fault() {
            self.context.record_nested_effect_error(error.clone());
            return Err(error.to_string());
        }

        if let Some(origin) = origin {
            // A noncooperative body may return success after its durable stop.
            // Neither its state commands nor its intents can escape that stop.
            // A body that observed it keeps its own cancellation, under the
            // stop's true cause.
            let own = match outcome {
                crate::ToolAttemptOutcome::Done { result, .. } => {
                    match result.into_parts().0.outcome {
                        crate::ToolCallOutcome::Cancelled(cancellation) => Some(cancellation),
                        _ => None,
                    }
                }
                _ => None,
            };
            let mut cancellation = own.unwrap_or_else(|| {
                crate::ToolCancellation::runtime(match origin {
                    crate::CancelOrigin::RunClosing => {
                        "the owning Run closed during the tool attempt"
                    }
                    _ => "the turn stopped during the tool attempt",
                })
            });
            cancellation.source = crate::ToolFailureSource::Cancellation;
            let capture = Captured {
                original: None,
                output: ToolCallOutput::cancelled(cancellation.with_origin(origin)),
                triggers: Vec::new(),
                occurrence: crate::plugin::ToolHookOccurrence::Attempt {
                    attempt: attempt.attempt,
                },
                intents: ToolIntents::default(),
                start_refusal: None,
            };
            return Ok(SingletonBodyOutcome::Cancelled {
                evidence: Some(encode(&capture)?),
            });
        }

        match outcome {
            // A retryable attempt fault took no effect: the attempt runs
            // again, and nothing of it reaches the enclosing execution.
            crate::ToolAttemptOutcome::HostFailed(error) if error.is_attempt_fault() => {
                Ok(SingletonBodyOutcome::Faulted(error))
            }
            crate::ToolAttemptOutcome::HostFailed(error) => {
                self.context.record_nested_effect_error(*error.clone());
                Err(error.to_string())
            }
            crate::ToolAttemptOutcome::Pending(pending) => {
                let refusal = prepared
                    .input
                    .definition
                    .manifest
                    .declaration
                    .admits(OutcomeShape::Deferred)
                    .and_then(|()| {
                        if matches!(
                            pending.resolved_by,
                            Some(crate::PendingResolver::DeclaredStart(_))
                        ) {
                            prepared.input.definition.manifest.declaration.admits(
                                OutcomeShape::Done {
                                    intents: &[crate::ToolIntentKind::StartProcess],
                                },
                            )
                        } else {
                            Ok(())
                        }
                    })
                    .err();
                if let Some(refusal) = refusal {
                    let failure = crate::ToolFailure::runtime(
                        crate::ToolFailureClass::Internal,
                        "tool_outcome_not_declared",
                        refusal.to_string(),
                    )
                    .with_cause(crate::ToolFailureCause::Declaration { refusal });
                    let capture = self
                        .capture_output(
                            prepared,
                            ToolCallOutput::failure(failure),
                            crate::plugin::ToolHookOccurrence::Attempt {
                                attempt: attempt.attempt,
                            },
                            ToolIntents::default(),
                            dispatch.trigger_outcomes.drain(),
                        )
                        .await?;
                    Ok(SingletonBodyOutcome::Failed {
                        output: encode(&capture)?,
                        suggested_delay_ms: None,
                    })
                } else if let Some(crate::PendingResolver::DeclaredStart(start)) =
                    &pending.resolved_by
                    && let Err(refusal) = start.bound_to(&declaring)
                {
                    // A declared start decodes without its constructor, so
                    // its bytes may name another session, another call or a
                    // nonzero index. It is refused before anything of it is
                    // admitted or registered, and settles the call.
                    let outcome =
                        super::pending_resolver::unbound_declaration(&declaring, refusal).outcome;
                    let mut capture = self
                        .capture_output(
                            prepared,
                            ToolCallOutput::failure(super::pending_resolver::launch_refusal(
                                &outcome,
                            )),
                            crate::plugin::ToolHookOccurrence::Attempt {
                                attempt: attempt.attempt,
                            },
                            ToolIntents::default(),
                            dispatch.trigger_outcomes.drain(),
                        )
                        .await?;
                    capture.start_refusal = Some(outcome);
                    Ok(SingletonBodyOutcome::Failed {
                        output: encode(&capture)?,
                        suggested_delay_ms: None,
                    })
                } else if completion_context.take_completion_key().is_none() {
                    let capture = self
                        .capture_output(
                            prepared,
                            ToolCallOutput::failure(crate::ToolFailure::runtime(
                                crate::ToolFailureClass::Internal,
                                "pending_tool_missing_completion_key",
                                "tool returned Pending without obtaining its completion key",
                            )),
                            crate::plugin::ToolHookOccurrence::Attempt {
                                attempt: attempt.attempt,
                            },
                            ToolIntents::default(),
                            dispatch.trigger_outcomes.drain(),
                        )
                        .await?;
                    Ok(SingletonBodyOutcome::Failed {
                        output: encode(&capture)?,
                        suggested_delay_ms: None,
                    })
                } else {
                    let pending = match super::atomic_attempt::announce_pending_park(
                        &completion_context,
                        pending,
                    )
                    .await
                    {
                        Ok(pending) => pending,
                        Err(failure) => {
                            let capture = self
                                .capture_output(
                                    prepared,
                                    ToolCallOutput::failure(*failure),
                                    crate::plugin::ToolHookOccurrence::Attempt {
                                        attempt: attempt.attempt,
                                    },
                                    ToolIntents::default(),
                                    dispatch.trigger_outcomes.drain(),
                                )
                                .await?;
                            return Ok(SingletonBodyOutcome::Failed {
                                output: encode(&capture)?,
                                suggested_delay_ms: None,
                            });
                        }
                    };
                    let mut store_local = Vec::new();
                    let launch = match &pending.resolved_by {
                        Some(crate::PendingResolver::DeclaredStart(start)) => {
                            let (receipt, effect) = self
                                .launch_parked(
                                    attempt.call_id,
                                    start,
                                    pending.on_cancel == crate::CancelHint::CancelExternalWork,
                                )
                                .await?;
                            if receipt.process_id.is_none() {
                                // A refused start settles the call: there is
                                // no child whose terminal it could await.
                                let mut capture = self
                                    .capture_output(
                                        prepared,
                                        ToolCallOutput::failure(
                                            super::pending_resolver::launch_refusal(
                                                &receipt.outcome,
                                            ),
                                        ),
                                        crate::plugin::ToolHookOccurrence::Attempt {
                                            attempt: attempt.attempt,
                                        },
                                        ToolIntents::default(),
                                        dispatch.trigger_outcomes.drain(),
                                    )
                                    .await?;
                                capture.start_refusal = Some(receipt.outcome);
                                return Ok(SingletonBodyOutcome::Failed {
                                    output: encode(&capture)?,
                                    suggested_delay_ms: None,
                                });
                            }
                            store_local.extend(effect);
                            Some(Box::new(receipt))
                        }
                        Some(crate::PendingResolver::ProcessTerminal { .. }) | None => None,
                    };
                    Ok(SingletonBodyOutcome::Pending {
                        completion: Box::new(pending),
                        launch,
                        store_local,
                    })
                }
            }
            crate::ToolAttemptOutcome::Done { result, intents } => {
                let (mut output, mut commands) = result.into_parts();
                let mut intents = intents;
                if let Err(refusal) =
                    prepared
                        .input
                        .definition
                        .manifest
                        .declaration
                        .admits(OutcomeShape::Done {
                            intents: &intents
                                .intents
                                .iter()
                                .map(crate::ToolIntent::kind)
                                .collect::<Vec<_>>(),
                        })
                {
                    output = ToolCallOutput::failure(
                        crate::ToolFailure::runtime(
                            crate::ToolFailureClass::Internal,
                            "tool_outcome_not_declared",
                            refusal.to_string(),
                        )
                        .with_cause(crate::ToolFailureCause::Declaration { refusal }),
                    );
                    commands = Default::default();
                    intents = Default::default();
                }
                let captured = self
                    .capture_output(
                        prepared,
                        output,
                        crate::plugin::ToolHookOccurrence::Attempt {
                            attempt: attempt.attempt,
                        },
                        intents,
                        dispatch.trigger_outcomes.drain(),
                    )
                    .await?;
                let output = encode(&captured)?;
                match &captured.output.outcome {
                    crate::ToolCallOutcome::Success(_) => Ok(SingletonBodyOutcome::Done {
                        output,
                        commands,
                        intents: captured
                            .intents
                            .intents
                            .iter()
                            .map(crate::ToolIntent::kind)
                            .collect(),
                        start: None,
                    }),
                    crate::ToolCallOutcome::Failure(failure) => match failure.cause.as_deref() {
                        Some(crate::ToolFailureCause::Interrupted) => {
                            Ok(SingletonBodyOutcome::Interrupted)
                        }
                        Some(crate::ToolFailureCause::ExecutionLimit { cause }) => {
                            Ok(SingletonBodyOutcome::TimedOut {
                                cause: *cause,
                                evidence: Some(output),
                            })
                        }
                        _ => Ok(SingletonBodyOutcome::Failed {
                            output,
                            suggested_delay_ms: failure.suggested_delay_ms,
                        }),
                    },
                    crate::ToolCallOutcome::Cancelled(_) => Ok(SingletonBodyOutcome::Cancelled {
                        evidence: Some(output),
                    }),
                }
            }
        }
    }
    async fn after_checks(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        self.check_after(call_id, capture).await
    }
    fn observe_terminal(
        &self,
        call_id: &crate::ToolCallId,
        decision: &CallDecision,
        cause: Option<&AttributedVerdict<HookCause>>,
        capture: Option<&SingletonCapture>,
        presentation: Option<&str>,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.inline_stops.lock_recover().remove(call_id);
        let record = self
            .observed_record(call_id, decision, cause, capture, presentation)
            .map_err(|message| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RecordEncodingFailed,
                    message,
                )
            })?;
        let context = self.context.with_tool_observation_attribution(
            &self
                .prepared
                .lock_recover()
                .get(call_id)
                .ok_or_else(|| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RecordEncodingFailed,
                        "the observed call has no admitted preparation",
                    )
                })?
                .input
                .attribution,
        );
        // The presented record retains the call's realized intents.
        if let Some(presented) = presentation.and_then(|text| decode::<Presented>(text).ok()) {
            context.emit_tool_intent_outcome_activities(
                call_id.as_str(),
                call_id,
                &presented.intent_outcomes,
            );
        }
        if self.traces_call {
            context.trace_tool_call_completed(&record, &[]);
        }
        context.emit_tool_call_completed_activity(call_id.as_str(), &record, 0);
        Ok(())
    }
    fn observe_started(&self, _call_id: &crate::ToolCallId, request: &SingletonPreparedRequest) {
        if let Err(error) = self.started_observation(request) {
            self.context.record_nested_effect_error(error);
        }
    }

    fn incorporate(
        &self,
        call_id: &crate::ToolCallId,
        capture: Option<&SingletonCapture>,
        presentation: Option<&str>,
        observe: bool,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.incorporate_capture(call_id, capture, presentation, observe)
    }
    async fn cancel_call(&self, call_id: &crate::ToolCallId) -> Result<(), String> {
        self.cancel_inline_stop(call_id, false);
        Ok(())
    }
    async fn realize(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<super::Realization, RuntimeEffectControllerError> {
        let shape = |message: String| {
            RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RuntimeToolRunShape, message)
        };
        let captured: Captured = decode(
            capture
                .output()
                .ok_or_else(|| shape("final has no canonical output".into()))?,
        )
        .map_err(shape)?;
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or_else(|| shape("the final has no admitted preparation".into()))?;
        let mut dispatch = self.context.dispatch().as_ref().clone();
        dispatch.execution_env_spec = self
            .context
            .recorded_tool_run_env_spec(&prepared.input.environment)
            .await?;
        let attempt = match captured.occurrence {
            crate::plugin::ToolHookOccurrence::Attempt { attempt }
            | crate::plugin::ToolHookOccurrence::DeferredCompletion { attempt } => attempt,
            crate::plugin::ToolHookOccurrence::Admission
            | crate::plugin::ToolHookOccurrence::Cached => {
                return Err(shape("tool declarations have no attempt".into()));
            }
        };
        dispatch.parent_invocation = Some(
            ToolAttemptLineage::from_parent(prepared.input.parent.clone())
                .attempt_invocation(&dispatch, &prepared.call, attempt.get())
                .into(),
        );
        super::execute_final_tool_intents(
            &dispatch.intent_realization_context(),
            call_id,
            &captured.intents,
            None,
        )
        .await
    }
    async fn commit_store_local(
        &self,
        effects: Vec<crate::runtime::actor::round::StoreLocalEffect>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.context
            .dispatch()
            .effect_controller
            .commit_store_local(effects)
            .await
    }
    fn adopt_realization(
        &self,
        call_id: &crate::ToolCallId,
        receipt: &RealizationReceipt,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.declarations
            .lock_recover()
            .insert(call_id.clone(), receipt.outcomes.clone());
        Ok(())
    }
    async fn present(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        self.present_capture(call_id, capture).await
    }
    fn emit_stream(
        &self,
        call_id: &crate::ToolCallId,
        stream: &crate::runtime::effect::AttemptStream,
    ) {
        let (events, _) = stream.decode();
        let mut cursor = self
            .context
            .dispatch()
            .observation_cursor(&format!("run:{call_id}:stream"));
        for event in events {
            match event {
                crate::runtime::effect::DecodedStreamEvent::Session(event) => cursor.observe(
                    self.context.dispatch().observer.as_ref(),
                    crate::engine::ObservedEvent::RecordedSession(event),
                ),
                crate::runtime::effect::DecodedStreamEvent::Activity(event) => cursor.observe(
                    self.context.dispatch().observer.as_ref(),
                    crate::engine::ObservedEvent::RecordedActivity(event),
                ),
            }
        }
    }
    async fn stage_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<StartLaunch, String> {
        let parent = self
            .context
            .language_runtime_invocation(&format!("run:start:{}", obligation.start_key()));
        // The start registers with the call's outcome, so no hold is left
        // for the call to release.
        let mut registration = obligation.registration.clone();
        registration.consumer_hold = None;
        let staged = self
            .context
            .dispatch()
            .processes
            .stage_bound(
                registration,
                self.context.process_scope_for_language_call(
                    parent.into_runtime_invocation(),
                    &obligation.call_id,
                ),
            )
            .await;
        match staged {
            Ok(staged) => Ok(StartLaunch::Staged {
                handle: crate::ProcessHandleView::from_record(staged.record),
                effect: staged
                    .rows
                    .map(crate::runtime::actor::round::StoreLocalEffect::ProcessStart),
            }),
            // A terminal refusal, such as a closed starter scope's, is the
            // start's own: a retry would only meet it again (ADR 0116 §3.2).
            Err(error) if super::intent_executor::declared_start_fault(&error).is_none() => Ok(
                StartLaunch::Refused(crate::ToolIntentRefusalReason::CommandFailed {
                    cause: crate::ToolIntentCommandFailure::from(&error),
                }),
            ),
            Err(error) => {
                self.context
                    .record_nested_effect_error(error.clone().into());
                Err(error.to_string())
            }
        }
    }
}
