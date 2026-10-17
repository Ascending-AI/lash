//! A turn's tool round on the production tools: each member's attempt runs
//! the call's admission, its attempt and its decision in memory, between
//! the `x_start` and the `x_outcome` its round commits, and hands the round
//! the store-local effects its realization staged, which commit with that
//! `x_outcome` (ADR 0132 §5).
use super::*;
use crate::runtime::actor::round::{
    AcceptedStop, AdmittedExecution, CompletedCall, Discharge, MemberBody, MemberOutcome,
    MemberPin, PolicyView, Presented, RoundTools, StoreLocalEffect, completed_material,
    decode_completed,
};
use crate::runtime::actor::waits::{self, Resolution};
use crate::session::tool_execution::ToolInvocation;
use crate::tool_dispatch::call_run::{AdmittedToolCall, AttemptEnd, CallEnd};
use crate::tool_run::{
    AvailableEvidence, CompletionSource, KnownFailureReason, MaterialOwner, MaterialRole,
};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;

/// How one attempt of a round member ended.
pub(super) enum MemberEnd {
    /// The call ended with this answer.
    Final(CompletedCall),
    /// The attempt reported a failure its policy repeats.
    Retry {
        failure: CompletedCall,
        suggested_delay_ms: Option<u64>,
    },
    /// The turn's cancel withheld the call's result.
    Cancelled,
    /// The body parked on its completion wait.
    Parked(ParkedCall),
    /// The attempt ended in a retryable attempt fault: it took no effect,
    /// and [`member_body`] runs it again.
    Faulted(Box<crate::RuntimeEffectControllerError>),
}

/// A parked call as its `Waiting` outcome records it: its pending
/// completion, and the launch receipt of the start it declared to resolve
/// it. What its resolution is answered from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ParkedCall {
    completion: crate::PendingCompletion,
    launch: Option<super::super::LaunchReceipt>,
    /// The declaration the call was admitted under: what its resolution
    /// settles against, whatever the session's catalog has become since.
    declaration: crate::ToolDeclaration,
    /// The output schema of the contract the call was prepared under, which
    /// its resolution's success value settles against.
    output_schema: crate::JsonSchema,
}

impl ParkedCall {
    /// The process whose terminal resolves the call, when the runtime owns
    /// one.
    fn awaited_process(&self) -> Option<&crate::ProcessId> {
        match self.completion.resolved_by.as_ref()? {
            crate::PendingResolver::ProcessTerminal { process_id } => Some(process_id),
            crate::PendingResolver::DeclaredStart(_) => self
                .launch
                .as_ref()
                .and_then(|launch| launch.process_id.as_ref()),
        }
    }
}

pub(super) fn answered(
    call: &crate::sansio::PendingToolCall,
    output: ToolCallOutput,
) -> CompletedCall {
    CompletedCall {
        call_id: call.call_id.clone(),
        provider_call_id: call.provider_call_id.clone(),
        tool_name: call.tool_name.clone(),
        args: call.args.clone(),
        model_return: crate::ModelToolReturn::from_output(call.tool_name.clone(), &output),
        output,
        intent_outcomes: Vec::new(),
        replay: call.replay.clone(),
    }
}

fn unavailable() -> ToolCallOutput {
    ToolCallOutput::failure(crate::ToolFailure::runtime(
        crate::ToolFailureClass::InvalidRequest,
        "tool_unavailable",
        "Tool is unavailable in this session",
    ))
}

/// A parked member's answer: `Waiting` on the completion wait its round
/// pinned, the parked call riding as its material, and the process whose
/// terminal its runner pins a wait on beside it.
fn parked_output(
    call: &crate::sansio::PendingToolCall,
    owner: &crate::EffectOpener,
    execution: &AdmittedExecution,
    parked: &ParkedCall,
) -> MemberOutcome {
    let encoded = execution.draft().pinned_wait().and_then(|pinned| {
        let text = serde_json::to_string(parked).ok()?;
        Some(
            Material::journal_local(
                MaterialOwner::Run {
                    opener: owner.clone(),
                },
                MaterialRole::AttemptOutput,
                text,
            )
            .parked(pinned.id.to_hex()),
        )
    });
    // A call whose round pinned no wait never had a key to take, so it
    // cannot park; nor can one whose park does not encode.
    let Some(source) = encoded else {
        return member_output(
            owner,
            MemberEnd::Final(answered(
                call,
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "pending_tool_missing_completion_key",
                    "tool returned Pending without a completion wait to park on",
                )),
            )),
        )
        .into();
    };
    MemberOutcome {
        output: SettledOutput::Waiting(source),
        store_local: Vec::new(),
        terminal: parked.awaited_process().cloned(),
        capacity: None,
        candidate: None,
    }
}

/// The tool output of a call that parked as `parked`, once one of its waits
/// ended with `resolution`: a pure function of the two. A process's catalog
/// tool step that parked settles from it.
pub fn parked_call_output(
    parked: &Material<CompletionSource>,
    resolution: Resolution,
) -> ToolCallOutput {
    let parked = serde_json::from_str::<ParkedCall>(parked.payload()).ok();
    crate::tool_result::tool_output_from_completion_resolution(
        resolution,
        parked
            .as_ref()
            .and_then(|parked| parked.completion.resolved_by.as_ref()),
    )
}

/// A member's answer as its attempt's output: a final completes, a
/// repeatable failure is a known failure, a cancel is `Cancelled`.
pub(super) fn member_output(owner: &crate::EffectOpener, end: MemberEnd) -> SettledOutput {
    let (completed, failure) = match end {
        MemberEnd::Cancelled => {
            return SettledOutput::Cancelled {
                evidence: AvailableEvidence::default(),
            };
        }
        // A park is answered by `parked_output`, and a fault is attempted
        // again by `member_body`.
        MemberEnd::Parked(_) | MemberEnd::Faulted(_) => return SettledOutput::Interrupted,
        MemberEnd::Final(completed) => (completed, None),
        MemberEnd::Retry {
            failure,
            suggested_delay_ms,
        } => (failure, Some(suggested_delay_ms)),
    };
    // An answer that does not encode reached no durable form: the call may
    // or may not have taken effect.
    let Ok(output) = completed_material(owner, &completed) else {
        return SettledOutput::Interrupted;
    };
    match failure {
        None => SettledOutput::Completed(output),
        Some(suggested_delay_ms) => {
            SettledOutput::Failed(output.failure(KnownFailureReason::Reported, suggested_delay_ms))
        }
    }
}

impl ProductionToolHandlers<'_> {
    /// Run attempt `attempt` of the member `call`, invoked as `invocation`,
    /// to its end or to a failure its policy repeats when `may_retry`, with
    /// the store-local effects its final or its park staged.
    async fn member_attempt(
        self: &Arc<Self>,
        owner: &crate::EffectOpener,
        call: &crate::sansio::PendingToolCall,
        invocation: ToolInvocation,
        attempt: AttemptOrdinal,
        may_retry: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(MemberEnd, Vec<StoreLocalEffect>), SingletonRunError> {
        let Some(mut definition) = self.leaf_definition(&invocation) else {
            return Ok((MemberEnd::Final(self.unavailable_call(call)), Vec::new()));
        };
        // Every attempt settles under the declaration the call's admission
        // pinned, on whichever owner runs it: a host that retyped the tool
        // since changes later calls, never this one.
        if let Some(admitted) = &self.admitted_declaration {
            definition = definition.with_admitted_declaration(admitted.clone());
        }
        if let Some(engine) = definition.manifest.isolation_engine() {
            let granted = invocation
                .execution_grant
                .as_deref()
                .map(|grant| &grant.provider);
            let binding = self
                .context
                .dispatch()
                .composition
                .tool_run_binding(
                    &invocation.tool_id,
                    self.context
                        .dispatch()
                        .routed_provider(&invocation.tool_id, granted),
                )
                .map_err(crate::RuntimeEffectControllerError::from)?;
            match self
                .bind_isolated(
                    owner,
                    &invocation.id,
                    &definition.manifest.id,
                    &invocation.args,
                    engine,
                    &binding.executable,
                )
                .await
            {
                Ok(start) => {
                    self.isolated
                        .lock_recover()
                        .insert(invocation.id.clone(), start);
                }
                Err(crate::HoldEnvError::Refused(refusal)) => {
                    return Ok((
                        MemberEnd::Final(self.refused_isolated_call(call, &refusal)),
                        Vec::new(),
                    ));
                }
                Err(crate::HoldEnvError::Fault(error)) => {
                    return Err(crate::RuntimeEffectControllerError::from(error).into());
                }
            }
        }
        let declaration = definition.manifest.declaration().clone();
        let output_schema = definition.contract.output_schema.canonical.clone();
        let singleton = self.admit_leaf(
            owner,
            &invocation,
            definition,
            self.context.parent_invocation().cloned(),
            &self.context.tool_observation_attribution(),
        )?;
        let scope = self
            .context
            .dispatch()
            .effect_controller
            .execution_scope()
            .clone();
        let handlers: Arc<dyn SingletonToolHandlers + '_> = Arc::clone(self) as _;
        let admitted = AdmittedToolCall::admit(handlers, singleton, &scope).await?;
        Ok(match admitted.attempt(attempt, may_retry, cancel).await? {
            AttemptEnd::Ended(CallEnd::Withheld {
                decision: CallDecision::Cancelled,
                ..
            }) => (MemberEnd::Cancelled, Vec::new()),
            AttemptEnd::Ended(end) => {
                let store_local = match &end {
                    CallEnd::Final { store_local, .. } => store_local.clone(),
                    CallEnd::Withheld { .. } => Vec::new(),
                };
                (
                    MemberEnd::Final(self.completed_call(&call.call_id, &end)?),
                    store_local,
                )
            }
            AttemptEnd::Retry {
                capture,
                suggested_delay_ms,
            } => (
                MemberEnd::Retry {
                    failure: retry_failure(call, &capture),
                    suggested_delay_ms,
                },
                Vec::new(),
            ),
            AttemptEnd::Parked {
                completion,
                launch,
                store_local,
            } => (
                MemberEnd::Parked(ParkedCall {
                    completion: *completion,
                    launch: launch.map(|launch| *launch),
                    declaration,
                    output_schema,
                }),
                store_local,
            ),
            AttemptEnd::Faulted(fault) => (MemberEnd::Faulted(fault), Vec::new()),
        })
    }

    /// The answer of `call`, an isolated call whose start its provider or
    /// environment refused (D6): its tool error. No admission observes it,
    /// so its activity pair is observed here, as an unavailable call's is.
    fn refused_isolated_call(
        &self,
        call: &crate::sansio::PendingToolCall,
        refusal: &crate::ProcessEnvRefusal,
    ) -> CompletedCall {
        let output = ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::InvalidRequest,
            "process_env_refused",
            refusal.to_string(),
        ));
        self.observed_unadmitted(call, output)
    }

    /// The answer of `call`, whose tool the catalog does not name. No
    /// admission observes it, so its activity pair is observed here, in
    /// order, as an admitted call's is (FIG-5353).
    fn unavailable_call(&self, call: &crate::sansio::PendingToolCall) -> CompletedCall {
        self.observed_unadmitted(call, unavailable())
    }

    /// `call` answered `output` without an admission: its activity pair is
    /// observed here, in order.
    fn observed_unadmitted(
        &self,
        call: &crate::sansio::PendingToolCall,
        output: ToolCallOutput,
    ) -> CompletedCall {
        let completed = answered(call, output);
        self.context.emit_tool_call_started(
            call.call_id.as_str(),
            &crate::tool_dispatch::ToolCallIds::of_pending(call),
            &call.tool_name,
            call.args.clone(),
            crate::session::tool_execution::tool_activity_id(&call.call_id),
        );
        self.context.emit_tool_call_completed_activity(
            call.call_id.as_str(),
            &ToolCallRecord {
                call_id: completed.call_id.clone(),
                provider_call_id: completed.provider_call_id.clone(),
                tool: completed.tool_name.clone(),
                args: completed.args.clone(),
                output: completed.output.clone(),
            },
            None,
        );
        completed
    }
}

/// The answer of a failure the call's policy repeats: its reported output,
/// undecided and unpresented, should the round end on it.
fn retry_failure(
    call: &crate::sansio::PendingToolCall,
    capture: &SingletonCapture,
) -> CompletedCall {
    let output = capture
        .output()
        .and_then(|output| decode::<Captured>(output).ok())
        .map_or_else(
            || {
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Execution,
                    "tool_failed",
                    "the tool reported a failure",
                ))
            },
            |captured| captured.output,
        );
    answered(call, output)
}

/// What each attempt of a member carries from its owner, beyond its call.
#[derive(Clone, Default)]
pub(super) struct MemberAttempts {
    /// The trace scope each attempt traces the call under, when its fold
    /// does not: a cell's call, whose cell's admission retained it. A round
    /// member is traced by its round's fold.
    pub(super) traced_scope: Option<lash_trace::DurableTraceScope>,
    /// The closing of a cell's Run: a stop an attempt meets once it closes
    /// is the closing's own.
    pub(super) closing: Option<super::RunClosing>,
    /// The round owner's accepted stop: an answer an attempt decides after
    /// it is the stop's.
    pub(super) stop: Option<AcceptedStop>,
}

/// The body of `execution`, an attempt of the member `call` invoked as
/// `invocation`, over `context`'s catalog and owned by `owner`'s run: the
/// call's admission checks, its attempt and its decision, run in memory
/// between its `x_start` and its `x_outcome`. `policies` are the current
/// declarations a repeat is vetoed against. `attempts` is what each
/// attempt carries from the member's owner.
pub(super) fn member_body(
    context: &RuntimeExecutionContext<'static>,
    owner: &crate::EffectOpener,
    call: crate::sansio::PendingToolCall,
    invocation: ToolInvocation,
    execution: &AdmittedExecution,
    policies: &PolicyView,
    attempts: MemberAttempts,
) -> MemberBody {
    let MemberAttempts {
        traced_scope,
        closing,
        stop,
    } = attempts;
    let context = context.clone();
    let owner = owner.clone();
    let tool = execution.draft().tool().clone();
    let pinned = execution.policy();
    let ordinal = AttemptOrdinal::new(execution.attempt());
    let may_retry = pinned
        .retry()
        .is_some_and(|retry| execution.attempt() < retry.max_attempts.get())
        && policies.permits_repeat(&tool, pinned);
    let execution = execution.clone();
    let key = execution
        .draft()
        .pinned_wait()
        .and_then(|wait| waits::host_key(&wait.wait()));
    Box::new(move |token| {
        Box::pin(async move {
            let Some(ordinal) = ordinal else {
                return SettledOutput::Interrupted.into();
            };
            let mut faults = 0_u32;
            let (end, store_local) = loop {
                // Each attempt owns its handlers: an inline body stops on the
                // member's cancel, which its owner fires on a turn cancel.
                let mut handlers = ProductionToolHandlers::new(
                    context.clone().with_cancellation_token(token.clone()),
                )
                .with_completion_key(key.clone())
                .with_admitted_declaration(execution.draft().declaration().cloned())
                .with_stops(closing.clone(), stop.clone());
                handlers.model_call_owner = Some(lash_durable::domain::ModelCallOwner {
                    owner: execution.id().owner.clone(),
                    run: execution.id().run,
                    call: call.call_id.clone(),
                });
                handlers.traced_scope = traced_scope.clone();
                let handlers = Arc::new(handlers);
                match handlers
                    .member_attempt(
                        &owner,
                        &call,
                        invocation.clone(),
                        ordinal,
                        may_retry,
                        &token,
                    )
                    .await
                {
                    // A retryable attempt fault took no effect and is never
                    // the call's answer: the same attempt runs again,
                    // uncounted and unrecorded, until it answers or the
                    // call's limit or cancel stops it (FIG-5329). A crash
                    // meanwhile recovers its started row as any other.
                    Ok((MemberEnd::Faulted(fault), _)) => {
                        tracing::debug!(
                            call_id = %call.call_id,
                            faults,
                            error = %fault,
                            "tool attempt faulted; attempting it again"
                        );
                        context
                            .dispatch()
                            .clock
                            .sleep(context.tool_fault_retry().after_faults(faults))
                            .await;
                        faults = faults.saturating_add(1);
                    }
                    Ok(ended) => break ended,
                    Err(error) => {
                        break (
                            MemberEnd::Final(answered(
                                &call,
                                ToolCallOutput::failure(crate::ToolFailure::runtime(
                                    crate::ToolFailureClass::Internal,
                                    "tool_run_fault",
                                    error.to_string(),
                                )),
                            )),
                            Vec::new(),
                        );
                    }
                }
            };
            let mut result = match end {
                MemberEnd::Parked(parked) => parked_output(&call, &owner, &execution, &parked),
                end => member_output(&owner, end).into(),
            };
            // The effects commit with the completion or the park that
            // staged them; any other answer leaves them unwritten, and the
            // plugin state among them publishes nothing.
            if matches!(
                result.output,
                SettledOutput::Completed(_) | SettledOutput::Waiting(_)
            ) {
                result.store_local = store_local;
            } else {
                for effect in store_local {
                    if let StoreLocalEffect::PluginState(staged) = effect {
                        staged.discard();
                    }
                }
            }
            result
        })
    })
}

/// The final answer of the member `call`, parked as `parked`, once one of
/// its waits ended with `resolution`: a pure function of the resolution and
/// the parked call its `Waiting` outcome recorded, settled as the
/// declaration and output schema the park recorded say. Runs no body. A
/// park that does not decode has no declaration to settle under, and
/// answers a typed recovery failure rather than a result no declaration
/// checked.
pub(super) fn resolved_member(
    owner: &crate::EffectOpener,
    call: &crate::sansio::PendingToolCall,
    parked: &Material<CompletionSource>,
    resolution: Resolution,
) -> SettledOutput {
    let Ok(recorded) = serde_json::from_str::<ParkedCall>(parked.payload()) else {
        return member_output(
            owner,
            MemberEnd::Final(answered(
                call,
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "parked_call_unreadable",
                    "the parked call's record does not decode, so its admitted declaration is unknown",
                )),
            )),
        );
    };
    let output = parked_call_output(parked, resolution)
        .settled(&recorded.declaration, &recorded.output_schema)
        .unwrap_or_else(|refusal| super::declaration_refused(&call.tool_name, refusal));
    let mut completed = answered(call, output);
    // The launch receipt is the call's host-facing intent outcome; the
    // model sees the child's value only.
    completed.intent_outcomes = recorded
        .launch
        .map(|launch| vec![launch.outcome])
        .unwrap_or_default();
    member_output(owner, MemberEnd::Final(completed))
}

/// The final answer `output` that a park of the member `call`, a call of
/// `tool`, resolved to, presented as a body presents its own (ADR 0099 §6):
/// the presentation plan `binding` recorded at admission runs over it, not
/// the session's live selection, and what it makes of the model-facing
/// return is recorded as the call's outcome. The call's
/// settlement, its intent outcomes and its completion, is published to the
/// host live, as provisional activity that the turn's commit settles.
pub(super) async fn present_resolved(
    context: &RuntimeExecutionContext<'static>,
    owner: &crate::EffectOpener,
    call: &crate::sansio::PendingToolCall,
    tool: &crate::ToolId,
    binding: Option<&crate::tool_run::AdmittedBinding>,
    output: SettledOutput,
) -> SettledOutput {
    let SettledOutput::Completed(material) = &output else {
        return output;
    };
    let Some(mut completed) = decode_completed(material.payload()) else {
        return output;
    };
    let dispatch = context.dispatch();
    let mut projected = completed.output.clone();
    super::super::attempt_coordinator::project_recorded_intent_outcomes(
        &mut projected,
        &completed.intent_outcomes,
    );
    let projection = crate::plugin::ToolResultProjectionContext {
        owner: dispatch.owner.runtime_owner(),
        call_id: call.call_id.clone(),
        tool_id: tool.clone(),
        tool_name: call.tool_name.clone(),
        render: dispatch.facts.render.clone(),
        args: call.args.clone(),
        output: projected,
        artifacts: Arc::new(crate::runtime::effect::SessionPresentationArtifacts::new(
            context.attachment_store(),
        )),
    };
    let facts = Arc::new(crate::plugin::ToolPresentationFacts {
        intent_outcomes: completed.intent_outcomes.clone(),
    });
    match dispatch
        .composition
        .present_tool_result(
            projection,
            facts,
            &binding
                .map(|binding| binding.presentation.clone())
                .unwrap_or_default(),
        )
        .await
    {
        Ok(presentation) => completed.model_return = presentation.model_return,
        // A presentation that faults leaves the call nothing to show, as a
        // body's does.
        Err(error) => {
            completed = answered(
                call,
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "tool_run_fault",
                    error.to_string(),
                )),
            );
        }
    }
    context.emit_tool_intent_outcome_activities(
        call.call_id.as_str(),
        &call.call_id,
        &completed.intent_outcomes,
    );
    context.emit_tool_call_completed_activity(
        call.call_id.as_str(),
        &ToolCallRecord {
            call_id: completed.call_id.clone(),
            provider_call_id: completed.provider_call_id.clone(),
            tool: completed.tool_name.clone(),
            args: completed.args.clone(),
            output: completed.output.clone(),
        },
        None,
    );
    member_output(owner, MemberEnd::Final(completed))
}

/// Discharges the child a parked call's declared start launched (ADR 0116
/// §3.6): a call that ends cancelled under
/// [`CancelHint::CancelExternalWork`](crate::CancelHint) cancels it, and the
/// call's hold on it is released, so a later redrive finds the launch
/// receipt its park recorded and the row may be pruned. Both are idempotent
/// registry writes; a failure is logged and leaves the hold to the owning
/// scope's close.
pub(super) async fn discharge_member(
    context: &RuntimeExecutionContext<'static>,
    call_id: &crate::ToolCallId,
    parked: &Material<CompletionSource>,
    cancelled: bool,
) {
    let Ok(parked) = serde_json::from_str::<ParkedCall>(parked.payload()) else {
        return;
    };
    let Some(process_id) = parked
        .launch
        .as_ref()
        .and_then(|launch| launch.process_id.as_ref())
    else {
        return;
    };
    let processes = &context.dispatch().processes;
    if cancelled
        && parked.completion.on_cancel == crate::CancelHint::CancelExternalWork
        && let Err(error) = processes
            .cancel_bound(process_id, context.process_scope(None))
            .await
    {
        tracing::warn!(
            process_id = %process_id,
            error = %error,
            "a cancelled parked call could not cancel the child it launched"
        );
        return;
    }
    if let Err(error) = processes
        .release_consumer_hold(process_id, &super::super::call_run::start_hold_key(call_id))
        .await
    {
        tracing::warn!(
            process_id = %process_id,
            error = %error,
            "a parked call could not release its hold on the child it launched"
        );
    }
}

/// The answer the member `call` settled with as `output`.
pub(super) fn completed_answer(
    call: &crate::sansio::PendingToolCall,
    output: &SettledOutput,
) -> CompletedCall {
    if let Some(answer) = output.stopped_answer() {
        return answered(call, answer);
    }
    output
        .payload()
        .and_then(decode_completed)
        .unwrap_or_else(|| {
            answered(
                call,
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "tool_outcome_unreadable",
                    "the call's committed output is not a tool answer",
                )),
            )
        })
}

/// The catalog's pin of the tool `manifest` declares (or none), read at
/// `now_ms`: its policy, its body's limit from its host-set execution bound,
/// and for a deferring tool the deadline its host-set park bound sets,
/// written once with the admission ([`MemberPin::admitted`]). A call no tool
/// answers is pinned under `context`'s control-phase bound: its body only
/// answers that it is unavailable. The pin binds the provider `granted`
/// names, else the one the run's surface routes the tool to, and the
/// session's presentation plan now: the callbacks the call's claim
/// obligations hold until it settles, and the plan its parked answer is
/// presented under.
pub(super) fn member_pin(
    context: &RuntimeExecutionContext<'_>,
    manifest: Option<&crate::ToolManifest>,
    tool: crate::ToolId,
    granted: Option<&crate::plugin::ToolProviderHandle>,
    now_ms: u64,
) -> MemberPin {
    if let Some(manifest) = manifest {
        let dispatch = context.dispatch();
        let binding = dispatch
            .composition
            .tool_run_binding(&tool, dispatch.routed_provider(&tool, granted))
            .ok();
        return MemberPin::admitted(tool, manifest.execution_policy, manifest.bounds(), now_ms)
            .with_declaration(manifest.declaration().clone())
            .with_binding(binding);
    }
    let control = context.dispatch().execution_budgets.tool_check_phase();
    MemberPin {
        tool,
        policy: ExecutionPolicy::Once,
        limit: lash_sansio::ExecutionLimit::starting_at(now_ms, control, control),
        park: None,
        declaration: None,
        binding: None,
    }
}

/// The policies `context`'s catalog declares now: what a resumed member
/// vetoes a stored repeat against.
pub(super) fn catalog_policies(context: &RuntimeExecutionContext<'_>) -> PolicyView {
    PolicyView::new(
        context
            .tool_catalog()
            .tools
            .iter()
            .map(|tool| (tool.manifest.id.clone(), tool.manifest.execution_policy)),
    )
}

/// A member's committed answer as the singleton trace helpers read it,
/// including the call's recorded intent outcomes.
pub(super) fn member_record(
    call: &crate::sansio::PendingToolCall,
    output: &SettledOutput,
) -> ToolCallRecord {
    let mut completed = completed_answer(call, output);
    super::super::attempt_coordinator::project_recorded_intent_outcomes(
        &mut completed.output,
        &completed.intent_outcomes,
    );
    ToolCallRecord {
        call_id: completed.call_id,
        provider_call_id: completed.provider_call_id,
        tool: completed.tool_name,
        args: completed.args,
        output: completed.output,
    }
}

/// The tools a turn's rounds run, over one execution context's catalog.
struct ProductionRoundTools {
    /// Calls this live context started, and whether it observed their final.
    traced: Mutex<BTreeMap<crate::ToolCallId, bool>>,
    context: RuntimeExecutionContext<'static>,
    owner: crate::EffectOpener,
}

impl RoundTools for ProductionRoundTools {
    fn observe(
        &self,
        call: &crate::sansio::PendingToolCall,
        member: &crate::runtime::actor::round::RoundMember,
        admitted_here: bool,
        settled_here: bool,
    ) {
        let (start, close) = {
            let mut traced = self.traced.lock_recover();
            let start = !traced.contains_key(&call.call_id);
            let closed = traced.entry(call.call_id.clone()).or_insert(false);
            let close = !*closed && settled_here && member.outcome().is_some();
            *closed |= close;
            (start, close)
        };
        // The call was admitted with its round, under the scope its member
        // carries: a successor re-folding the round traces it under that
        // scope and admits nothing (FIG-5382). Its start is the admitting
        // owner's record; a successor traces only what it observes after.
        if start && let Some(scope) = member.draft().trace() {
            if admitted_here {
                let start = crate::session::ToolCallStart {
                    call_id: &call.call_id,
                    provider_call_id: call.provider_call_id.as_deref(),
                    tool: &call.tool_name,
                    args: &call.args,
                };
                self.context.trace_tool_call_admitted(start, scope.clone());
            } else {
                self.context
                    .adopt_tool_call_trace(&call.call_id, scope.clone());
            }
        }
        // Only the owner whose acknowledged commit wrote the outcome closes
        // the call's span, at the end that commit retained (P1): a successor
        // that folds an outcome another owner committed closes nothing.
        if close && let Some(output) = member.outcome() {
            let record = member_record(call, output);
            let attempts = member
                .attempts()
                .map(|(ordinal, output, delay)| {
                    crate::trace::trace_tool_attempt(ordinal, &member_record(call, output), delay)
                })
                .collect::<Vec<_>>();
            self.context
                .trace_tool_call_completed(&record, &attempts, member.ended_at_ms());
        }
    }

    fn trace_scope(
        &self,
        call: &crate::sansio::PendingToolCall,
    ) -> Option<lash_trace::DurableTraceScope> {
        self.context
            .tool_trace_scope(&call.call_id, self.context.dispatch().clock.timestamp_ms())
            .unwrap_or_else(|error| {
                self.context.record_nested_effect_error(error);
                None
            })
    }

    fn pin(&self, call: &crate::sansio::PendingToolCall, now_ms: u64) -> MemberPin {
        let manifest = self
            .context
            .tool_catalog()
            .tools
            .iter()
            .find(|tool| tool.manifest.name == call.tool_name)
            .map(|tool| tool.manifest.clone());
        let tool = manifest.as_ref().map_or_else(
            || crate::ToolId::new(call.tool_name.clone()),
            |manifest| manifest.id.clone(),
        );
        member_pin(&self.context, manifest.as_ref(), tool, None, now_ms)
    }

    fn policies(&self) -> PolicyView {
        catalog_policies(&self.context)
    }

    fn drift(&self, calls: &[&crate::sansio::PendingToolCall]) -> Option<crate::RuntimeError> {
        let dispatch = self.context.dispatch();
        calls.iter().find_map(|call| {
            dispatch
                .drift_of(&call.tool_name)
                .map(|drift| drift.refusal(&call.call_id).into_runtime_error())
        })
    }

    fn stop_grace(&self) -> std::time::Duration {
        self.context.dispatch().execution_budgets.stop_grace()
    }

    fn refusal(&self, calls: &[crate::sansio::PendingToolCall]) -> Option<Vec<CompletedCall>> {
        // A step's group is the count the session's `max_tool_calls` caps on
        // a protocol without cells: a group past it is refused whole, each
        // member answering the refusal that names the limit (FIG-4546).
        let limit = self.context.max_tool_calls();
        if calls.len() > limit.get() {
            let exceeded = crate::ToolCallLimitExceeded {
                scope: crate::ToolCallLimitScope::Cell,
                limit,
                counted: 0,
                requested: calls.len(),
            };
            return Some(
                calls
                    .iter()
                    .map(|call| {
                        answered(
                            call,
                            ToolCallOutput::failure(
                                crate::session::tool_execution::tool_call_limit_failure(exceeded),
                            ),
                        )
                    })
                    .collect(),
            );
        }
        None
    }

    fn body(
        &self,
        call: &crate::sansio::PendingToolCall,
        execution: &AdmittedExecution,
        stop: Option<&AcceptedStop>,
    ) -> MemberBody {
        let invocation =
            ToolInvocation::from_pending(call.clone(), execution.draft().tool().clone());
        member_body(
            &self.context,
            &self.owner,
            call.clone(),
            invocation,
            execution,
            &self.policies(),
            MemberAttempts {
                stop: stop.cloned(),
                ..MemberAttempts::default()
            },
        )
    }

    fn granted_body(
        &self,
        call: &crate::sansio::PendingToolCall,
        execution: &AdmittedExecution,
        grant: crate::ToolExecutionGrant,
    ) -> MemberBody {
        let invocation =
            ToolInvocation::from_pending(call.clone(), execution.draft().tool().clone())
                .with_execution_grant(grant);
        member_body(
            &self.context,
            &self.owner,
            call.clone(),
            invocation,
            execution,
            &self.policies(),
            MemberAttempts::default(),
        )
    }

    fn resolved(
        &self,
        call: &crate::sansio::PendingToolCall,
        _execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        resolved_member(&self.owner, call, parked, resolution)
    }

    fn present<'a>(
        &'a self,
        call: &'a crate::sansio::PendingToolCall,
        execution: &'a AdmittedExecution,
        output: SettledOutput,
    ) -> Presented<'a> {
        Box::pin(present_resolved(
            &self.context,
            &self.owner,
            call,
            execution.draft().tool(),
            execution.draft().binding(),
            output,
        ))
    }

    fn discharge<'a>(
        &'a self,
        call: &'a crate::sansio::PendingToolCall,
        _execution: &'a AdmittedExecution,
        parked: &'a Material<CompletionSource>,
        cancelled: bool,
    ) -> Discharge<'a> {
        Box::pin(discharge_member(
            &self.context,
            &call.call_id,
            parked,
            cancelled,
        ))
    }

    fn completed(
        &self,
        call: &crate::sansio::PendingToolCall,
        output: &SettledOutput,
    ) -> CompletedCall {
        completed_answer(call, output)
    }

    fn publish_state(
        &self,
        state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.context.dispatch().state.publish_committed_state(state)
    }
}

impl RuntimeExecutionContext<'_> {
    /// The tools the rounds of `owner`'s turn run, over this context's
    /// catalog: what a production `TurnDrive::tools` answers.
    ///
    /// # Errors
    ///
    /// A context whose dispatch borrows its caller's frame, which no round
    /// body may outlive.
    #[doc(hidden)]
    pub fn round_tools(
        &self,
        owner: crate::EffectOpener,
    ) -> Result<Arc<dyn RoundTools>, crate::RuntimeEffectControllerError> {
        let context = self.to_static().ok_or_else(|| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                "a round's tools need a context that owns its dispatch",
            )
        })?;
        Ok(Arc::new(ProductionRoundTools {
            context,
            owner,
            traced: Mutex::default(),
        }))
    }
}

#[cfg(test)]
#[path = "round_tests.rs"]
mod tests;
