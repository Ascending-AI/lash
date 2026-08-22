use crate::facade_support::ScopedEffectControllerFacadeOps;
use crate::{
    PreparedToolCall, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeInvocation,
    ToolCallOutput, ToolCallRecord, ToolFailure, ToolFailureClass, ToolOutcome, ToolRetryPolicy,
};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;

use super::{
    PendingToolDispatchOutcome, ToolCallLaunch, ToolDispatchContext, ToolDispatchOutcome,
    ToolTriggerEffectOutcome, mark_retry_exhausted, retry_after_ms,
};

#[derive(Clone)]
pub(crate) enum ToolAttemptEffectIdentity {
    Scalar {
        parent: Option<RuntimeInvocation>,
    },
    Batch {
        parent: RuntimeInvocation,
        replay_suffix: String,
    },
    Process {
        parent: Option<RuntimeInvocation>,
        process_id: String,
    },
}

impl ToolAttemptEffectIdentity {
    fn attempt_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeInvocation {
        let replay_prefix = match self {
            Self::Scalar { .. } => call.call_id.clone(),
            Self::Batch { replay_suffix, .. } => replay_suffix.clone(),
            Self::Process { process_id, .. } => {
                format!("process:{process_id}:tool:{}", call.tool_name)
            }
        };
        let suffix = format!("{replay_prefix}:attempt:{attempt}");
        if let Some(parent) = self.parent() {
            let fallback = if matches!(self, Self::Batch { .. }) {
                "tool-batch"
            } else {
                "tool"
            };
            let parent_effect_id = parent.effect_id().unwrap_or(fallback);
            return crate::runtime::causal::child_effect_invocation(
                parent,
                format!("{parent_effect_id}:{suffix}"),
                RuntimeEffectKind::ToolAttempt,
                suffix,
            );
        }

        let effect_id = format!("tool:{suffix}");
        RuntimeInvocation::effect(
            crate::RuntimeScope::new(&context.session_id),
            effect_id.clone(),
            RuntimeEffectKind::ToolAttempt,
            effect_id,
        )
    }

    fn retry_sleep_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeInvocation {
        if let Self::Batch {
            parent,
            replay_suffix,
        } = self
        {
            let suffix = format!("{replay_suffix}:attempt:{attempt}:sleep");
            let parent_effect_id = parent.effect_id().unwrap_or("tool-batch");
            return crate::runtime::causal::child_effect_invocation(
                parent,
                format!("{parent_effect_id}:{suffix}"),
                RuntimeEffectKind::Sleep,
                suffix,
            );
        }
        if let Some(parent) = self.parent() {
            return crate::runtime::tool_retry_sleep_invocation(parent, &call.tool_name, attempt);
        }

        let replay_base = format!(
            "lash-tool:{}:{}:{}",
            context.session_id, call.call_id, call.tool_name
        );
        let effect_id = format!("{replay_base}:attempt:{attempt}:sleep");
        RuntimeInvocation::effect(
            crate::RuntimeScope::new(&context.session_id),
            effect_id.clone(),
            RuntimeEffectKind::Sleep,
            effect_id,
        )
    }

    fn parent(&self) -> Option<&RuntimeInvocation> {
        match self {
            Self::Scalar { parent } => parent.as_ref(),
            Self::Process { parent, .. } => parent.as_ref(),
            Self::Batch { parent, .. } => Some(parent),
        }
    }

    fn duration_ms(
        &self,
        context: &ToolDispatchContext<'_>,
        started_at: std::time::Instant,
        attempt_duration_ms: u64,
    ) -> u64 {
        match self {
            Self::Batch { .. } => attempt_duration_ms,
            Self::Scalar { .. } => context
                .clock
                .now()
                .duration_since(started_at)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            Self::Process { .. } => context
                .clock
                .now()
                .duration_since(started_at)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }
    }
}

pub(crate) struct CoordinatedToolInvocation {
    pub launch: ToolCallLaunch,
    pub triggers: Vec<ToolTriggerEffectOutcome>,
}

/// Sequences the per-child final intent drains of one tool batch in source
/// order.
///
/// `next` names the slot whose drain may run. A slot publishes its own
/// completion into `discharged`; `next` then walks forward over the consecutive
/// discharged prefix. Publishing is therefore order-free and idempotent, and it
/// needs neither the gate's lock to be held across an await nor a caller that
/// remembered to take its turn first. That is what lets [`IntentDrainGuard`]
/// discharge from `Drop`: a synchronous, infallible operation no exit path can
/// skip. Ordering is a property of how `next` advances rather than of an
/// assertion that only holds in debug builds.
#[derive(Default)]
pub(crate) struct BatchIntentDrainGate {
    state: std::sync::Mutex<BatchIntentDrainState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct BatchIntentDrainState {
    next: usize,
    discharged: std::collections::BTreeSet<usize>,
}

impl BatchIntentDrainGate {
    async fn wait_for(&self, index: usize) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before re-reading `next`: a discharge runs synchronously
            // from `Drop`, and `notify_waiters` only wakes waiters that were
            // already registered when it ran.
            changed.as_mut().enable();
            if self.state.lock_recover().next == index {
                return;
            }
            changed.await;
        }
    }

    fn discharge(&self, index: usize) {
        let mut state = self.state.lock_recover();
        // The prefix walk below removes indices as it consumes them, so a
        // re-discharge of an already-consumed slot would look new to `insert`
        // and settle inertly into the set rather than being caught. The guard's
        // type makes that unreachable; say so where it would break.
        debug_assert!(
            index >= state.next,
            "a discharged drain slot cannot discharge again"
        );
        if !state.discharged.insert(index) {
            return;
        }
        let mut next = state.next;
        while state.discharged.remove(&next) {
            next = next.saturating_add(1);
        }
        if next == state.next {
            return;
        }
        state.next = next;
        drop(state);
        self.changed.notify_waiters();
    }
}

/// One child's exactly-once claim on its slot in a [`BatchIntentDrainGate`].
///
/// Holding the guard is the claim; dropping it discharges the slot. Every exit
/// path drops it — an early return, a future cancelled mid-await, or an unwind
/// — so discharge is a property of the guard's lifetime rather than of twelve
/// hand-written calls, and no exit path can pin the gate's next index.
pub(crate) struct IntentDrainGuard {
    gate: std::sync::Arc<BatchIntentDrainGate>,
    index: usize,
    final_result_committed: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
}

impl IntentDrainGuard {
    pub(crate) fn new(
        gate: std::sync::Arc<BatchIntentDrainGate>,
        index: usize,
    ) -> (Self, IntentDrainCommitSignal) {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let sender = std::sync::Arc::new(sender);
        (
            Self {
                gate,
                index,
                final_result_committed: std::sync::Arc::clone(&sender),
            },
            IntentDrainCommitSignal {
                _sender: sender,
                receiver,
            },
        )
    }

    /// Publishes this child's committed final result and waits for its turn to
    /// drain the declared intents. The matching discharge is the guard's drop,
    /// so a body that exits between the two — by error return, cancellation or
    /// unwind — still releases the next slot.
    pub(crate) async fn begin_final_drain(&self) {
        self.final_result_committed.send_replace(true);
        self.gate.wait_for(self.index).await;
    }
}

impl Drop for IntentDrainGuard {
    fn drop(&mut self) {
        self.gate.discharge(self.index);
    }
}

/// The batch-side view of whether a child committed its final result.
pub(crate) struct IntentDrainCommitSignal {
    // Retaining a sender keeps the channel open for this handle's whole life,
    // so `committed` never resolves merely because the child's guard was
    // dropped. Only a real commit resolves it; the caller's grace timer decides
    // every other case, exactly as it did when the batch held a second slot
    // handle for this purpose.
    _sender: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl IntentDrainCommitSignal {
    pub(crate) fn is_committed(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolves once the child has committed its final result, and otherwise
    /// stays pending.
    pub(crate) async fn committed(&mut self) {
        let closed = self
            .receiver
            .wait_for(|committed| *committed)
            .await
            .is_err();
        if closed {
            // Unreachable while `_sender` is held; never report a closed
            // channel as a commit.
            std::future::pending::<()>().await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn coordinate_tool_invocation<'run>(
    context: &ToolDispatchContext<'run>,
    call: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    retry_policy: ToolRetryPolicy,
    identity: ToolAttemptEffectIdentity,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    // Owned for the whole coordination: the guard discharges its drain slot on
    // drop, so every return below — terminal, pending or failed — releases the
    // next slot without a hand-written call.
    mut intent_drain_slot: Option<IntentDrainGuard>,
    child_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    mut local_executor: impl FnMut(Option<crate::AwaitEventKey>) -> RuntimeEffectLocalExecutor<'run>,
) -> CoordinatedToolInvocation {
    let started_at = context.clock.now();
    let max_attempts = retry_policy.max_attempts().max(1);
    let mut triggers = Vec::new();
    let mut attempts = Vec::new();

    for attempt in 1..=max_attempts {
        let completion_key = if context.tools.attempt_may_defer(&call.tool_id)
            && context
                .effect_controller
                .controller()
                .allows_process_lifetime_completion_keys()
        {
            match context
                .effect_controller
                .controller()
                .await_event_key(
                    context.effect_controller.scoped().execution_scope(),
                    crate::AwaitEventWaitIdentity::tool_completion(call.call_id.clone()),
                )
                .await
            {
                Ok(key) => Some(key),
                Err(err) => {
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
                            &call,
                            "tool_completion_key_prederive_failed",
                            err.to_string(),
                            identity.duration_ms(context, started_at, 0),
                            attempts,
                        ))),
                        triggers,
                    };
                }
            }
        } else {
            None
        };
        let invocation = identity.attempt_invocation(context, &call, attempt);
        let outcome = context
            .effect_controller
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::ToolAttempt {
                        call: call.clone(),
                        execution_grant: execution_grant.clone(),
                        attempt,
                        max_attempts,
                    },
                ),
                local_executor(completion_key),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_tool_attempt_effect);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
                        &call,
                        "tool_attempt_failed",
                        err.to_string(),
                        identity.duration_ms(context, started_at, 0),
                        attempts,
                    ))),
                    triggers,
                };
            }
        };
        if let crate::ToolAttemptLaunch::Done { record, .. } = &outcome.launch
            && let crate::ToolCallOutcome::Failure(failure) = &record.output.outcome
            && failure.code == "tool_panicked"
        {
            crate::panic_containment::enforce_message("tool_panicked", &failure.message);
        }
        triggers.extend(outcome.triggers);
        match outcome.launch {
            crate::ToolAttemptLaunch::Pending {
                key,
                pending,
                duration_ms,
            } => {
                let duration_ms = identity.duration_ms(context, started_at, duration_ms);
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::Pending(Box::new(PendingToolDispatchOutcome {
                        tool_name: call.tool_name,
                        args: call.args,
                        key: *key,
                        pending,
                        duration_ms,
                        attempts,
                    })),
                    triggers,
                };
            }
            crate::ToolAttemptLaunch::Done {
                mut record,
                intents,
            } => {
                // Admission uses the identity committed by the attempt. The
                // projection below still normalizes the host-facing record,
                // but must not repair a malformed durable attempt before the
                // intent executor has had a chance to refuse it.
                let recorded_call_id = record.call_id.clone();
                record.call_id = Some(call.call_id.clone());
                let retry_after = retry_after_ms(
                    &ToolOutcome::from_output(record.output.clone()),
                    retry_policy,
                    attempt - 1,
                );
                attempts.push(crate::trace::trace_tool_attempt(
                    attempt,
                    record.as_ref(),
                    (attempt < max_attempts).then_some(retry_after).flatten(),
                ));
                record.duration_ms = identity.duration_ms(context, started_at, record.duration_ms);
                let Some(retry_after) = retry_after else {
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(
                            settle_terminal_attempt(
                                context,
                                intent_drain_slot.take(),
                                child_trace_hook.as_ref(),
                                recorded_call_id.as_deref(),
                                record,
                                intents,
                                attempts,
                            )
                            .await,
                        )),
                        triggers,
                    };
                };
                if attempt >= max_attempts {
                    let exhausted =
                        mark_retry_exhausted(ToolOutcome::from_output(record.output), attempt);
                    record.output = exhausted.into_done_output().unwrap_or_else(|_| {
                        ToolCallOutput::failure(ToolFailure::runtime(
                            ToolFailureClass::Internal,
                            "tool_retry_exhaustion_failed",
                            "retry exhaustion produced a pending output",
                        ))
                    });
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(
                            settle_terminal_attempt(
                                context,
                                intent_drain_slot.take(),
                                child_trace_hook.as_ref(),
                                recorded_call_id.as_deref(),
                                record,
                                intents,
                                attempts,
                            )
                            .await,
                        )),
                        triggers,
                    };
                }
                if retry_after > 0
                    && let Err(err) = sleep_before_retry(
                        context,
                        identity.retry_sleep_invocation(context, &call, attempt),
                        cancellation.clone(),
                        retry_after,
                    )
                    .await
                {
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(runtime_failure_outcome(
                            &call,
                            "tool_retry_sleep_failed",
                            format!(
                                "retry sleep for tool `{}` failed after attempt {attempt}: {err}",
                                call.tool_name
                            ),
                            identity.duration_ms(context, started_at, 0),
                            attempts,
                        ))),
                        triggers,
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
            identity.duration_ms(context, started_at, 0),
            attempts,
        ))),
        triggers,
    }
}

/// Settles one terminal tool attempt: drain the declared intents in this
/// batch's source order, project their outcomes onto the record, and report
/// them.
///
/// Both terminal callers — a first attempt with no retry left to schedule, and
/// a retry-exhausted attempt — reach the same terminal state and run this one
/// body. Taking the drain guard by value makes the settlement window the
/// guard's lifetime: the slot is claimed for the whole drain and discharged
/// when this body ends, on every path out of it.
async fn settle_terminal_attempt(
    context: &ToolDispatchContext<'_>,
    intent_drain_slot: Option<IntentDrainGuard>,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
    recorded_call_id: Option<&str>,
    mut record: Box<ToolCallRecord>,
    intents: crate::ToolIntents,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
) -> ToolDispatchOutcome {
    if let Some(slot) = &intent_drain_slot {
        slot.begin_final_drain().await;
    }
    let intent_outcomes =
        super::execute_final_tool_intents(context, recorded_call_id, &intents, child_trace_hook)
            .await;
    project_recorded_intent_outcomes(&mut record.output, &intent_outcomes);
    context.recorded_intent_outcomes.record(&intent_outcomes);
    // Discharges the drain slot, where both former bodies called
    // `complete_final_drain`. Written out so the release point stays explicit
    // even though the guard would do it at the end of this scope anyway.
    drop(intent_drain_slot);
    ToolDispatchOutcome {
        record: *record,
        attempts,
        intents,
        intent_outcomes,
    }
}

fn project_recorded_intent_outcomes(
    output: &mut crate::ToolCallOutput,
    outcomes: &[crate::ToolIntentExecutionOutcome],
) {
    // Only a refusal produced while executing a declared intent can supersede
    // the provider's optimistic output. Batch-admission refusals describe the
    // intent protocol itself and stay in the typed intent-outcome stream.
    if let Some(crate::ToolIntentExecutionOutcome::Refused {
        refusal: crate::ToolIntentRefusalReason::CommandFailed { code, message },
        ..
    }) = outcomes.iter().find(|outcome| {
        matches!(
            outcome,
            crate::ToolIntentExecutionOutcome::Refused {
                refusal: crate::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            }
        )
    }) {
        // ADR 0042 deliberately seals the ToolAttempt terminal before draining
        // intents. Its optimistic provider value is therefore immutable; the
        // child process-command journal row is the authoritative refusal, and
        // this turn projection plus `intent_outcomes` expose that evidence.
        // Rewriting the completed parent row here would break journal-first
        // settlement and append-only replay.
        *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Unavailable,
            code.clone(),
            message.clone(),
        ));
        return;
    }

    let Some(sequence) = outcomes.iter().find_map(|outcome| match outcome {
        crate::ToolIntentExecutionOutcome::Executed {
            kind: crate::ToolIntentKind::SignalProcess,
            result,
            ..
        } => result.get("sequence").and_then(serde_json::Value::as_u64),
        _ => None,
    }) else {
        return;
    };
    let crate::ToolCallOutcome::Success(value) = &mut output.outcome else {
        return;
    };
    let mut projected = value.to_json_value();
    let Some(object) = projected.as_object_mut() else {
        return;
    };
    object.insert("sequence".to_string(), serde_json::json!(sequence));
    *value = crate::ToolValue::from(projected);
}

fn runtime_failure_outcome(
    call: &PreparedToolCall,
    code: impl Into<String>,
    message: impl Into<String>,
    duration_ms: u64,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
) -> ToolDispatchOutcome {
    ToolDispatchOutcome {
        record: ToolCallRecord {
            call_id: Some(call.call_id.clone()),
            tool: call.tool_name.clone(),
            args: call.args.clone(),
            output: ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::Internal,
                code,
                message,
            )),
            duration_ms,
        },
        attempts,
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

async fn sleep_before_retry(
    context: &ToolDispatchContext<'_>,
    invocation: RuntimeInvocation,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    retry_after_ms: u64,
) -> Result<(), crate::RuntimeEffectControllerError> {
    let outcome = context
        .effect_controller
        .controller()
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::Sleep {
                    duration_ms: retry_after_ms,
                },
            ),
            RuntimeEffectLocalExecutor::sleep_with_clock(
                cancellation.unwrap_or_default(),
                std::sync::Arc::clone(&context.clock),
            )
            .with_turn_cancel_scope(context.effect_controller.scoped().execution_scope().clone()),
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

    fn refusal(reason: crate::ToolIntentRefusalReason) -> crate::ToolIntentExecutionOutcome {
        crate::ToolIntentExecutionOutcome::Refused {
            identity: None,
            intent_index: 0,
            kind: crate::ToolIntentKind::SignalProcess,
            refusal: reason,
        }
    }

    #[test]
    fn command_refusal_projects_its_typed_code_and_message() {
        let mut output = crate::ToolCallOutput::success(serde_json::json!("optimistic"));
        project_recorded_intent_outcomes(
            &mut output,
            &[refusal(crate::ToolIntentRefusalReason::CommandFailed {
                code: "process_not_visible".to_string(),
                message: "process is outside the invoking session".to_string(),
            })],
        );

        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("intent command refusal must supersede optimistic success")
        };
        assert_eq!(failure.code, "process_not_visible");
        assert_eq!(failure.message, "process is outside the invoking session");
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
}
