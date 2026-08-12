use crate::facade_support::ScopedEffectControllerFacadeOps;
use crate::{
    PreparedToolCall, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeInvocation,
    ToolCallOutput, ToolCallRecord, ToolFailure, ToolFailureClass, ToolResult, ToolRetryPolicy,
};
use lash_sansio::core_support::*;

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

#[derive(Default)]
pub(crate) struct BatchIntentDrainGate {
    next: tokio::sync::Mutex<usize>,
    changed: tokio::sync::Notify,
}

impl BatchIntentDrainGate {
    async fn wait_for(&self, index: usize) {
        loop {
            let changed = self.changed.notified();
            if *self.next.lock().await == index {
                return;
            }
            changed.await;
        }
    }

    async fn advance(&self, index: usize) {
        let mut next = self.next.lock().await;
        debug_assert_eq!(*next, index, "intent drains advance in source order");
        *next = next.saturating_add(1);
        drop(next);
        self.changed.notify_waiters();
    }
}

#[derive(Clone)]
pub(crate) struct IntentDrainSlot {
    gate: std::sync::Arc<BatchIntentDrainGate>,
    index: usize,
    final_result_committed: tokio::sync::watch::Sender<bool>,
}

impl IntentDrainSlot {
    pub(crate) fn new(
        gate: std::sync::Arc<BatchIntentDrainGate>,
        index: usize,
    ) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (final_result_committed, receiver) = tokio::sync::watch::channel(false);
        (
            Self {
                gate,
                index,
                final_result_committed,
            },
            receiver,
        )
    }

    fn mark_final_result_committed(&self) {
        self.final_result_committed.send_replace(true);
    }

    pub(crate) async fn finish(&self) {
        self.gate.wait_for(self.index).await;
        self.gate.advance(self.index).await;
    }

    async fn begin_final_drain(&self) {
        self.mark_final_result_committed();
        self.gate.wait_for(self.index).await;
    }

    async fn complete_final_drain(&self) {
        self.gate.advance(self.index).await;
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
    intent_drain_slot: Option<IntentDrainSlot>,
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
                    if let Some(slot) = &intent_drain_slot {
                        slot.finish().await;
                    }
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
                if let Some(slot) = &intent_drain_slot {
                    slot.finish().await;
                }
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
                if let Some(slot) = &intent_drain_slot {
                    slot.finish().await;
                }
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
                    &ToolResult::from_output(record.output.clone()),
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
                    if let Some(slot) = &intent_drain_slot {
                        slot.begin_final_drain().await;
                    }
                    let intent_outcomes = super::execute_final_tool_intents(
                        context,
                        recorded_call_id.as_deref(),
                        &intents,
                        child_trace_hook.as_ref(),
                    )
                    .await;
                    super::intent_executor::record_parent_end_actions(
                        context,
                        &intents,
                        &intent_outcomes,
                    );
                    if let Some(slot) = &intent_drain_slot {
                        slot.complete_final_drain().await;
                    }
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(ToolDispatchOutcome {
                            record: *record,
                            attempts,
                            intents,
                            intent_outcomes,
                        })),
                        triggers,
                    };
                };
                if attempt >= max_attempts {
                    let exhausted =
                        mark_retry_exhausted(ToolResult::from_output(record.output), attempt);
                    record.output = exhausted.into_done_output().unwrap_or_else(|_| {
                        ToolCallOutput::failure(ToolFailure::runtime(
                            ToolFailureClass::Internal,
                            "tool_retry_exhaustion_failed",
                            "retry exhaustion produced a pending output",
                        ))
                    });
                    if let Some(slot) = &intent_drain_slot {
                        slot.begin_final_drain().await;
                    }
                    let intent_outcomes = super::execute_final_tool_intents(
                        context,
                        recorded_call_id.as_deref(),
                        &intents,
                        child_trace_hook.as_ref(),
                    )
                    .await;
                    super::intent_executor::record_parent_end_actions(
                        context,
                        &intents,
                        &intent_outcomes,
                    );
                    if let Some(slot) = &intent_drain_slot {
                        slot.complete_final_drain().await;
                    }
                    return CoordinatedToolInvocation {
                        launch: ToolCallLaunch::Done(Box::new(ToolDispatchOutcome {
                            record: *record,
                            attempts,
                            intents,
                            intent_outcomes,
                        })),
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
                    if let Some(slot) = &intent_drain_slot {
                        slot.finish().await;
                    }
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

    if let Some(slot) = &intent_drain_slot {
        slot.finish().await;
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
