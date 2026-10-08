use crate::{
    ExecutionPolicy, PreparedToolCall, RuntimeEffectInvocation, RuntimeEffectLocalExecutor,
    RuntimeInvocation, ToolCallOutput, ToolCallRecord, ToolFailure, ToolFailureClass,
};

use super::{ToolCallLaunch, ToolDispatchContext, ToolDispatchOutcome};

/// The invocation a tool call's attempts descend from: its lineage.
///
/// Every attempt and retry sleep of a call is keyed by the call's
/// [`ToolCallId`](lash_sansio::ToolCallId) and the attempt number (ADR 0117
/// §6): `{call_id}:attempt:{n}` and `{call_id}:attempt:{n}:sleep`, under the
/// parent invocation when the call has one — a command's or a process
/// body's — and under `tool:` when it has none. The call id is
/// unique per logical call, so no second formula names an attempt.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolAttemptLineage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<RuntimeInvocation>,
}

impl ToolAttemptLineage {
    /// Attempts under `parent`.
    pub fn under(parent: RuntimeInvocation) -> Self {
        Self {
            parent: Some(parent),
        }
    }

    /// Attempts under `parent`, or parentless.
    pub fn from_parent(parent: Option<RuntimeInvocation>) -> Self {
        Self { parent }
    }

    pub(super) fn attempt_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeEffectInvocation {
        self.invocation(
            context,
            crate::runtime::causal::CommandSubKey::ToolAttempt {
                call_id: call.call_id.clone(),
                attempt,
            }
            .to_string(),
        )
    }

    fn retry_sleep_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeEffectInvocation {
        self.invocation(
            context,
            crate::runtime::causal::CommandSubKey::ToolRetrySleep {
                call_id: call.call_id.clone(),
                attempt,
            }
            .to_string(),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        suffix: String,
    ) -> RuntimeEffectInvocation {
        let scoped = context.effect_controller.clone();
        let scope = scoped.execution_scope();
        if let Some(parent) = &self.parent {
            let parent_effect_id = parent.effect_id().unwrap_or("tool");
            return crate::runtime::causal::child_effect_invocation(
                scope,
                parent,
                format!("{parent_effect_id}:{suffix}"),
                suffix,
            );
        }
        let effect_id = format!("tool:{suffix}");
        RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), effect_id.clone())
                .expect("tool dispatch carries an admitted effect scope"),
            context.parentless_attribution(),
            effect_id,
        )
    }
}

pub struct CoordinatedToolInvocation {
    pub launch: ToolCallLaunch,
}

#[allow(clippy::too_many_arguments)]
pub async fn coordinate_tool_invocation<'run>(
    context: &ToolDispatchContext<'run>,
    call: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    execution_policy: ExecutionPolicy,
    lineage: ToolAttemptLineage,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
    child_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    mut local_executor: impl FnMut() -> RuntimeEffectLocalExecutor<'run>,
) -> CoordinatedToolInvocation {
    let max_attempts = execution_policy.max_attempts().max(1);
    let mut attempts = Vec::new();

    for attempt in 1..=max_attempts {
        let invocation = lineage.attempt_invocation(context, &call, attempt);
        let outcome = context
            .effect_controller
            .tool_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation.clone(),
                    crate::RuntimeEffectCommand::ToolAttempt {
                        call: Box::new(call.clone()),
                        execution_grant: execution_grant.clone(),
                        attempt,
                        max_attempts,
                    },
                ),
                local_executor(),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_tool_attempt_effect);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            // A runner bound to another call or owner is a host refusal,
            // including on replay. It cannot become a tool result.
            Err(err) if err.code == crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch => {
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::ControllerAborted(err),
                };
            }
            // A journaled error is the attempt's recorded `Failed` terminal
            // replaying: that durable record is the attempt's outcome, so it
            // stays model-visible exactly as it does today (FIG-3528).
            Err(err) if err.journaled => {
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
                        &call,
                        "tool_attempt_failed",
                        err.to_string(),
                        attempts,
                    ))),
                };
            }
            // Every other `Err` is a live controller error — the attempt's
            // claim, renewal or finalize failed and nothing was journaled.
            // The tool's own failures arrive inside an `Ok` outcome; a store
            // fault is handled like a crash at that point: abort the turn so
            // ADR 0042 recovery redrives the attempt, rather than committing a
            // `tool_attempt_failed` the tool never produced (FIG-3528).
            Err(err) => {
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::ControllerAborted(err),
                };
            }
        };
        if let crate::ToolAttemptLaunch::Done { record, .. } = &outcome.launch
            && let crate::ToolCallOutcome::Failure(failure) = &record.output.outcome
            && failure.code == "tool_panicked"
        {
            crate::panic_containment::enforce_message("tool_panicked", &failure.message);
        }
        match outcome.launch {
            crate::ToolAttemptLaunch::Done {
                mut record,
                intents,
            } => {
                // Admission uses the identity committed by the attempt. The
                // projection below still normalizes the host-facing record,
                // but must not repair a malformed durable attempt before the
                // intent executor has had a chance to refuse it.
                let recorded_call_id = record.call_id.clone();
                record.call_id = call.call_id.clone();
                record.provider_call_id = call.provider_call_id.clone();
                let current = context
                    .tools
                    .resolve_manifest_by_id(&call.tool_id)
                    .map_or(execution_policy, |manifest| manifest.execution_policy);
                let retry_after = if execution_policy.permits_repeat(current, attempt) {
                    match &record.output.outcome {
                        crate::ToolCallOutcome::Failure(failure)
                            if !matches!(
                                failure.cause.as_deref(),
                                Some(
                                    crate::ToolFailureCause::Interrupted
                                        | crate::ToolFailureCause::ExecutionLimit {
                                            cause: crate::LimitCause::ExecutionTotal
                                                | crate::LimitCause::WaitDeadline,
                                        }
                                )
                            ) =>
                        {
                            Some(
                                execution_policy
                                    .delay_ms_for_retry(attempt - 1, failure.suggested_delay_ms),
                            )
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                attempts.push(crate::trace::trace_tool_attempt(
                    attempt,
                    record.as_ref(),
                    (attempt < max_attempts).then_some(retry_after).flatten(),
                ));
                let Some(retry_after) = retry_after else {
                    return CoordinatedToolInvocation {
                        launch: match settle_terminal_attempt(
                            context,
                            TerminalAttemptSettlement {
                                minting_emission: &invocation,
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: &recorded_call_id,

                                record,
                                intents,
                                attempts,
                            },
                        )
                        .await
                        {
                            Ok(outcome) => ToolCallLaunch::Done(Box::new(outcome)),
                            Err(error) => ToolCallLaunch::ControllerAborted(error),
                        },
                    };
                };
                if attempt >= max_attempts {
                    return CoordinatedToolInvocation {
                        launch: match settle_terminal_attempt(
                            context,
                            TerminalAttemptSettlement {
                                minting_emission: &invocation,
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: &recorded_call_id,

                                record,
                                intents,
                                attempts,
                            },
                        )
                        .await
                        {
                            Ok(outcome) => ToolCallLaunch::Done(Box::new(outcome)),
                            Err(error) => ToolCallLaunch::ControllerAborted(error),
                        },
                    };
                }
                if retry_after > 0
                    && let Err(err) = sleep_before_retry(
                        context,
                        lineage.retry_sleep_invocation(context, &call, attempt),
                        turn_cancel_wait,
                        retry_after,
                    )
                    .await
                {
                    // The retry backoff is itself a journaled `Sleep` effect.
                    // A recorded `Failed` terminal replaying is the sleep's
                    // durable outcome and stays model-visible; a live store
                    // fault left nothing recorded and aborts like a crash
                    // (FIG-3528).
                    if err.journaled {
                        return CoordinatedToolInvocation {
                            launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
                                &call,
                                "tool_retry_sleep_failed",
                                format!(
                                    "retry sleep for tool `{}` failed after attempt {attempt}: {err}",
                                    call.tool_name
                                ),
                                attempts,
                            ))),
                        };
                    }
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::ControllerAborted(err),
                    };
                }
            }
        }
    }

    CoordinatedToolInvocation {
        launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
            &call,
            "tool_retry_loop_failed",
            "tool retry loop exited without a terminal result",
            attempts,
        ))),
    }
}

/// Facts needed to realize and project a terminal attempt.
struct TerminalAttemptSettlement<'settlement> {
    minting_emission: &'settlement RuntimeEffectInvocation,
    child_trace_hook: Option<&'settlement crate::ToolChildExecutionTraceHook>,
    recorded_call_id: &'settlement lash_sansio::ToolCallId,

    record: Box<ToolCallRecord>,
    intents: crate::ToolIntents,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
}

/// The local terminal facts passed from coordination to declaration realization.
struct SealedToolFinal {
    /// The attempt invocation that minted the declared intents, whose
    /// identities derive from it. `None` for a deferred completion's
    /// terminal: a parked attempt declares no intents, so it has none to mint.
    minting_emission: Option<RuntimeInvocation>,
    /// The identity the committed attempt recorded, which intent admission
    /// uses.
    recorded_call_id: lash_sansio::ToolCallId,
    record: ToolCallRecord,
    intents: crate::ToolIntents,
    intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
}

async fn settle_terminal_attempt(
    context: &ToolDispatchContext<'_>,
    settlement: TerminalAttemptSettlement<'_>,
) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
    let TerminalAttemptSettlement {
        minting_emission,
        child_trace_hook,
        recorded_call_id,

        record,
        intents,
        attempts,
    } = settlement;
    let sealed = SealedToolFinal {
        minting_emission: Some(minting_emission.clone().into_runtime_invocation()),
        recorded_call_id: recorded_call_id.clone(),
        record: *record,
        intents,
        intent_outcomes: Vec::new(),
        attempts,
    };
    drain_sealed_final(context, sealed, child_trace_hook).await
}

/// Drains a committed final and projects its intent outcomes onto its record.
///
/// The §5 barrier: admitted drains emit their nested semantic commands in rank
/// order, so a committed sibling ranked below this child that has not seated
/// holds this drain back until it does. The host waits on its own wakes for
/// those seats; the dispatch clock plays no part. The sealed final is the one
/// the point holds, so what it declared is known whichever invocation
/// committed it: a final with no intent to drain emits nothing and does not
/// wait (FIG-4308).
async fn drain_sealed_final(
    context: &ToolDispatchContext<'_>,
    sealed: SealedToolFinal,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
    let SealedToolFinal {
        minting_emission,
        recorded_call_id,
        mut record,
        intents,
        intent_outcomes: mut retained_outcomes,
        attempts,
    } = sealed;
    let intent_outcomes = match minting_emission {
        Some(minting_emission) => {
            let mut intent_context = context.intent_realization_context();
            intent_context.parent_invocation = Some(minting_emission);
            let realization = super::execute_final_tool_intents(
                &intent_context,
                &recorded_call_id,
                &intents,
                child_trace_hook,
            )
            .await?;
            // No round records this call's outcome: its store-local effects
            // commit at once.
            if !realization.store_local.is_empty() {
                context
                    .effect_controller
                    .commit_store_local(realization.store_local)
                    .await?;
            }
            let intent_outcomes = realization.receipt.outcomes;
            project_recorded_intent_outcomes(&mut record.output, &intent_outcomes);
            intent_outcomes
        }
        None if intents.is_empty() => Vec::new(),
        None => {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeToolRunShape,
                format!(
                    "the committed final of `{recorded_call_id}` declares intents but names \
                     no attempt that minted them"
                ),
            ));
        }
    };
    retained_outcomes.extend(intent_outcomes);
    Ok(ToolDispatchOutcome {
        record,
        attempts,
        intents,
        intent_outcomes: retained_outcomes,
    })
}

/// Whether `outcome` is the attempt's declared process start at
/// `intent_index`, realized or refused.
fn declares_start_at(outcome: &crate::ToolIntentExecutionOutcome, intent_index: u32) -> bool {
    declares_at(outcome, crate::ToolIntentKind::StartProcess, intent_index)
}

fn declares_at(
    outcome: &crate::ToolIntentExecutionOutcome,
    kind: crate::ToolIntentKind,
    intent_index: u32,
) -> bool {
    let (outcome_kind, index) = match outcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity, realized, ..
        } => (realized.kind(), identity.intent_index),
        crate::ToolIntentExecutionOutcome::Refused {
            intent_index: refused,
            kind,
            ..
        } => (*kind, *refused),
        _ => return false,
    };
    outcome_kind == kind && index == intent_index
}

/// The refusal that supersedes a final's optimistic output. Only a refusal
/// produced while executing a declared intent can: batch-admission refusals
/// describe the intent protocol itself and stay in the typed intent-outcome
/// stream.
pub(super) fn superseding_refusal(
    outcomes: &[crate::ToolIntentExecutionOutcome],
) -> Option<&crate::ToolIntentExecutionOutcome> {
    outcomes.iter().find(|outcome| {
        matches!(
            outcome,
            crate::ToolIntentExecutionOutcome::Refused {
                refusal: crate::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            }
        )
    })
}

pub(super) fn project_recorded_intent_outcomes(
    output: &mut crate::ToolCallOutput,
    outcomes: &[crate::ToolIntentExecutionOutcome],
) {
    if let Some(crate::ToolIntentExecutionOutcome::Refused {
        refusal: crate::ToolIntentRefusalReason::CommandFailed { cause },
        ..
    }) = superseding_refusal(outcomes)
    {
        // ADR 0042 deliberately seals the ToolAttempt terminal before draining
        // intents. Its optimistic provider value is therefore immutable; the
        // child process-command journal row is the authoritative refusal, and
        // this turn projection plus `intent_outcomes` expose that evidence.
        // Rewriting the completed parent row here would break journal-first
        // settlement and append-only replay.
        *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            cause.failure_class(),
            cause.code().to_string(),
            cause.to_string(),
        ));
        return;
    }

    let crate::ToolCallOutcome::Success(value) = &mut output.outcome else {
        return;
    };
    if let Some(index) = lash_sansio::handle::definition_slot(&value.to_json_value()) {
        let outcome = outcomes.iter().find(|outcome| {
            declares_at(outcome, crate::ToolIntentKind::PublishDefinition, index)
                || declares_at(outcome, crate::ToolIntentKind::GetDefinition, index)
        });
        match outcome {
            Some(crate::ToolIntentExecutionOutcome::Executed { realized, .. }) => {
                match realized.model_value().and_then(serde_json::from_value) {
                    Ok(decoded) => *value = decoded,
                    Err(error) => {
                        *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                            crate::ToolFailureClass::Internal,
                            "tool_value_decode_failed",
                            error.to_string(),
                        ))
                    }
                }
            }
            Some(crate::ToolIntentExecutionOutcome::Refused { refusal, .. }) => {
                *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Unavailable,
                    refusal.code(),
                    format!("{refusal:?}"),
                ));
            }
            _ => {}
        }
        return;
    }
    // A start's slot is resolved from the realized start of the same intent
    // index: the process handle its registration minted replaces the slot
    // before any model or cell sees the output (ADR 0107). A slot whose start
    // did not realize is never exposed as a handle. Only an attempt that
    // declared a start at that index answers a slot: the same spelling in any
    // other tool's output is that tool's own data and is left untouched.
    if let Some(intent_index) = lash_sansio::handle::process_start_slot(&value.to_json_value())
        && let Some(start) = outcomes
            .iter()
            .find(|outcome| declares_start_at(outcome, intent_index))
    {
        let handle = match start {
            crate::ToolIntentExecutionOutcome::Executed {
                realized: crate::ToolIntentRealized::StartProcess(handle),
                ..
            } => Ok(realized_start_handle(handle)),
            crate::ToolIntentExecutionOutcome::Executed { realized, .. } => Err(format!(
                "start slot names realized {}",
                realized.kind().as_str()
            )),
            crate::ToolIntentExecutionOutcome::Refused { refusal, .. }
            | crate::ToolIntentExecutionOutcome::ProtocolRefused { refusal } => Err(format!(
                "the declared process start did not register a process: it was refused with {}",
                refusal.describe()
            )),
        };
        let handle = match handle {
            Ok(handle) => handle,
            Err(message) => {
                *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Unavailable,
                    "process_start_unrealized",
                    message,
                ));
                return;
            }
        };
        match serde_json::from_value(handle) {
            Ok(decoded) => *value = decoded,
            Err(error) => {
                *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "tool_value_decode_failed",
                    format!("malformed realized process handle: {error}"),
                ));
            }
        }
    }
}

/// The handle record a realized start answers: the one handle kind, its id,
/// and the process id beside it. Nothing else of the realized view is copied —
/// `status` and the rest are facts a holder reads through the process tools.
fn realized_start_handle(handle: &crate::ProcessHandleView) -> serde_json::Value {
    serde_json::json!({
        lash_sansio::handle::HANDLE_FIELD: lash_sansio::handle::HANDLE_KIND,
        "id": handle.id,
        "process_id": handle.process_id,
    })
}

fn runtime_failure_outcome(
    call: &PreparedToolCall,
    code: impl Into<String>,
    message: impl Into<String>,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
) -> ToolDispatchOutcome {
    ToolDispatchOutcome {
        record: ToolCallRecord {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool: call.tool_name.clone(),
            args: call.args.clone(),
            output: ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::Internal,
                code,
                message,
            )),
        },
        attempts,
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

async fn sleep_before_retry(
    context: &ToolDispatchContext<'_>,
    invocation: RuntimeEffectInvocation,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
    retry_after_ms: u64,
) -> Result<(), crate::RuntimeEffectControllerError> {
    let outcome = context
        .effect_controller
        .wait_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::Sleep {
                    spec: crate::SleepSpec::For {
                        duration_ms: retry_after_ms,
                    },
                },
            ),
            RuntimeEffectLocalExecutor::sleep_under(
                turn_cancel_wait,
                std::sync::Arc::clone(&context.clock),
            ),
        )
        .await?;
    match outcome {
        crate::RuntimeEffectOutcome::Sleep => Ok(()),
        other => Err(crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
            format!("expected sleep outcome, got {}", other.kind().as_str()),
        )),
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::SessionId;

    fn refusal(reason: crate::ToolIntentRefusalReason) -> crate::ToolIntentExecutionOutcome {
        crate::ToolIntentExecutionOutcome::Refused {
            identity: None,
            intent_index: 0,
            kind: crate::ToolIntentKind::CancelProcess,
            refusal: reason,
        }
    }

    fn executed_at(
        realized: crate::ToolIntentRealized,
        intent_index: u32,
    ) -> crate::ToolIntentExecutionOutcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity: crate::ToolIntentIdentity {
                owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                execution_scope_id: "turn".to_string(),
                tool_call_id: crate::ToolCallId::fixture("call"),
                intent_index,
                replay_key: "replay".to_string(),
                minting_emission_replay_key: None,
            },
            realized,
        }
    }

    /// The slot a start answers before its declaration is realized, as
    /// `lash_plugin_process_controls::declarations` writes it.
    fn start_slot(intent_index: u32) -> serde_json::Value {
        lash_sansio::handle::process_start_slot_json(intent_index)
    }

    fn realized_handle(label: &str) -> serde_json::Value {
        let process_id = crate::process_id_for_test(label);
        serde_json::json!({
            "__handle__": "lash",
            "id": lash_sansio::handle::HandleId::process(&process_id).as_str(),
            "process_id": process_id,
        })
    }

    fn start_outcome(label: &str, intent_index: u32) -> crate::ToolIntentExecutionOutcome {
        executed_at(
            crate::ToolIntentRealized::StartProcess(crate::ProcessHandleView::new(
                crate::process_id_for_test(label),
                crate::ProcessIdentity::new("external"),
                crate::ProcessStatus::Running,
            )),
            intent_index,
        )
    }

    #[test]
    fn command_refusal_projects_its_typed_code_and_message() {
        let mut output = crate::ToolCallOutput::success(serde_json::json!("optimistic"));
        project_recorded_intent_outcomes(
            &mut output,
            &[refusal(crate::ToolIntentRefusalReason::CommandFailed {
                cause: crate::ToolIntentCommandFailure::ProcessNotVisible {
                    process_id: crate::process_id_for_test("p-invisible"),
                },
            })],
        );

        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("intent command refusal must supersede optimistic success")
        };
        assert_eq!(failure.code, "process_not_visible");
        assert!(failure.message.contains("not live or visible"));
    }

    #[test]
    fn protocol_admission_refusal_does_not_rewrite_provider_output() {
        let mut output = crate::ToolCallOutput::success(serde_json::json!("provider-terminal"));
        project_recorded_intent_outcomes(
            &mut output,
            &[refusal(
                crate::ToolIntentRefusalReason::CountBudgetExceeded {
                    actual: 33,
                    maximum: 32,
                },
            )],
        );

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!("provider-terminal")
        );
    }

    /// FIG-4255: the failed call names the refusal that kept the start from
    /// registering, so the next occurrence diagnoses itself from the model
    /// feedback alone.
    #[test]
    fn an_unrealized_start_names_the_refusal_that_decided_it() {
        let mut output = crate::ToolCallOutput::success(start_slot(0));

        project_recorded_intent_outcomes(
            &mut output,
            &[crate::ToolIntentExecutionOutcome::Refused {
                identity: None,
                intent_index: 0,
                kind: crate::ToolIntentKind::StartProcess,
                refusal: crate::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                    actual: 139_000,
                    maximum: crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
                },
            }],
        );

        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("an unrealized slot must not survive projection");
        };
        assert_eq!(failure.code, "process_start_unrealized");
        assert_eq!(
            failure.message,
            "the declared process start did not register a process: it was refused with \
             canonical_byte_budget_exceeded: the attempt declared 139000 canonical bytes of \
             intents; at most 65536 are admitted"
        );
    }

    /// The slot spelling is only a slot where the attempt declared a start at
    /// that index. The same bytes in any other tool's output are the tool's
    /// own data, and no projection rewrites them.
    #[test]
    fn a_slot_spelling_in_an_attempt_that_declared_no_start_there_is_left_alone() {
        let mut output = crate::ToolCallOutput::success(start_slot(1));

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-other", 0)]);
        assert_eq!(output.value_for_projection(), start_slot(1));

        let mut untrusted = crate::ToolCallOutput::success(start_slot(0));
        project_recorded_intent_outcomes(&mut untrusted, &[]);
        assert_eq!(untrusted.value_for_projection(), start_slot(0));
    }

    #[test]
    fn an_output_naming_a_process_without_being_a_handle_keeps_its_own_shape() {
        // Same id, but no handle to replace: a start's answer is a handle, so
        // an output that is not one is not the optimistic form of it.
        let mut output = crate::ToolCallOutput::success(
            serde_json::json!({ "process_id": crate::process_id_for_test("p-child"), "ok": true }),
        );

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 0)]);

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({ "process_id": crate::process_id_for_test("p-child"), "ok": true })
        );
    }

    #[test]
    fn several_starts_project_the_one_the_output_names() {
        // Field-by-field merging let the last declared start overwrite the
        // handle of the one the attempt actually answered with (FIG-3119).
        let mut output = crate::ToolCallOutput::success(start_slot(0));

        project_recorded_intent_outcomes(
            &mut output,
            &[start_outcome("p-first", 0), start_outcome("p-second", 1)],
        );

        assert_eq!(output.value_for_projection(), realized_handle("p-first"));
    }
}
