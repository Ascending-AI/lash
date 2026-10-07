//! A turn's tool round on the production tools: each member's attempt runs
//! the call's admission, its attempt and its decision in memory, between
//! the `x_start` and the `x_outcome` its round commits, and hands the round
//! the store-local effects its realization staged, which commit with that
//! `x_outcome` (ADR 0132 §5).
use super::*;
use crate::runtime::actor::round::{
    AdmittedExecution, BodyOutput, CompletedCall, Discharge, MemberBody, MemberPin, MemberResult,
    PolicyView, RoundTools, StoreLocalEffect, completed_material, decode_completed,
};
use crate::runtime::actor::waits::{self, Resolution, WaitDeadline};
use crate::session::tool_execution::ToolInvocation;
use crate::tool_dispatch::call_run::{AdmittedToolCall, AttemptEnd, CallEnd};
use crate::tool_run::{
    AttemptOutcome, AvailableEvidence, CompletionSource, KnownFailure, KnownFailureReason,
    MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole,
};

/// How one attempt of a round member ended.
enum MemberEnd {
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
}

/// A parked call as its `Waiting` outcome records it: its pending
/// completion, and the launch receipt of the start it declared to resolve
/// it. What its resolution is answered from.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ParkedCall {
    completion: crate::PendingCompletion,
    launch: Option<super::super::LaunchReceipt>,
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

fn answered(call: &crate::sansio::PendingToolCall, output: ToolCallOutput) -> CompletedCall {
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

/// The answer of an outcome that names no material: what a crash, a limit
/// or a cancel left of the call.
fn outcome_output(outcome: &AttemptOutcome) -> ToolCallOutput {
    match outcome {
        AttemptOutcome::Interrupted => ToolCallOutput::failure(
            crate::ToolFailure::runtime(
                crate::ToolFailureClass::Execution,
                "tool_interrupted",
                "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.",
            )
            .with_cause(crate::ToolFailureCause::Interrupted),
        ),
        AttemptOutcome::TimedOut { cause, .. } => ToolCallOutput::failure(
            crate::ToolFailure::runtime(
                crate::ToolFailureClass::Timeout,
                "tool_timed_out",
                format!(
                    "tool exceeded its {cause:?} limit; it may have partly run, and may still be running."
                ),
            )
            .with_cause(crate::ToolFailureCause::ExecutionLimit { cause: *cause }),
        ),
        AttemptOutcome::Cancelled { .. } => ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime("the turn cancelled the call"),
        ),
        AttemptOutcome::Completed(_) | AttemptOutcome::Failed(_) | AttemptOutcome::Waiting(_) => {
            ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "tool_outcome_unreadable",
                "the call's committed outcome names no readable answer",
            ))
        }
    }
}

/// A parked member's answer: `Waiting` on the completion wait its round
/// pinned, the parked call riding as its material, and the process whose
/// terminal its runner pins a wait on beside it.
fn parked_output(
    call: &crate::sansio::PendingToolCall,
    owner: &crate::EffectOpener,
    execution: &AdmittedExecution,
    parked: &ParkedCall,
) -> MemberResult {
    let encoded = execution.draft().pinned_wait().and_then(|pinned| {
        let text = serde_json::to_string(parked).ok()?;
        let metadata = MaterialPayload::new(
            MaterialOwner::Run {
                opener: owner.clone(),
            },
            MaterialRole::AttemptOutput,
            None,
            text.clone(),
        )
        .reference(MaterialLocation::JournalLocal)
        .ok()?;
        Some((pinned, metadata, text))
    });
    // A call whose round pinned no wait never had a key to take, so it
    // cannot park; nor can one whose park does not encode.
    let Some((pinned, metadata, text)) = encoded else {
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
    MemberResult {
        output: BodyOutput {
            outcome: AttemptOutcome::Waiting(CompletionSource {
                wait: pinned.id.to_hex(),
                terminal: None,
                metadata,
            }),
            material: Some(text),
        },
        store_local: Vec::new(),
        terminal: parked.awaited_process().cloned(),
    }
}

/// A member's answer as its attempt's output: a final completes, a
/// repeatable failure is a known failure, a cancel is `Cancelled`.
fn member_output(owner: &crate::EffectOpener, end: MemberEnd) -> BodyOutput {
    let (completed, failure) = match end {
        MemberEnd::Cancelled => {
            return AttemptOutcome::Cancelled {
                evidence: AvailableEvidence::default(),
            }
            .into();
        }
        // A park is answered by `parked_output`.
        MemberEnd::Parked(_) => return AttemptOutcome::Interrupted.into(),
        MemberEnd::Final(completed) => (completed, None),
        MemberEnd::Retry {
            failure,
            suggested_delay_ms,
        } => (failure, Some(suggested_delay_ms)),
    };
    // An answer that does not encode reached no durable form: the call may
    // or may not have taken effect.
    let Ok((output, text)) = completed_material(owner, &completed) else {
        return AttemptOutcome::Interrupted.into();
    };
    BodyOutput {
        outcome: match failure {
            None => AttemptOutcome::Completed(output),
            Some(suggested_delay_ms) => AttemptOutcome::Failed(KnownFailure {
                output,
                reason: KnownFailureReason::Reported,
                suggested_delay_ms,
            }),
        },
        material: Some(text),
    }
}

impl ProductionToolHandlers<'_> {
    /// Run attempt `attempt` of the round member `call`, pinned to `tool`,
    /// to its end or to a failure its policy repeats when `may_retry`, with
    /// the store-local effects its final or its park staged.
    async fn round_member(
        self: &Arc<Self>,
        owner: &crate::EffectOpener,
        call: &crate::sansio::PendingToolCall,
        tool: crate::ToolId,
        attempt: AttemptOrdinal,
        may_retry: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(MemberEnd, Vec<StoreLocalEffect>), SingletonRunError> {
        let invocation = ToolInvocation::from_pending(call.clone(), tool);
        let Some(definition) = self.leaf_definition(&invocation) else {
            return Ok((MemberEnd::Final(answered(call, unavailable())), Vec::new()));
        };
        let mut isolation_bound = false;
        if definition.manifest.declaration.isolated {
            let source = invocation
                .execution_grant
                .as_deref()
                .and_then(|grant| grant.source_id.as_deref());
            let binding = self
                .context
                .dispatch()
                .plugins
                .tool_run_binding(&invocation.tool_id, source)
                .map_err(crate::RuntimeEffectControllerError::from)?;
            if let Some(start) = self.bind_isolated(
                owner,
                &invocation.id,
                &definition.manifest.id,
                &invocation.args,
                &binding.executable,
            ) {
                self.isolated
                    .lock_recover()
                    .insert(invocation.id.clone(), start);
                isolation_bound = true;
            }
        }
        if let Err(refusal) =
            super::super::admit_tool_round([Some((&definition.manifest, isolation_bound))])
        {
            return Ok((
                MemberEnd::Final(answered(
                    call,
                    ToolCallOutput::failure(refusal.failure_for(0, &call.tool_name)),
                )),
                Vec::new(),
            ));
        }
        let mut environment = self.environment.clone();
        let singleton = self
            .admit_leaf(
                owner,
                &invocation,
                definition,
                self.context.parent_invocation().cloned(),
                &self.context.tool_run_env_spec(),
                &self.context.tool_observation_attribution(),
                &mut environment,
            )
            .await?;
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
                }),
                store_local,
            ),
        })
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

/// The tools a turn's rounds run, over one execution context's catalog.
struct ProductionRoundTools {
    context: RuntimeExecutionContext<'static>,
    owner: crate::EffectOpener,
}

impl RoundTools for ProductionRoundTools {
    fn pin(&self, call: &crate::sansio::PendingToolCall, now_ms: u64) -> MemberPin {
        let budgets = self.context.dispatch().plugins.execution_budgets();
        let manifest = self
            .context
            .tool_catalog()
            .tools
            .iter()
            .find(|tool| tool.manifest.name == call.tool_name)
            .map(|tool| tool.manifest.clone());
        let total = manifest
            .as_ref()
            .and_then(|manifest| budgets.admit_tool(manifest).ok())
            .unwrap_or_else(|| budgets.tool_default());
        let limit = lash_sansio::ExecutionLimit::starting_at(now_ms, total, total);
        // One limit spans a deferring call's body and its park: its wait's
        // deadline is the limit's, written once with the admission.
        let wait = manifest
            .as_ref()
            .filter(|manifest| manifest.declaration.may_defer)
            .map(|_| {
                WaitDeadline::at_instant(lash_durable::DurableInstant(
                    i64::try_from(limit.expires_at).unwrap_or(i64::MAX),
                ))
            });
        MemberPin {
            tool: manifest.as_ref().map_or_else(
                || crate::ToolId::new(call.tool_name.clone()),
                |manifest| manifest.id.clone(),
            ),
            policy: manifest.map_or(ExecutionPolicy::Once, |manifest| manifest.execution_policy),
            limit,
            wait,
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new(
            self.context
                .tool_catalog()
                .tools
                .iter()
                .map(|tool| (tool.manifest.id.clone(), tool.manifest.execution_policy)),
        )
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
        let catalog = self.context.tool_catalog();
        let manifests: Vec<_> = calls
            .iter()
            .map(|call| {
                catalog
                    .tools
                    .iter()
                    .find(|tool| tool.manifest.name == call.tool_name)
                    .map(|tool| tool.manifest.clone())
            })
            .collect();
        // An isolated member is bound when its provider binds a process; the
        // binding itself is the member's body's, so only a member no
        // provider can bind refuses here.
        let handlers = ProductionToolHandlers::new(self.context.clone(), None);
        let bound: Vec<bool> = calls
            .iter()
            .zip(&manifests)
            .map(|(call, manifest)| {
                manifest.as_ref().is_some_and(|manifest| {
                    manifest.declaration.isolated
                        && self
                            .context
                            .dispatch()
                            .plugins
                            .tool_run_binding(&manifest.id, None)
                            .ok()
                            .and_then(|binding| {
                                handlers.bind_isolated(
                                    &self.owner,
                                    &call.call_id,
                                    &manifest.id,
                                    &call.args,
                                    &binding.executable,
                                )
                            })
                            .is_some()
                })
            })
            .collect();
        let refusal = super::super::admit_tool_round(
            manifests
                .iter()
                .zip(&bound)
                .map(|(manifest, bound)| manifest.as_ref().map(|manifest| (manifest, *bound))),
        )
        .err()?;
        Some(
            calls
                .iter()
                .enumerate()
                .map(|(member, call)| {
                    let output = if manifests[member].is_some() {
                        ToolCallOutput::failure(refusal.failure_for(member, &call.tool_name))
                    } else {
                        unavailable()
                    };
                    answered(call, output)
                })
                .collect(),
        )
    }

    fn body(
        &self,
        call: &crate::sansio::PendingToolCall,
        execution: &AdmittedExecution,
    ) -> MemberBody {
        let context = self.context.clone();
        let owner = self.owner.clone();
        let call = call.clone();
        let tool = execution.draft().tool().clone();
        let pinned = execution.policy();
        let ordinal = AttemptOrdinal::new(execution.attempt());
        let may_retry = execution.attempt() < pinned.max_attempts()
            && self.policies().permits_repeat(&tool, pinned);
        let execution = execution.clone();
        let key = execution
            .draft()
            .pinned_wait()
            .and_then(|wait| waits::host_key(&wait.wait()));
        Box::new(move |token| {
            Box::pin(async move {
                let Some(ordinal) = ordinal else {
                    return BodyOutput::from(AttemptOutcome::Interrupted).into();
                };
                // Each attempt owns its handlers: an inline body stops on the
                // member's cancel, which the round fires on a turn cancel.
                let handlers = Arc::new(
                    ProductionToolHandlers::new(
                        context.with_cancellation_token(token.clone()),
                        None,
                    )
                    .with_completion_key(key),
                );
                let (end, store_local) = handlers
                    .round_member(&owner, &call, tool, ordinal, may_retry, &token)
                    .await
                    .unwrap_or_else(|error| {
                        (
                            MemberEnd::Final(answered(
                                &call,
                                ToolCallOutput::failure(crate::ToolFailure::runtime(
                                    crate::ToolFailureClass::Internal,
                                    "tool_run_fault",
                                    error.to_string(),
                                )),
                            )),
                            Vec::new(),
                        )
                    });
                let mut result = match end {
                    MemberEnd::Parked(parked) => parked_output(&call, &owner, &execution, &parked),
                    end => member_output(&owner, end).into(),
                };
                // The effects commit with the completion or the park that
                // staged them; any other answer leaves them unwritten.
                if matches!(
                    result.output.outcome,
                    AttemptOutcome::Completed(_) | AttemptOutcome::Waiting(_)
                ) {
                    result.store_local = store_local;
                }
                result
            })
        })
    }

    fn resolved(
        &self,
        call: &crate::sansio::PendingToolCall,
        _execution: &AdmittedExecution,
        _source: &CompletionSource,
        metadata: Option<&str>,
        resolution: Resolution,
    ) -> BodyOutput {
        let parked = metadata.and_then(|text| serde_json::from_str::<ParkedCall>(text).ok());
        let output = crate::tool_result::tool_output_from_completion_resolution(
            resolution,
            parked
                .as_ref()
                .and_then(|parked| parked.completion.resolved_by.as_ref()),
        );
        let mut completed = answered(call, output);
        // The launch receipt is the call's host-facing intent outcome; the
        // model sees the child's value only.
        completed.intent_outcomes = parked
            .and_then(|parked| parked.launch)
            .map(|launch| vec![launch.outcome])
            .unwrap_or_default();
        member_output(&self.owner, MemberEnd::Final(completed))
    }

    /// Discharges the child a parked call's declared start launched (ADR
    /// 0116 §3.6): a call that ends cancelled under
    /// [`CancelHint::CancelExternalWork`](crate::CancelHint) cancels it, and
    /// the call's hold on it is released, so a later redrive finds the
    /// launch receipt its park recorded and the row may be pruned. Both are
    /// idempotent registry writes; a failure is logged and leaves the hold
    /// to the owning scope's close.
    fn discharge<'a>(
        &'a self,
        call: &'a crate::sansio::PendingToolCall,
        _execution: &'a AdmittedExecution,
        metadata: Option<&'a str>,
        cancelled: bool,
    ) -> Discharge<'a> {
        Box::pin(async move {
            let Some(parked) =
                metadata.and_then(|text| serde_json::from_str::<ParkedCall>(text).ok())
            else {
                return;
            };
            let Some(process_id) = parked
                .launch
                .as_ref()
                .and_then(|launch| launch.process_id.as_ref())
            else {
                return;
            };
            let processes = &self.context.dispatch().processes;
            if cancelled
                && parked.completion.on_cancel == crate::CancelHint::CancelExternalWork
                && let Err(error) = processes
                    .cancel_bound(process_id, self.context.process_scope(None))
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
                .release_consumer_hold(
                    process_id,
                    &super::super::call_run::start_hold_key(&call.call_id),
                )
                .await
            {
                tracing::warn!(
                    process_id = %process_id,
                    error = %error,
                    "a parked call could not release its hold on the child it launched"
                );
            }
        })
    }

    fn completed(
        &self,
        call: &crate::sansio::PendingToolCall,
        outcome: &AttemptOutcome,
        material: Option<&str>,
    ) -> CompletedCall {
        material
            .and_then(decode_completed)
            .unwrap_or_else(|| answered(call, outcome_output(outcome)))
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
        Ok(Arc::new(ProductionRoundTools { context, owner }))
    }
}
