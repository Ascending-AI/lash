use crate::ProcessId;
use crate::{
    PreparedToolCall, RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeInvocation,
    ToolCallOutput, ToolCallRecord, ToolFailure, ToolFailureClass, ToolOutcome, ToolRetryPolicy,
};
use lash_sansio::core_support::*;

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
    /// One call a replayed language program issued as a command (FIG-3586):
    /// `command` is the [`command_invocation`](crate::runtime::command_invocation)
    /// at the command's replay key, and every attempt, retry sleep and
    /// deferred-completion await of the call is a child of it —
    /// `{command}:attempt:{n}`, `{command}:attempt:{n}:sleep`,
    /// `{command}:await`. Neither the call id nor the tool name is key
    /// material: two calls of one tool in one run are two commands.
    Command { command: RuntimeInvocation },
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
        let suffix = match self {
            Self::Scalar { .. } => format!("{}:attempt:{attempt}", call.call_id),
            Self::Batch { replay_suffix, .. } => format!("{replay_suffix}:attempt:{attempt}"),
            Self::Command { .. } => format!("attempt:{attempt}"),
            Self::Process { process_id, .. } => {
                format!(
                    "process:{process_id}:tool:{}:attempt:{attempt}",
                    call.tool_name
                )
            }
        };
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
        if let Self::Command { command } = self {
            let suffix = format!("attempt:{attempt}:sleep");
            let command_effect_id = command.effect_id().unwrap_or("command");
            return crate::runtime::causal::child_effect_invocation(
                context.effect_controller.scoped().execution_scope(),
                command,
                format!("{command_effect_id}:{suffix}"),
                suffix,
            );
        }
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
            Self::Batch { .. } | Self::Command { .. } => {
                unreachable!("batch and command retry sleeps return above")
            }
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

    /// The parent invocation this identity's attempts descend from.
    ///
    /// Public because a tool child of an effect group reconstructs its lineage
    /// out of its *recorded* identity and has no caller to ask (ADR 0099 §3):
    /// every arm holds the parent, which is why the retained request carries no
    /// separate lineage field.
    pub fn parent_invocation(&self) -> Option<&RuntimeInvocation> {
        self.parent()
    }

    fn parent(&self) -> Option<&RuntimeInvocation> {
        match self {
            Self::Scalar { parent } => parent.as_ref(),
            Self::Process { parent, .. } => parent.as_ref(),
            Self::Batch { parent, .. } => Some(parent),
            Self::Command { command } => Some(command),
        }
    }
}

pub struct CoordinatedToolInvocation {
    pub launch: ToolCallLaunch,
}

/// What a tool child of an effect group carries into coordination and a live
/// caller cannot: the completion routing it was admitted under (ADR 0099 §3)
/// and the address of its own replay row — the §4 linearization point (ADR
/// 0099 §4). `None` for every caller that admitted its call live.
///
/// A live admission answers `None` for both halves: deferral is read from the
/// live registry or provider, which is what admitted the call a moment ago,
/// and there is no group membership to commit against. A **tool child of an
/// effect group** answers `Some`, because §3 makes the *recorded* admission
/// authoritative — "a reopen uses the recorded facts, not current session
/// policy or fresh admission" — and §4 makes the child's own replay row the
/// commit boundary its final record must reach.
#[derive(Clone, Debug)]
pub struct GroupChildCoordination {
    pub completion_routing: crate::runtime::ToolChildCompletionRouting,
    /// The child's `ToolInvocation` envelope address: its execution scope and
    /// replay key.
    pub child: crate::EffectAddress,
}

/// Refuses a child whose recorded routing this deployment cannot honour.
///
/// The routing mismatch ADR 0099 §3 amendment 2 names: "The request records
/// which of `inline` or `durable` the child was admitted under, so a recovered
/// child never derives a key nothing will resolve." A
/// child admitted with a durable completion key that lands on a host issuing
/// none would park on a key no resolver can reach, and a child admitted inline
/// that suddenly acquires a key would defer where its opener expects a value.
/// Both are refusals, never a repaired derivation.
fn completion_routing_mismatch(
    recorded: crate::runtime::ToolChildCompletionRouting,
    observed: &str,
    call: &PreparedToolCall,
) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting,
        format!(
            "tool child `{}` was admitted under {recorded:?} completion routing and this \
             deployment answers {observed}; a recovered child is refused rather than run under \
             a key its opener cannot resolve",
            call.call_id
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn coordinate_tool_invocation<'run>(
    context: &ToolDispatchContext<'run>,
    call: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    retry_policy: ToolRetryPolicy,
    // `None` for every caller that admitted this call live; `Some` only for a
    // group child running from its retained request. See
    // [`GroupChildCoordination`].
    group_child: Option<GroupChildCoordination>,
    identity: ToolAttemptEffectIdentity,
    turn_cancel_wait: &crate::runtime::TurnCancelWait,
    child_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    mut local_executor: impl FnMut(Option<crate::AwaitEventKey>) -> RuntimeEffectLocalExecutor<'run>,
) -> CoordinatedToolInvocation {
    let max_attempts = retry_policy.max_attempts().max(1);
    let mut triggers = Vec::new();
    let mut captures = Vec::new();
    let mut attempts = Vec::new();

    // Whether this attempt may defer is a recorded fact for a group child and a
    // live one for everyone else. Read once, above the loop, because every
    // attempt of one invocation is admitted under the same authority.
    let may_defer = match group_child.as_ref().map(|child| &child.completion_routing) {
        None => context.attempt_may_defer(&call.tool_id, execution_grant.as_deref()),
        Some(crate::runtime::ToolChildCompletionRouting::Inline) => false,
        Some(crate::runtime::ToolChildCompletionRouting::Durable) => true,
    };

    for attempt in 1..=max_attempts {
        let prepared_key = context
            .effect_controller
            .controller()
            .prepare_completion_key(
                context.effect_controller.scoped().execution_scope(),
                crate::AwaitEventWaitIdentity::tool_completion(call.call_id.clone()),
                may_defer,
            )
            .await;
        if let Some(recorded) = group_child.as_ref().map(|child| &child.completion_routing) {
            let observed = match &prepared_key {
                Ok(crate::CompletionKeyPreparation::Issued(_)) => "issued",
                Ok(crate::CompletionKeyPreparation::NotNeeded) => "not-needed",
                Ok(crate::CompletionKeyPreparation::Unsupported) => "unsupported",
                Err(_) => "",
            };
            let honoured = match recorded {
                crate::runtime::ToolChildCompletionRouting::Inline => observed == "not-needed",
                crate::runtime::ToolChildCompletionRouting::Durable => observed == "issued",
            };
            if !observed.is_empty() && !honoured {
                abandon_to_open_buffers(context, triggers, captures);
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::ControllerAborted(completion_routing_mismatch(
                        recorded.clone(),
                        observed,
                        &call,
                    )),
                };
            }
        }
        let completion_key = match prepared_key {
            Ok(crate::CompletionKeyPreparation::Issued(key)) => Some(key),
            Ok(crate::CompletionKeyPreparation::NotNeeded)
            | Ok(crate::CompletionKeyPreparation::Unsupported) => None,
            // A completion-key prederive failure is a controller error, not a
            // tool result: it must abort like the attempt's own journal faults
            // do, so nothing the store reported reaches the model (FIG-3528).
            Err(err) => {
                abandon_to_open_buffers(context, triggers, captures);
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::ControllerAborted(err.into()),
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
                        captures,
                        triggers,
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
                abandon_to_open_buffers(context, triggers, captures);
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
        triggers.extend(outcome.triggers);
        // The attempt's journaled facts ride the outcome to the incorporation
        // boundary: on a live execution this moves them out of the
        // attempt-local buffers the runner installed, and on replay the
        // journaled capture is the only place they exist. Either way they are
        // consumed exactly once per attempt outcome, which is what makes a
        // replayed attempt indistinguishable from a fresh one (ADR 0099 §13).
        captures.push(outcome.capture);
        match outcome.launch {
            crate::ToolAttemptLaunch::Pending { key, pending } => {
                return CoordinatedToolInvocation {
                    launch: ToolCallLaunch::Pending(Box::new(PendingToolDispatchOutcome {
                        tool_name: call.tool_name,
                        args: call.args,
                        key: *key,
                        pending,
                        attempts,
                        captures,
                        triggers,
                    })),
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
                let Some(retry_after) = retry_after else {
                    return CoordinatedToolInvocation {
                        launch: match settle_terminal_attempt(
                            context,
                            TerminalAttemptSettlement {
                                minting_emission: &invocation,
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: recorded_call_id.as_deref(),
                                group_child,
                                record,
                                intents,
                                attempts,
                                captures,
                                triggers,
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
                                child_trace_hook: child_trace_hook.as_ref(),
                                recorded_call_id: recorded_call_id.as_deref(),
                                group_child,
                                record,
                                intents,
                                attempts,
                                captures,
                                triggers,
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
                        identity.retry_sleep_invocation(context, &call, attempt),
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
                                captures,
                                triggers,
                            ))),
                        };
                    }
                    abandon_to_open_buffers(context, triggers, captures);
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
            captures,
            triggers,
        ))),
    }
}

/// When no outcome exists to carry them — a controller abort refuses the
/// launch itself — an attempt's journaled facts land in the open context's
/// buffers, exactly where the pre-applicator restore put them, because the
/// commit they are evidence of already happened. The abort ends the call, so
/// nothing downstream could incorporate a settlement for it; reaching for the
/// buffers directly is tolerable only because there is no settlement left to
/// own the facts.
fn abandon_to_open_buffers(
    context: &ToolDispatchContext<'_>,
    triggers: Vec<ToolTriggerEffectOutcome>,
    captures: Vec<crate::runtime::ToolAttemptCapture>,
) {
    for trigger in triggers {
        context.trigger_outcomes.enqueue(trigger);
    }
    for capture in captures {
        context.checkpoint_messages.enqueue(capture.messages);
        if let Some(ledger) = context.direct_completions.usage_ledger() {
            ledger.extend(capture.usage);
        }
    }
}

/// Settles one terminal tool attempt: commit a group child's final record,
/// drain the declared intents in final-commit order (ADR 0099 §5), project
/// their outcomes onto the record, and report them.
///
/// Both terminal callers — a first attempt with no retry left to schedule, and
/// a retry-exhausted attempt — reach the same terminal state and run this one
/// body.
struct TerminalAttemptSettlement<'settlement> {
    minting_emission: &'settlement RuntimeEffectInvocation,
    child_trace_hook: Option<&'settlement crate::ToolChildExecutionTraceHook>,
    recorded_call_id: Option<&'settlement str>,
    group_child: Option<GroupChildCoordination>,
    record: Box<ToolCallRecord>,
    intents: crate::ToolIntents,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
    captures: Vec<crate::runtime::ToolAttemptCapture>,
    triggers: Vec<ToolTriggerEffectOutcome>,
}

/// The sealed settlement a §4 boundary commit persists as its drain input:
/// the record the drain projects onto and the declared intents, so a
/// committed-but-undrained row carries everything its recovery needs.
#[derive(serde::Serialize)]
struct GroupChildDrainInput<'settlement> {
    record: &'settlement ToolCallRecord,
    intents: &'settlement crate::ToolIntents,
    recorded_call_id: Option<&'settlement str>,
}

/// The owned decode of [`GroupChildDrainInput`]: what a re-driven attempt
/// that lost the §4 point drains instead of whatever it re-derived — the
/// committed settlement is the durable fact, not the replay.
#[derive(serde::Deserialize)]
struct SealedGroupChildDrainInput {
    record: ToolCallRecord,
    intents: crate::ToolIntents,
    recorded_call_id: Option<String>,
}

async fn settle_terminal_attempt(
    context: &ToolDispatchContext<'_>,
    settlement: TerminalAttemptSettlement<'_>,
) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
    let TerminalAttemptSettlement {
        minting_emission,
        child_trace_hook,
        recorded_call_id,
        group_child,
        mut record,
        mut intents,
        attempts,
        captures,
        triggers,
    } = settlement;
    let mut recorded_call_id = recorded_call_id.map(str::to_string);
    let controller = context.effect_controller.controller();
    let drain_admission = commit_group_child_boundary(
        context,
        group_child.as_ref(),
        &mut record,
        &mut intents,
        &mut recorded_call_id,
    )
    .await?;
    // The §5 barrier: admitted drains emit their nested semantic commands in
    // final-commit order, so a committed sibling below this child that still
    // owes its drain holds this drain back until it finishes. The host waits
    // on its own wake for that drain; the dispatch clock plays no part.
    if let Some((group_key, commit_seq)) = &drain_admission {
        controller
            .await_group_child_drain_admission(group_key, *commit_seq)
            .await?;
    }
    let mut intent_context = context.clone();
    intent_context.parent_invocation = Some(minting_emission.clone().into_runtime_invocation());
    intent_context.observation_call_key = None;
    let intent_outcomes = super::execute_final_tool_intents(
        &intent_context,
        recorded_call_id.as_deref(),
        &intents,
        child_trace_hook,
    )
    .await?;
    project_recorded_intent_outcomes(&mut record.output, &intent_outcomes);
    Ok(ToolDispatchOutcome {
        record: *record,
        attempts,
        intents,
        intent_outcomes,
        captures,
        triggers,
    })
}

/// The §4 boundary: a group child's final record commits the moment the child
/// reaches its terminal, *before* any declared intent runs and before its
/// result is presented.
///
/// Every terminal of a group child crosses this one boundary — an attempt that
/// finished inline (`settle_terminal_attempt`) and a parked attempt whose
/// deferred completion resolved (the invocation driver's resume). The durable
/// final-commit order it allocates is the order sibling drains are admitted in
/// and the order ranks are seated in (§5), so a child that deferred its
/// boundary to a later step would take its place in the settlement order by
/// when that step ran, not by when it settled.
///
/// Only a group child carries the address of its own replay row into
/// settlement; every other caller passes `None` and pays no boundary work at
/// all. `AlreadyCommitted` replaces the re-derived settlement with the sealed
/// drain input the winner committed — the committed settlement is the durable
/// fact, not the replay (W6/W7). A `CancelDecided` answer is the group's
/// arbitration losing this child's final: the typed refusal is the whole
/// record, and nothing mints beneath it.
pub(crate) async fn commit_group_child_boundary(
    context: &ToolDispatchContext<'_>,
    group_child: Option<&GroupChildCoordination>,
    record: &mut ToolCallRecord,
    intents: &mut crate::ToolIntents,
    recorded_call_id: &mut Option<String>,
) -> Result<Option<(String, u64)>, crate::RuntimeEffectControllerError> {
    let Some(address) = group_child.map(|child| &child.child) else {
        return Ok(None);
    };
    let scope_id = address
        .execution_scope
        .journal_identity()
        .map_err(crate::RuntimeEffectControllerError::from)?
        .key()
        .to_string();
    let drain_input = serde_json::to_string(&GroupChildDrainInput {
        record,
        intents,
        recorded_call_id: recorded_call_id.as_deref(),
    })
    .map_err(|error| {
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
            format!(
                "sealed drain input for {} does not encode: {error}",
                address.replay_key
            ),
        )
    })?;
    match context
        .effect_controller
        .controller()
        .commit_group_child_final(crate::runtime::effect::GroupChildFinalCommit {
            scope_id,
            replay_key: address.replay_key.clone(),
            drain_input,
        })
        .await?
    {
        crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped => Ok(None),
        crate::runtime::effect::EffectGroupChildCommitOutcome::Committed {
            group_key,
            commit_seq,
        } => Ok(Some((group_key, commit_seq))),
        crate::runtime::effect::EffectGroupChildCommitOutcome::AlreadyCommitted {
            group_key,
            commit_seq,
            drain_input: sealed,
        } => {
            if let Some(sealed) = sealed {
                let sealed: SealedGroupChildDrainInput =
                    serde_json::from_str(&sealed).map_err(|error| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            format!(
                                "committed drain input for {} does not decode: {error}",
                                address.replay_key
                            ),
                        )
                    })?;
                *record = sealed.record;
                *intents = sealed.intents;
                *recorded_call_id = sealed.recorded_call_id;
            }
            Ok(Some((group_key, commit_seq)))
        }
        crate::runtime::effect::EffectGroupChildCommitOutcome::CancelDecided {
            group_key, ..
        } => Err(crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
            format!(
                "the final record of `{}` reached durable effect group {group_key} \
                 after its cancel disposition committed; the refusal is the whole \
                 record and no declared intent may mint beneath it",
                address.replay_key
            ),
        )),
    }
}

/// Whether `outcome` is the attempt's declared process start at
/// `intent_index`, realized or refused.
fn declares_start_at(outcome: &crate::ToolIntentExecutionOutcome, intent_index: u32) -> bool {
    match outcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity,
            kind: crate::ToolIntentKind::StartProcess,
            ..
        } => identity.intent_index == intent_index,
        crate::ToolIntentExecutionOutcome::Refused {
            intent_index: refused,
            kind: crate::ToolIntentKind::StartProcess,
            ..
        } => *refused == intent_index,
        _ => false,
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

    let crate::ToolCallOutcome::Success(value) = &mut output.outcome else {
        return;
    };
    // A start's slot is resolved from the realized start of the same intent
    // index: the process handle its registration minted replaces the slot
    // before any model or cell sees the output (ADR 0107). A slot whose start
    // did not realize is never exposed as a handle. Only an attempt that
    // declared a start at that index answers a slot: the same spelling in any
    // other tool's output is that tool's own data and is left untouched.
    if let Some(intent_index) = lash_sansio::handle::process_start_slot(&value.to_json_value())
        && outcomes
            .iter()
            .any(|outcome| declares_start_at(outcome, intent_index))
    {
        let realized = outcomes.iter().find_map(|outcome| match outcome {
            crate::ToolIntentExecutionOutcome::Executed {
                identity,
                kind: crate::ToolIntentKind::StartProcess,
                result,
            } if identity.intent_index == intent_index => Some(result),
            _ => None,
        });
        let Some(handle) = realized.and_then(realized_start_handle) else {
            *output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Unavailable,
                "process_start_unrealized",
                "the declared process start did not register a process",
            ));
            return;
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
        return;
    }
    let answers = projected_intent_answers(outcomes);
    if answers.is_empty() {
        return;
    }
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

/// The handle record a realized start answers: the one handle kind, its id,
/// and the process id beside it. Nothing else of the realized view is copied —
/// `status` and the rest are facts a holder reads through the process tools.
fn realized_start_handle(result: &serde_json::Value) -> Option<serde_json::Value> {
    let mut handle = serde_json::Map::new();
    for name in [lash_sansio::handle::HANDLE_FIELD, "id", "process_id"] {
        handle.insert(name.to_string(), result.get(name)?.clone());
    }
    Some(serde_json::Value::Object(handle))
}

/// The realized answer a signal contributes back to its declaring attempt's
/// optimistic output: the `sequence` its append landed at, which no attempt
/// can predict.
///
/// `process_id` is what makes this a merge into the right output: a realized
/// answer belongs to the output that already named the same process, and any
/// other output keeps what it answered (FIG-3119). A start's answer is not
/// merged at all: its slot is replaced whole, by intent index.
struct ProjectedIntentAnswer {
    process_id: String,
    fields: Vec<(String, serde_json::Value)>,
}

fn matching_answer<'a>(
    current: &serde_json::Value,
    answers: &'a [ProjectedIntentAnswer],
) -> Option<&'a ProjectedIntentAnswer> {
    let object = current.as_object()?;
    let named = object
        .get("process_id")
        .and_then(serde_json::Value::as_str)?;
    answers.iter().find(|answer| answer.process_id == named)
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
        if *kind == crate::ToolIntentKind::SignalProcess
            && let Some(sequence) = result.get("sequence").and_then(serde_json::Value::as_u64)
        {
            answers.push(ProjectedIntentAnswer {
                process_id,
                fields: vec![("sequence".to_string(), serde_json::json!(sequence))],
            });
        }
    }
    answers
}

#[allow(
    clippy::too_many_arguments,
    reason = "a failure outcome is assembled from every channel the attempts accumulated"
)]
fn runtime_failure_outcome(
    call: &PreparedToolCall,
    code: impl Into<String>,
    message: impl Into<String>,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
    captures: Vec<crate::runtime::ToolAttemptCapture>,
    triggers: Vec<ToolTriggerEffectOutcome>,
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
        },
        attempts,
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures,
        triggers,
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
        executed_at(kind, result, 0)
    }

    fn executed_at(
        kind: crate::ToolIntentKind,
        result: serde_json::Value,
        intent_index: u32,
    ) -> crate::ToolIntentExecutionOutcome {
        crate::ToolIntentExecutionOutcome::Executed {
            identity: crate::ToolIntentIdentity {
                session_id: SessionId::from("session"),
                execution_scope_id: "turn".to_string(),
                tool_call_id: "call".to_string(),
                intent_index,
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
        let mut result = realized_handle(label);
        result["kind"] = serde_json::json!("external");
        result["status"] = serde_json::json!("running");
        executed_at(crate::ToolIntentKind::StartProcess, result, intent_index)
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
    fn a_realized_start_replaces_the_slot_its_attempt_answered() {
        let mut output = crate::ToolCallOutput::success(start_slot(2));

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 2)]);

        assert_eq!(
            output.value_for_projection(),
            realized_handle("p-child"),
            "the realized handle replaces the slot, and nothing else is copied"
        );
    }

    /// ADR 0107: a slot is never exposed as a handle. A start that did not
    /// realize answers a typed failure, not an id nobody minted.
    #[test]
    fn a_slot_whose_start_did_not_realize_is_a_typed_failure() {
        let mut output = crate::ToolCallOutput::success(start_slot(1));

        project_recorded_intent_outcomes(
            &mut output,
            &[
                start_outcome("p-other", 0),
                crate::ToolIntentExecutionOutcome::Refused {
                    identity: None,
                    intent_index: 1,
                    kind: crate::ToolIntentKind::StartProcess,
                    refusal: crate::ToolIntentRefusalReason::MissingToolCallId,
                },
            ],
        );

        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("an unrealized slot must not survive projection");
        };
        assert_eq!(failure.code, "process_start_unrealized");
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
    fn a_realized_start_leaves_an_output_that_is_not_its_handle_alone() {
        // A tool that answers its own data and declares a start alongside it.
        // Merging the realized handle into that answer put `__handle__`, `id`
        // and `process_id` into the model-facing text of the tool result
        // (FIG-3119).
        let mut output = crate::ToolCallOutput::success(serde_json::json!({ "ok": true }));

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 0)]);

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

        project_recorded_intent_outcomes(&mut output, &[start_outcome("p-child", 0)]);

        assert_eq!(
            output.value_for_projection(),
            serde_json::json!({ "process_id": "p-child", "ok": true })
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
