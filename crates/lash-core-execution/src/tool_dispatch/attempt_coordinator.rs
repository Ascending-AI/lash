use crate::ProcessId;
use crate::{
    PreparedToolCall, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeInvocation, ToolCallOutput, ToolCallRecord, ToolFailure, ToolFailureClass, ToolOutcome,
    ToolRetryPolicy,
};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;

use super::{
    PendingToolDispatchOutcome, ToolCallLaunch, ToolDispatchContext, ToolDispatchOutcome,
    ToolTriggerEffectOutcome, mark_retry_exhausted, retry_after_ms,
};

/// Which family of replay keys and causal parent a tool call's attempts derive
/// from.
///
/// Serializable because a group child retains it: ADR 0099 §3 requires a tool
/// child of an effect group to be reconstructible from the journal alone, and
/// the attempt identity is how a recovered child re-derives the *same* replay
/// key for the *same* attempt instead of issuing a fresh unrelated one (W2).
/// It is carried whole rather than mirrored into a durable twin, because the
/// twin and this type would be two spellings of one fact — see
/// [`ToolChildRequest`](crate::runtime::effect::ToolChildRequest).
///
/// The parent invocation each arm carries is the dispatch context's
/// `parent_invocation`, so this type is also a tool call's lineage.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolAttemptEffectIdentity {
    Scalar {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<RuntimeInvocation>,
    },
    Batch {
        parent: RuntimeInvocation,
        replay_suffix: String,
    },
    Process {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<RuntimeInvocation>,
        process_id: ProcessId,
    },
}

impl ToolAttemptEffectIdentity {
    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn attempt_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeEffectInvocation {
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
                context.effect_controller.scoped().execution_scope(),
                parent,
                format!("{parent_effect_id}:{suffix}"),
                RuntimeEffectKind::ToolAttempt,
                suffix,
            );
        }

        let effect_id = format!("tool:{suffix}");
        RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                context.effect_controller.scoped().execution_scope().clone(),
                effect_id.clone(),
            )
            .expect("tool dispatch carries an admitted effect scope"),
            context.parentless_attribution(),
            effect_id.clone(),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    fn retry_sleep_invocation(
        &self,
        context: &ToolDispatchContext<'_>,
        call: &PreparedToolCall,
        attempt: u32,
    ) -> RuntimeEffectInvocation {
        if let Self::Batch {
            parent,
            replay_suffix,
        } = self
        {
            let suffix = format!("{replay_suffix}:attempt:{attempt}:sleep");
            let parent_effect_id = parent.effect_id().unwrap_or("tool-batch");
            return crate::runtime::causal::child_effect_invocation(
                context.effect_controller.scoped().execution_scope(),
                parent,
                format!("{parent_effect_id}:{suffix}"),
                RuntimeEffectKind::Sleep,
                suffix,
            );
        }
        if let Some(parent) = self.parent() {
            return crate::runtime::tool_retry_sleep_invocation(
                context.effect_controller.scoped().execution_scope(),
                parent,
                &call.tool_name,
                attempt,
            );
        }

        let replay_base = match self {
            Self::Process { process_id, .. } => {
                format!("process:{process_id}:tool:{}", call.tool_name)
            }
            Self::Scalar { .. } => format!(
                "lash-tool:{}:{}:{}",
                context.session_id, call.call_id, call.tool_name
            ),
            Self::Batch { .. } => unreachable!("batch retry sleeps return above"),
        };
        let effect_id = format!("{replay_base}:attempt:{attempt}:sleep");
        RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                context.effect_controller.scoped().execution_scope().clone(),
                effect_id.clone(),
            )
            .expect("tool retry carries an admitted effect scope"),
            context.parentless_attribution(),
            effect_id.clone(),
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

pub struct CoordinatedToolInvocation {
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
pub struct BatchIntentDrainGate {
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
pub struct IntentDrainGuard {
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
pub async fn coordinate_tool_invocation<'run>(
    context: &ToolDispatchContext<'run>,
    call: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    retry_policy: ToolRetryPolicy,
    identity: ToolAttemptEffectIdentity,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
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
        let completion_key = match context
            .effect_controller
            .controller()
            .prepare_completion_key(
                context.effect_controller.scoped().execution_scope(),
                crate::AwaitEventWaitIdentity::tool_completion(call.call_id.clone()),
                context.attempt_may_defer(&call.tool_id, execution_grant.as_deref()),
            )
            .await
        {
            Ok(crate::CompletionKeyPreparation::Issued(key)) => Some(key),
            Ok(crate::CompletionKeyPreparation::NotNeeded)
            | Ok(crate::CompletionKeyPreparation::Unsupported) => None,
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
        };
        let invocation = identity.attempt_invocation(context, &call, attempt);
        let outcome = context
            .effect_controller
            .scoped()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation.clone(),
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
                        launch: match settle_terminal_attempt(
                            context,
                            TerminalAttemptSettlement {
                                minting_emission: &invocation,
                                intent_drain_slot: intent_drain_slot.take(),
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: recorded_call_id.as_deref(),
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
                        launch: match settle_terminal_attempt(
                            context,
                            TerminalAttemptSettlement {
                                minting_emission: &invocation,
                                intent_drain_slot: intent_drain_slot.take(),
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: recorded_call_id.as_deref(),
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
                        triggers,
                    };
                }
                if retry_after > 0
                    && let Err(err) = sleep_before_retry(
                        context,
                        identity.retry_sleep_invocation(context, &call, attempt),
                        turn_cancel_wait,
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
struct TerminalAttemptSettlement<'settlement> {
    minting_emission: &'settlement RuntimeEffectInvocation,
    intent_drain_slot: Option<IntentDrainGuard>,
    child_trace_hook: Option<&'settlement crate::ToolChildExecutionTraceHook>,
    recorded_call_id: Option<&'settlement str>,
    record: Box<ToolCallRecord>,
    intents: crate::ToolIntents,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
}

async fn settle_terminal_attempt(
    context: &ToolDispatchContext<'_>,
    settlement: TerminalAttemptSettlement<'_>,
) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
    let TerminalAttemptSettlement {
        minting_emission,
        intent_drain_slot,
        child_trace_hook,
        recorded_call_id,
        mut record,
        intents,
        attempts,
    } = settlement;
    if let Some(slot) = &intent_drain_slot {
        slot.begin_final_drain().await;
    }
    let mut intent_context = context.clone();
    intent_context.parent_invocation = Some(minting_emission.clone().into_runtime_invocation());
    let intent_outcomes = super::execute_final_tool_intents(
        &intent_context,
        recorded_call_id,
        &intents,
        child_trace_hook,
    )
    .await?;
    project_recorded_intent_outcomes(&mut record.output, &intent_outcomes);
    // Discharges the drain slot, where both former bodies called
    // `complete_final_drain`. Written out so the release point stays explicit
    // even though the guard would do it at the end of this scope anyway.
    drop(intent_drain_slot);
    Ok(ToolDispatchOutcome {
        record: *record,
        attempts,
        intents,
        intent_outcomes,
    })
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

    let answers = projected_intent_answers(outcomes);
    if answers.is_empty() {
        return;
    }
    let crate::ToolCallOutcome::Success(value) = &mut output.outcome else {
        return;
    };
    // Only the attempt's own optimistic answer is rewritten. An attempt that
    // answered something else keeps what it answered (FIG-3119).
    let Some(projected) =
        matching_answer(&value.to_json_value(), &answers).map(|answer| answer.fields.clone())
    else {
        return;
    };
    match value {
        crate::ToolValue::Object(object) => {
            for (name, field) in &projected {
                object.insert(
                    name.clone(),
                    match serde_json::from_value(field.clone()) {
                        Ok(decoded) => decoded,
                        Err(_) => return,
                    },
                );
            }
        }
        crate::ToolValue::UntrustedJson(serde_json::Value::Object(object)) => {
            for (name, field) in &projected {
                object.insert(name.clone(), field.clone());
            }
        }
        _ => return,
    }
    let encoded = match serde_json::to_value(&*value) {
        Ok(encoded) => encoded,
        Err(error) => {
            *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "tool_value_encode_failed",
                format!("failed to encode projected tool value: {error}"),
            ));
            return;
        }
    };
    match serde_json::from_value(encoded) {
        Ok(decoded) => *value = decoded,
        Err(error) => {
            *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "tool_value_decode_failed",
                format!("malformed projected tool value: {error}"),
            ));
        }
    }
}

/// The realized answer one intent contributes back to its declaring attempt's
/// optimistic output, and the process that answer is about.
///
/// An attempt seals its output before its intents run, so anything only the
/// realization knows has to travel back through here. Two facts do. A signal's
/// `sequence` is the position the append landed at. A start's handle is the
/// whole answer: the process handle carries the incarnation the registry
/// allocated, which no attempt can predict, so the declaration answers a handle
/// that names no incarnation and the realized one replaces it here. Nothing
/// else is copied — `incarnation`, `status` and the rest of the realized view
/// are facts a holder reads through the process tools, and spelling the
/// incarnation beside the id is exactly what ADR 0095 forbids.
///
/// `process_id` is what makes this a replacement rather than a merge. A
/// realized answer belongs to the output that already named the same process:
/// `start_process` answers the unrealized handle for exactly this id
/// (`unrealized_start_handle`), `signal_process` answers `{process_id, signal}`.
/// Any other output — a tool that returns its own data and happens to declare a
/// start alongside it — is not answering a handle, and merging one into it put
/// `__handle__`, `id` and `process_id` into the model-facing text of every such
/// tool result, and made the last of several starts overwrite the others
/// (FIG-3119).
struct ProjectedIntentAnswer {
    process_id: String,
    /// Set when the answer replaces a handle, so the output has to be one.
    replaces_handle: bool,
    fields: Vec<(String, serde_json::Value)>,
}

/// Picks the one realized answer this output is the optimistic form of.
fn matching_answer<'a>(
    current: &serde_json::Value,
    answers: &'a [ProjectedIntentAnswer],
) -> Option<&'a ProjectedIntentAnswer> {
    let object = current.as_object()?;
    let named = object
        .get("process_id")
        .and_then(serde_json::Value::as_str)?;
    answers.iter().find(|answer| {
        answer.process_id == named
            && (!answer.replaces_handle || object.contains_key(lash_sansio::handle::HANDLE_FIELD))
    })
}

fn projected_intent_answers(
    outcomes: &[crate::ToolIntentExecutionOutcome],
) -> Vec<ProjectedIntentAnswer> {
    let mut answers = Vec::new();
    for outcome in outcomes {
        let crate::ToolIntentExecutionOutcome::Executed { kind, result, .. } = outcome else {
            continue;
        };
        let Some(process_id) = result
            .get("process_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        match kind {
            crate::ToolIntentKind::SignalProcess => {
                if let Some(sequence) = result.get("sequence").and_then(serde_json::Value::as_u64) {
                    answers.push(ProjectedIntentAnswer {
                        process_id,
                        replaces_handle: false,
                        fields: vec![("sequence".to_string(), serde_json::json!(sequence))],
                    });
                }
            }
            crate::ToolIntentKind::StartProcess => {
                let mut fields = Vec::new();
                for name in [lash_sansio::handle::HANDLE_FIELD, "id", "process_id"] {
                    if let Some(field) = result.get(name) {
                        fields.push((name.to_string(), field.clone()));
                    }
                }
                if !fields.is_empty() {
                    answers.push(ProjectedIntentAnswer {
                        process_id,
                        replaces_handle: true,
                        fields,
                    });
                }
            }
            _ => {}
        }
    }
    answers
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
    invocation: RuntimeEffectInvocation,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
    retry_after_ms: u64,
) -> Result<(), crate::RuntimeEffectControllerError> {
    let outcome = context
        .effect_controller
        .scoped()
        .execute_effect(
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
            kind: crate::ToolIntentKind::SignalProcess,
            refusal: reason,
        }
    }

    fn executed(
        kind: crate::ToolIntentKind,
        result: serde_json::Value,
    ) -> crate::ToolIntentExecutionOutcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity: crate::ToolIntentIdentity {
                session_id: SessionId::from("session"),
                execution_scope_id: "turn".to_string(),
                tool_call_id: "call".to_string(),
                intent_index: 0,
                replay_key: "replay".to_string(),
                minting_emission_replay_key: None,
            },
            kind,
            result,
        }
    }

    fn signal_outcome(process_id: &str, sequence: u64) -> crate::ToolIntentExecutionOutcome {
        executed(
            crate::ToolIntentKind::SignalProcess,
            serde_json::json!({ "process_id": process_id, "sequence": sequence }),
        )
    }

    /// The handle a start answers before its declaration is realized, as
    /// `lash_plugin_process_controls::declarations` writes it.
    fn unrealized_handle(process_id: &str) -> serde_json::Value {
        serde_json::json!({
            "__handle__": "lash",
            "id": format!("p.0.{process_id}"),
            "process_id": process_id,
        })
    }

    fn start_outcome(process_id: &str, incarnation: u64) -> crate::ToolIntentExecutionOutcome {
        executed(
            crate::ToolIntentKind::StartProcess,
            serde_json::json!({
                "__handle__": "lash",
                "id": format!("p.{incarnation}.{process_id}"),
                "process_id": process_id,
                "incarnation": incarnation,
                "kind": "external",
                "status": "running",
            }),
        )
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

    #[test]
    fn signal_projection_rejects_a_malformed_tagged_tool_value() {
        let mut output = crate::ToolCallOutput::success_tool_value(crate::ToolValue::Object(
            std::collections::BTreeMap::from([
                (
                    "$lash_tool_value".to_string(),
                    crate::ToolValue::String("attachment".to_string()),
                ),
                (
                    "process_id".to_string(),
                    crate::ToolValue::String("p-signalled".to_string()),
                ),
            ]),
        ));

        project_recorded_intent_outcomes(&mut output, &[signal_outcome("p-signalled", 7)]);

        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("a malformed projected tag must become a typed decode failure");
        };
        assert_eq!(failure.code, "tool_value_decode_failed");
    }

    #[test]
    fn a_realized_start_replaces_the_unrealized_handle_its_attempt_answered() {
        let mut output = crate::ToolCallOutput::success(unrealized_handle("p-child"));

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 2)]);

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({
                "__handle__": "lash",
                "id": "p.2.p-child",
                "process_id": "p-child",
            }),
            "the realized handle replaces the unrealized one, and nothing else is copied"
        );
    }

    #[test]
    fn a_realized_start_leaves_an_output_that_is_not_its_handle_alone() {
        // A tool that answers its own data and declares a start alongside it.
        // Merging the realized handle into that answer put `__handle__`, `id`
        // and `process_id` into the model-facing text of the tool result
        // (FIG-3119).
        let mut output = crate::ToolCallOutput::success(serde_json::json!({ "ok": true }));

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 2)]);

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({ "ok": true })
        );
    }

    #[test]
    fn an_output_naming_a_process_without_being_a_handle_keeps_its_own_shape() {
        // Same id, but no handle to replace: a start's answer is a handle, so
        // an output that is not one is not the optimistic form of it.
        let mut output = crate::ToolCallOutput::success(
            serde_json::json!({ "process_id": "p-child", "ok": true }),
        );

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 2)]);

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({ "process_id": "p-child", "ok": true })
        );
    }

    #[test]
    fn several_starts_project_the_one_the_output_names() {
        // Field-by-field merging let the last declared start overwrite the
        // handle of the one the attempt actually answered with (FIG-3119).
        let mut output = crate::ToolCallOutput::success(unrealized_handle("p-first"));

        project_recorded_intent_outcomes(
            &mut output,
            &[start_outcome("p-first", 1), start_outcome("p-second", 4)],
        );

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({
                "__handle__": "lash",
                "id": "p.1.p-first",
                "process_id": "p-first",
            })
        );
    }

    #[test]
    fn a_realized_signal_projects_its_sequence_onto_the_process_it_signalled() {
        let mut output = crate::ToolCallOutput::success(
            serde_json::json!({ "process_id": "p-target", "signal": "resume" }),
        );

        project_recorded_intent_outcomes(
            &mut output,
            &[signal_outcome("p-other", 3), signal_outcome("p-target", 11)],
        );

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({
                "process_id": "p-target",
                "signal": "resume",
                "sequence": 11,
            })
        );
    }
}
