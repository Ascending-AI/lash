//! A turn's tool round on the production tools: each member's attempt runs
//! the call's admission, its attempt and its decision in memory, between
//! the `x_start` and the `x_outcome` its round commits (ADR 0132 §5).
use super::*;
use crate::runtime::actor::round::{
    AdmittedExecution, BodyOutput, CompletedCall, MemberBody, MemberPin, PolicyView, RoundTools,
    completed_material, decode_completed,
};
use crate::session::tool_execution::ToolInvocation;
use crate::tool_dispatch::call_run::{AdmittedToolCall, AttemptEnd, CallEnd};
use crate::tool_run::{AttemptOutcome, AvailableEvidence, KnownFailure, KnownFailureReason};

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
    /// to its end or to a failure its policy repeats when `may_retry`.
    async fn round_member(
        self: &Arc<Self>,
        owner: &crate::EffectOpener,
        call: &crate::sansio::PendingToolCall,
        tool: crate::ToolId,
        attempt: AttemptOrdinal,
        may_retry: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<MemberEnd, SingletonRunError> {
        let invocation = ToolInvocation::from_pending(call.clone(), tool);
        let Some(definition) = self.leaf_definition(&invocation) else {
            return Ok(MemberEnd::Final(answered(call, unavailable())));
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
            return Ok(MemberEnd::Final(answered(
                call,
                ToolCallOutput::failure(refusal.failure_for(0, &call.tool_name)),
            )));
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
            }) => MemberEnd::Cancelled,
            AttemptEnd::Ended(end) => MemberEnd::Final(self.completed_call(&call.call_id, &end)?),
            AttemptEnd::Retry {
                capture,
                suggested_delay_ms,
            } => MemberEnd::Retry {
                failure: retry_failure(call, &capture),
                suggested_delay_ms,
            },
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
        MemberPin {
            tool: manifest.as_ref().map_or_else(
                || crate::ToolId::new(call.tool_name.clone()),
                |manifest| manifest.id.clone(),
            ),
            policy: manifest.map_or(ExecutionPolicy::Once, |manifest| manifest.execution_policy),
            limit: lash_sansio::ExecutionLimit::starting_at(now_ms, total, total),
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
        Box::new(move |token| {
            Box::pin(async move {
                let Some(ordinal) = ordinal else {
                    return BodyOutput::from(AttemptOutcome::Interrupted).into();
                };
                // Each attempt owns its handlers: an inline body stops on the
                // member's cancel, which the round fires on a turn cancel.
                let handlers = Arc::new(ProductionToolHandlers::new(
                    context.with_cancellation_token(token.clone()),
                    None,
                ));
                let end = handlers
                    .round_member(&owner, &call, tool, ordinal, may_retry, &token)
                    .await
                    .unwrap_or_else(|error| {
                        MemberEnd::Final(answered(
                            &call,
                            ToolCallOutput::failure(crate::ToolFailure::runtime(
                                crate::ToolFailureClass::Internal,
                                "tool_run_fault",
                                error.to_string(),
                            )),
                        ))
                    });
                member_output(&owner, end).into()
            })
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
