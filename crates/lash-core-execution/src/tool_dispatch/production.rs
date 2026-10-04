//! Production A/X/D/V callbacks. Canonical preparation and body facts live
//! in their owning records; live maps carry only invocation attribution.
use super::*;
use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use crate::session::tool_execution::{ToolAggregateOutcome, ToolAggregateRequest};
use crate::tool_run::*;
use crate::{
    PreparedToolCall, RuntimeExecutionContext, ToolCallOutput, ToolCallRecord, ToolIntents,
};
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

mod aggregate;
mod hooks;
mod settlement;

pub(crate) struct ProductionToolHandlers<'run> {
    context: RuntimeExecutionContext<'run>,
    materials: Option<Arc<dyn crate::store::ToolMaterialStore>>,
    environment: Option<crate::ProcessExecutionEnvRef>,
    calls: Mutex<BTreeMap<crate::ToolCallId, CallInput>>,
    prepared: Mutex<BTreeMap<crate::ToolCallId, Prepared>>,
    pending: Mutex<BTreeMap<crate::ToolCallId, crate::PendingCompletion>>,
    contributions: Mutex<BTreeMap<crate::ToolCallId, Vec<CheckContribution>>>,
    declarations: Mutex<BTreeMap<crate::ToolCallId, Vec<crate::ToolIntentExecutionOutcome>>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CallInput {
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
    messages: Vec<crate::PluginMessage>,
    triggers: Vec<super::ToolTriggerEffectOutcome>,
    occurrence: crate::plugin::ToolHookOccurrence,
    intents: ToolIntents,
}

#[derive(Clone, Serialize, Deserialize)]
struct CheckContribution {
    plugin_id: String,
    messages: Vec<crate::PluginMessage>,
    events: Vec<crate::PluginRuntimeEvent>,
}

#[derive(Serialize, Deserialize)]
struct Presented {
    presentation: crate::runtime::effect::ToolPresentation,
    intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
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
        materials: Option<Arc<dyn crate::store::ToolMaterialStore>>,
        environment: Option<crate::ProcessExecutionEnvRef>,
    ) -> Self {
        Self {
            context,
            materials,
            environment,
            calls: Mutex::default(),
            prepared: Mutex::default(),
            pending: Mutex::default(),
            contributions: Mutex::default(),
            declarations: Mutex::default(),
        }
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
        messages: Vec<crate::PluginMessage>,
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
            messages,
            triggers,
        })
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for ProductionToolHandlers<'_> {
    fn restore_request(
        &self,
        call_id: &crate::ToolCallId,
        binding: &AdmittedBinding,
        request: &SingletonPreparedRequest,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let prepared: Prepared =
            serde_json::from_value(request.prepared.clone()).map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RecordEncodingFailed,
                    error.to_string(),
                )
            })?;
        if &prepared.input.binding != binding || &prepared.call.call_id != call_id {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::EffectReplayDivergence,
                "the recorded preparation names a different callback or call",
            ));
        }
        self.context
            .dispatch()
            .plugins
            .resolve_context_tool_bindings(&[
                binding.executable.clone(),
                binding.preparation.clone(),
            ])
            .map_err(crate::RuntimeEffectControllerError::from)?;
        self.context
            .dispatch()
            .plugins
            .validate_tool_presentation_plan(&binding.presentation)
            .map_err(crate::RuntimeEffectControllerError::from)?;
        self.calls
            .lock_recover()
            .insert(call_id.clone(), prepared.input.clone());
        self.prepared
            .lock_recover()
            .insert(call_id.clone(), prepared);
        Ok(())
    }
    fn retry_policy(
        &self,
        call: &SingletonToolCall,
        _default: RecordedRetryPolicy,
    ) -> RecordedRetryPolicy {
        match self
            .calls
            .lock_recover()
            .get(&call.call_id)
            .map(|input| input.definition.manifest.retry_policy)
        {
            Some(crate::ToolRetryPolicy::Safe {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            }) => RecordedRetryPolicy::Reported {
                max_attempts: std::num::NonZeroU32::new(max_attempts)
                    .unwrap_or(std::num::NonZeroU32::MIN),
                base_delay_ms,
                max_delay_ms,
            },
            _ => RecordedRetryPolicy::Never,
        }
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
                }
            },
        )
    }
    fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
        Some(self.context.dispatch().plugins.clone())
    }
    fn tool_material_store(&self) -> Option<&dyn crate::store::ToolMaterialStore> {
        self.materials.as_deref()
    }
    fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        Some(&self.context.dispatch().process_engines)
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
        serde_json::to_value(Prepared {
            input,
            original_args,
            call: prepared,
            failure,
        })
        .map_err(|error| error.to_string())
    }
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        self.check_before(call, request).await
    }
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        let prepared: Prepared = serde_json::from_value(attempt.request.prepared.clone())
            .map_err(|error| error.to_string())?;
        let mut dispatch = self.dispatch(&prepared.input).await?;
        dispatch.observer = attempt.stream.clone();
        dispatch.checkpoint_messages = Default::default();
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
        let mut context = crate::ToolContext::from_dispatch(dispatch.clone(), &prepared.call)
            .runtime_execution_context(
                self.context
                    .clone()
                    .with_execution_env_spec(dispatch.execution_env_spec.clone()),
            )
            .build()
            .with_attempt_dispatch(dispatch.clone(), invocation);
        context.install_prederived_completion_key(attempt.completion_key.cloned());
        context = context.with_prepared_payload(prepared.call.prepared_payload.clone());
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
        let body = retry::execute_leaf_tool_attempt(
            &dispatch,
            &authority,
            &prepared.call,
            context,
            attempt.attempt.get(),
            match prepared.input.definition.manifest.retry_policy {
                crate::ToolRetryPolicy::Never => 1,
                crate::ToolRetryPolicy::Safe { max_attempts, .. } => max_attempts,
            },
        );
        let outcome = match futures_util::future::select(
            Box::pin(body),
            Box::pin(effect_attempt.attempt_faulted()),
        )
        .await
        {
            futures_util::future::Either::Left((outcome, _)) => outcome,
            futures_util::future::Either::Right((error, _)) => {
                self.context.record_nested_effect_error(error.clone());
                return Err(error.to_string());
            }
        };
        if let Some(error) = effect_attempt.attempt_fault() {
            self.context.record_nested_effect_error(error.clone());
            return Err(error.to_string());
        }

        match outcome {
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
                            dispatch.checkpoint_messages.drain(),
                            dispatch.trigger_outcomes.drain(),
                        )
                        .await?;
                    Ok(SingletonBodyOutcome::Failed {
                        output: encode(&capture)?,
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
                            dispatch.checkpoint_messages.drain(),
                            dispatch.trigger_outcomes.drain(),
                        )
                        .await?;
                    Ok(SingletonBodyOutcome::Failed {
                        output: encode(&capture)?,
                    })
                } else {
                    Ok(SingletonBodyOutcome::Pending {
                        completion: Box::new(pending),
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
                        dispatch.checkpoint_messages.drain(),
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
                    crate::ToolCallOutcome::Failure(failure)
                        if matches!(failure.retry, crate::ToolRetryStatus::Safe { .. }) =>
                    {
                        let crate::ToolRetryStatus::Safe { after_ms } = failure.retry else {
                            unreachable!()
                        };
                        Ok(SingletonBodyOutcome::RetryableFailure { output, after_ms })
                    }
                    _ => Ok(SingletonBodyOutcome::Failed { output }),
                }
            }
        }
    }
    async fn arm_pending(
        &self,
        source: &SourceDescriptor,
        completion: &crate::PendingCompletion,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.pending
            .lock_recover()
            .insert(source.call_id.clone(), completion.clone());
        if let Some(announcement) = &completion.announcement {
            self.context
                .append_process_events(vec![announcement.clone().into_append_request()])
                .await
                .map_err(crate::RuntimeEffectControllerError::from)?;
        }
        if let SourceAuthority::ProcessTerminal { process_id } = &source.authority {
            self.attach_start_terminal(source, process_id).await?;
        }
        Ok(())
    }
    fn finalizes_source(&self) -> bool {
        true
    }
    async fn finalize_source(
        &self,
        call_id: &crate::ToolCallId,
        attempt: AttemptOrdinal,
        capture: &SingletonCapture,
        completion: Option<&crate::PendingCompletion>,
    ) -> Result<SingletonCapture, String> {
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or("the source final has no admitted preparation")?;
        let output = capture
            .output()
            .ok_or("the source has no canonical result")?;
        let resolution = match capture {
            SingletonCapture::Done { .. } => crate::Resolution::Ok(
                serde_json::from_str(output).map_err(|error| error.to_string())?,
            ),
            SingletonCapture::Failed { .. } | SingletonCapture::RetryableFailure { .. } => {
                serde_json::from_str(output).map_err(|error| error.to_string())?
            }
            _ => return Err("the source did not supply a completed result".to_owned()),
        };
        let output = crate::tool_result::tool_output_from_completion_resolution(
            resolution,
            completion.and_then(|completion| completion.resolved_by.as_ref()),
        );
        let captured = self
            .capture_output(
                prepared,
                output,
                crate::plugin::ToolHookOccurrence::DeferredCompletion { attempt },
                ToolIntents::default(),
                Vec::new(),
                Vec::new(),
            )
            .await?;
        let success = matches!(captured.output.outcome, crate::ToolCallOutcome::Success(_));
        let output = encode(&captured)?;
        Ok(if success {
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
            }
        })
    }
    async fn after_checks(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        self.check_after(call_id, capture).await
    }
    fn decision_contributions(
        &self,
        call_id: &crate::ToolCallId,
    ) -> Result<Option<String>, String> {
        self.contributions
            .lock_recover()
            .get(call_id)
            .filter(|values| !values.is_empty())
            .map(encode)
            .transpose()
    }
    fn restore_decision_contributions(
        &self,
        call_id: &crate::ToolCallId,
        text: &str,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let contributions = decode(text).map_err(|message| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                message,
            )
        })?;
        self.contributions
            .lock_recover()
            .insert(call_id.clone(), contributions);
        Ok(())
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
    fn run_cancel_requested(&self) -> bool {
        self.context.is_cancelled()
    }
    async fn cancel_call(
        &self,
        call_id: &crate::ToolCallId,
        _source: Option<&crate::AwaitEventKey>,
    ) -> Result<(), String> {
        let pending = self.pending.lock_recover().get(call_id).cloned();
        if let Some(crate::PendingCompletion {
            on_cancel: crate::CancelHint::CancelExternalWork,
            resolved_by: Some(crate::PendingResolver::ProcessTerminal { process_id }),
            ..
        }) = pending
        {
            self.context
                .dispatch()
                .processes
                .cancel(
                    &self.context.dispatch().owner.runtime_owner(),
                    &process_id,
                    self.context.process_scope(None),
                )
                .await
                .map_err(|error| {
                    self.context
                        .record_nested_effect_error(error.clone().into());
                    error.to_string()
                })?;
        }
        Ok(())
    }
    async fn realize_declarations(
        &self,
        _call_id: &crate::ToolCallId,
        _intents: &[crate::ToolIntentKind],
    ) -> Result<(), String> {
        Err("declarations need their canonical capture".to_owned())
    }
    async fn realize_capture(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<(), String> {
        let captured: Captured = decode(capture.output().ok_or("final has no canonical output")?)?;
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or("the final has no admitted preparation")?;
        let outcomes = intent_executor::execute_final_tool_intents(
            &self.dispatch(&prepared.input).await?,
            call_id,
            &captured.intents,
            None,
        )
        .await
        .map_err(|error| error.to_string())?;
        self.declarations
            .lock_recover()
            .insert(call_id.clone(), outcomes);
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
        let (events, _) = stream.decode(&serde_json::Value::Null);
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
    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<crate::ProcessId, String> {
        let invocation = self
            .context
            .language_runtime_invocation(&format!("run:start:{}", obligation.start_key()));
        let outcome = self
            .context
            .dispatch()
            .effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                        registration: obligation.registration.clone(),
                        observers: Vec::new(),
                        execution_context: Box::default(),
                    }),
                ),
                crate::RuntimeEffectLocalExecutor::unavailable(),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_process)
            .map_err(|error| {
                self.context.record_nested_effect_error(error.clone());
                error.to_string()
            })?;
        match outcome {
            crate::ProcessEffectOutcome::Start { record, .. } => Ok(record.id),
            _ => Err("the start command returned a different process outcome".to_owned()),
        }
    }

    async fn attach_start_terminal(
        &self,
        source: &SourceDescriptor,
        process_id: &crate::ProcessId,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.context
            .dispatch()
            .processes
            .attach_process_terminal(process_id, &source.source, self.context.process_scope(None))
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?;
        Ok(())
    }
    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process_id: &crate::ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        if cancel {
            self.context
                .dispatch()
                .processes
                .cancel(
                    &self.context.dispatch().owner.runtime_owner(),
                    process_id,
                    self.context.process_scope(None),
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        let hold = obligation
            .registration
            .consumer_hold
            .as_ref()
            .ok_or("declared start has no consumer hold")?;
        self.context
            .dispatch()
            .processes
            .release_consumer_hold(process_id, &hold.key)
            .await
            .map_err(|error| error.to_string())
    }
}
