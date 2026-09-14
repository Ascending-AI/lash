//! Plugin-owned recovery of an exhausted RLM context (FIG-2950): the third
//! context policy of `lash-plugin-rolling-history`.

use super::*;

// ---- FIG-2950: plugin-owned recovery of an exhausted RLM context ----
// A persisted turn that stops with the FIG-1272 `context_overflow` outcome
// leaves the task unfinished and unactionable by any instruction inside that
// same exhausted context. This third policy recovers it out of band:
//
// 1. The `after_turn` hook sees the persisted outcome and appends one durable
//    plugin-origin marker that rides the overflow turn's own commit.
// 2. The next turn (on restore included) re-derives the pending recovery from
//    that durable marker plus the terminal records; hooks are best-effort and
//    are never trusted across drives.
// 3. Recovery summarizes the whole committed history through the existing
//    `new_runtime_internal_compaction` managed child turn, with the oversized
//    parts elided first so the summarizer request itself fits the window.
// 4. The summary and one terminal record are appended through the pinning
//    graph seam under a stable operation id, and the continuing turn runs in
//    a fresh recovered window (system prefix + summary + current request).
//
// Attempt count, the cap, and the recoverable-failure record are all
// plugin-owned durable facts; the terminal exhausted record is what keeps a
// failed recovery from turning into a compact/retry loop.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OverflowRecoveryRecord {
    Pending,
    Completed,
    Failed { attempt: u32 },
    Exhausted,
}

pub(crate) fn recovery_record_message(record: OverflowRecoveryRecord) -> lash_core::PluginMessage {
    let title = match record {
        OverflowRecoveryRecord::Pending => OVERFLOW_RECOVERY_MARKER,
        OverflowRecoveryRecord::Completed => OVERFLOW_RECOVERY_COMPLETED,
        OverflowRecoveryRecord::Failed { .. } => OVERFLOW_RECOVERY_FAILED,
        OverflowRecoveryRecord::Exhausted => OVERFLOW_RECOVERY_EXHAUSTED,
    };
    let payload = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string());
    lash_core::PluginMessage::text(MessageRole::System, format!("{title}\n{payload}")).with_origin(
        MessageOrigin::Plugin {
            plugin_id: ROLLING_HISTORY_PLUGIN_ID.to_string(),
            transient: false,
        },
    )
}

pub(crate) fn recovery_record_kind(message: &Message) -> Option<OverflowRecoveryRecord> {
    if !matches!(
        message.origin,
        Some(MessageOrigin::Plugin { ref plugin_id, .. })
            if plugin_id == ROLLING_HISTORY_PLUGIN_ID
    ) {
        return None;
    }
    for part in message.parts.iter() {
        let text = part.content.as_str();
        let (title, rest) = if let Some(rest) = text.strip_prefix(OVERFLOW_RECOVERY_MARKER) {
            ("pending", rest)
        } else if let Some(rest) = text.strip_prefix(OVERFLOW_RECOVERY_COMPLETED) {
            ("completed", rest)
        } else if let Some(rest) = text.strip_prefix(OVERFLOW_RECOVERY_FAILED) {
            ("failed", rest)
        } else if let Some(rest) = text.strip_prefix(OVERFLOW_RECOVERY_EXHAUSTED) {
            ("exhausted", rest)
        } else {
            continue;
        };
        let payload: serde_json::Value =
            serde_json::from_str(rest.trim()).unwrap_or(serde_json::Value::Null);
        return match title {
            "pending" => Some(OverflowRecoveryRecord::Pending),
            "completed" => Some(OverflowRecoveryRecord::Completed),
            "failed" => Some(OverflowRecoveryRecord::Failed {
                attempt: payload
                    .get("attempt")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as u32,
            }),
            _ => Some(OverflowRecoveryRecord::Exhausted),
        };
    }
    None
}

/// Recovery state derived purely from committed history. Nothing else carries
/// recovery across drives, so a crash, a restore, or a redelivered marker
/// cannot duplicate it: one still-open pending marker is at most one recovery,
/// every attempt is settled by a terminal record, and the record order says
/// how many attempts an open pending state already spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverflowRecoveryState {
    pub(crate) pending: bool,
    pub(crate) attempts: usize,
}

impl OverflowRecoveryState {
    pub(crate) fn derive(records: impl IntoIterator<Item = OverflowRecoveryRecord>) -> Self {
        let mut state = Self {
            pending: false,
            attempts: 0,
        };
        for record in records {
            match record {
                OverflowRecoveryRecord::Pending => {
                    state.pending = true;
                    state.attempts = 0;
                }
                OverflowRecoveryRecord::Completed | OverflowRecoveryRecord::Exhausted => {
                    state.pending = false;
                    state.attempts = 0;
                }
                OverflowRecoveryRecord::Failed { attempt } => {
                    state.attempts = attempt as usize;
                }
            }
        }
        state
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.pending && self.attempts >= OVERFLOW_RECOVERY_MAX_ATTEMPTS
    }
}

pub(crate) fn history_recovery_records(messages: &[Message]) -> Vec<OverflowRecoveryRecord> {
    messages.iter().filter_map(recovery_record_kind).collect()
}

/// Elide each oversized part's body so the out-of-band summarization request
/// itself fits the model's window. Decision-local: durable history keeps the
/// original body, and the summary prompt names what was dropped.
pub(crate) fn elide_oversized_parts(messages: &mut [Message]) -> usize {
    let mut elided = 0usize;
    for message in messages {
        for part in std::sync::Arc::make_mut(&mut message.parts).iter_mut() {
            if approx_token_count(&part.content) < OVERFLOW_RECOVERY_ELIDE_PART_THRESHOLD_TOKENS {
                continue;
            }
            let head: &str = if let Some((index, _)) = part
                .content
                .char_indices()
                .nth(OVERFLOW_RECOVERY_ELIDED_RETAINED_CHARS)
            {
                &part.content[..index]
            } else {
                &part.content
            };
            part.content = format!("{head}{OVERFLOW_ELIDED_PART_PLACEHOLDER}");
            elided += 1;
        }
    }
    elided
}

pub(crate) fn recovery_instructions(elided_parts: usize) -> String {
    if elided_parts == 0 {
        OVERFLOW_RECOVERY_INSTRUCTIONS.to_string()
    } else {
        format!(
            "{OVERFLOW_RECOVERY_INSTRUCTIONS}\n\n{n} part(s) in the history below were elided for size; their bodies are intentionally absent.",
            n = elided_parts
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecoveryTraceTrigger {
    history_messages: usize,
    attempts: usize,
    oversized_elided_parts: usize,
}

pub(crate) async fn emit_recovery_trace(
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    trace_context: lash_core::TraceContext,
    event_name: &str,
    trigger: Option<&RecoveryTraceTrigger>,
    outcome: Option<&str>,
) -> Result<(), ContextError> {
    let mut payload = serde_json::json!({});
    if let Some(trigger) = trigger {
        payload["trigger"] = serde_json::json!("persisted_context_overflow");
        payload["history_messages"] = serde_json::json!(trigger.history_messages);
        payload["attempts"] = serde_json::json!(trigger.attempts);
        payload["oversized_elided_parts"] = serde_json::json!(trigger.oversized_elided_parts);
    }
    if let Some(outcome) = outcome {
        payload["outcome"] = serde_json::json!(outcome);
    }
    session_graph
        .emit_trace_event(
            trace_context,
            lash_core::TraceEvent::Custom {
                name: event_name.to_string(),
                payload,
            },
        )
        .await
        .map_err(ContextError::from)?;
    Ok(())
}

/// Plugin-origin record for the summarization attempt that just settled.
/// Stable within its attempt: the operation id materializes from the same
/// compaction child identity the summarizer turn used, so a retry that
/// re-derives the same attempt id's first, durable receipt idempotently.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_recovery_record(
    session_id: &SessionId,
    history_snapshot: &SessionSnapshot,
    request_snapshot: &SessionSnapshot,
    prompt_text: &str,
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    execution_scope: &lash_core::ExecutionScope,
    suffix: String,
    nodes: Vec<lash_core::SessionAppendNode>,
) -> Result<(), ContextError> {
    let (child_session, _) = compaction_child_ids(
        session_id,
        history_snapshot,
        request_snapshot,
        prompt_text,
        execution_scope,
    )?;
    let discriminator = child_session
        .as_str()
        .split_once("-compaction:")
        .map_or_else(|| child_session.to_string(), |(_, tail)| tail.to_string());
    let request = lash_core::AppendSessionNodesRequest {
        operation_id: format!("rolling-history-overflow-recovery/{suffix}/{discriminator}"),
        nodes,
        requires_ancestor_node_id: None,
    };
    session_graph
        .append_session_nodes(session_id, request)
        .await
        .map_err(ContextError::from)?;
    Ok(())
}

/// The recovered prompt window: system prefix, the recovered summary, and the
/// current turn's own request. Everything summarized is gone from the window
/// without being rewritten; the session keeps its full history durable and
/// inspectable.
pub(crate) fn recovered_prompt_window(
    summary: &str,
    history_messages: &[Message],
    current_request: &[Message],
) -> Vec<Message> {
    let prefix_len = leading_system_prefix_len(history_messages);
    let message_id = "m_rolling_history_overflow_recovery_summary";
    let summary_message = Message {
        id: message_id.to_string(),
        role: MessageRole::Assistant,
        parts: vec![Part::text(
            format!("{message_id}.p0"),
            format!("{COMPACTION_SUMMARY_TITLE}\n{summary}"),
            None,
        )]
        .into(),
        origin: Some(MessageOrigin::Plugin {
            plugin_id: ROLLING_HISTORY_PLUGIN_ID.to_string(),
            transient: false,
        }),
    };
    let mut projected: Vec<Message> = history_messages[..prefix_len].to_vec();
    projected.push(summary_message);
    projected.extend_from_slice(current_request);
    projected
}

pub(crate) fn recovery_pending_marker() -> lash_core::PluginMessage {
    recovery_record_message(OverflowRecoveryRecord::Pending)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_and_project_failure(
    session_id: &SessionId,
    history_snapshot: &SessionSnapshot,
    request_snapshot: &SessionSnapshot,
    prompt_text: &str,
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    execution_scope: &lash_core::ExecutionScope,
    trace_context: lash_core::TraceContext,
    attempt_no: usize,
    reason: &str,
) -> Result<Option<Vec<Message>>, ContextError> {
    let outcome: String = if attempt_no >= OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        "exhausted:recoverable_failure".to_string()
    } else {
        format!("failed:{reason}")
    };
    emit_recovery_trace(
        session_graph,
        trace_context.clone(),
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some(outcome.as_str()),
    )
    .await?;
    let mut nodes = vec![lash_core::SessionAppendNode::message(
        recovery_record_message(OverflowRecoveryRecord::Failed {
            attempt: attempt_no as u32,
        }),
    )];
    if attempt_no >= OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        nodes.push(lash_core::SessionAppendNode::message(
            recovery_record_message(OverflowRecoveryRecord::Exhausted),
        ));
    }
    append_recovery_record(
        session_id,
        history_snapshot,
        request_snapshot,
        prompt_text,
        session_graph,
        execution_scope,
        format!("failed/attempt-{attempt_no}"),
        nodes,
    )
    .await?;
    Ok(None)
}

/// One bounded recovery attempt. Success returns the fresh prompt window;
/// failure records the attempt (and, at the cap, the exhausted record) and
/// returns `None`, leaving the turn on the ordinary rolling projection.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_overflow_recovery(
    session_id: &SessionId,
    history_messages: &[Message],
    history_snapshot: &SessionSnapshot,
    direct_completions: &lash_core::facade_support::DirectCompletionClient<'_>,
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    execution_scope: &lash_core::ExecutionScope,
    trace_context: lash_core::TraceContext,
    state: OverflowRecoveryState,
    max_context_tokens: usize,
    current_request: &[Message],
) -> Result<Option<Vec<Message>>, ContextError> {
    let attempt_no = state.attempts + 1;

    let prefix_len = leading_system_prefix_len(history_messages);
    let summary_prefix: Vec<Message> =
        history_messages[prefix_len.min(history_messages.len())..].to_vec();

    let trigger = RecoveryTraceTrigger {
        history_messages: history_messages.len(),
        attempts: state.attempts,
        oversized_elided_parts: 0,
    };
    emit_recovery_trace(
        session_graph,
        trace_context.clone(),
        TRACE_OVERFLOW_RECOVERY_TRIGGER,
        Some(&trigger),
        None,
    )
    .await?;

    if summary_prefix.is_empty() {
        return record_and_project_failure(
            session_id,
            history_snapshot,
            history_snapshot,
            "",
            session_graph,
            execution_scope,
            trace_context,
            attempt_no,
            "insufficient_reduction",
        )
        .await;
    }

    // Elide first: the summarizer request itself must fit its window, and it
    // must never be the rejected oversized request with an instruction
    // appended.
    let mut summarizer_prefix = summary_prefix.clone();
    let elided_parts = elide_oversized_parts(&mut summarizer_prefix);

    let summarizer_budget_tokens = compaction_threshold(max_context_tokens);
    let projected_tokens: usize = summarizer_prefix
        .iter()
        .map(|message| {
            message
                .parts
                .iter()
                .map(|part| {
                    approx_token_count(&part.content)
                        + if part.attachment.is_some() { 1_200 } else { 0 }
                })
                .sum::<usize>()
        })
        .sum::<usize>()
        + approx_token_count(OVERFLOW_RECOVERY_INSTRUCTIONS)
        + 512;
    let (request_snapshot, prompt_text) = prepare_compaction_request(
        history_snapshot,
        summarizer_prefix.clone(),
        Some(&recovery_instructions(elided_parts)),
    )?;
    let request_tokens = projected_tokens + approx_token_count(&prompt_text);
    if request_tokens > summarizer_budget_tokens {
        return record_and_project_failure(
            session_id,
            history_snapshot,
            &request_snapshot,
            &prompt_text,
            session_graph,
            execution_scope,
            trace_context,
            attempt_no,
            "summarizer_request_exceeds_window",
        )
        .await;
    }

    // The summarizer runs as one direct, journaled LLM completion over the
    // elided history: a replay of the same operation is replay, not a new
    // summarization spend, and no child session has to hydrate anything.
    let mut user_text = String::new();
    for message in &summarizer_prefix {
        for part in message.parts.iter() {
            user_text.push_str(&part.content);
            user_text.push('\n');
        }
    }
    let model_id = history_snapshot.policy.model.id.clone();
    let direct_request = lash_core::facade_support::DirectRequest {
        instructions: None,
        model: model_id,
        model_variant: lash_core::ReasoningSelection::ProviderDefault,
        model_capability: lash_core::ModelCapability::default(),
        messages: vec![lash_core::facade_support::DirectMessage {
            role: lash_core::facade_support::DirectRole::User,
            parts: vec![lash_core::facade_support::DirectPart::Text(format!(
                "{user_text}\n\nProvide a detailed summary of the conversation above so the task can continue without the full history.\n\nAdditional focus:\n{}\n",
                recovery_instructions(elided_parts)
            ))],
        }],
        output: lash_core::facade_support::DirectOutputSpec::Text,
        generation: Default::default(),
        stream_events: None,
        session_id: Some(session_id.clone()),
        caused_by: None,
        replay: None,
    };
    let summarized = direct_completions
        .direct_completion(direct_request, "rolling_history.overflow_recovery")
        .await;

    let summary = match summarized {
        Ok(completion) if !completion.text.trim().is_empty() => completion.text,
        Ok(_) => {
            return record_and_project_failure(
                session_id,
                history_snapshot,
                &request_snapshot,
                &prompt_text,
                session_graph,
                execution_scope,
                trace_context,
                attempt_no,
                "insufficient_reduction",
            )
            .await;
        }
        Err(error) => {
            return record_and_project_failure(
                session_id,
                history_snapshot,
                &request_snapshot,
                &prompt_text,
                session_graph,
                execution_scope,
                trace_context,
                attempt_no,
                Box::leak(format!("summarizer_failed: {error}").into_boxed_str()),
            )
            .await;
        }
    };

    let window = recovered_prompt_window(&summary, history_messages, current_request);

    emit_recovery_trace(
        session_graph,
        trace_context,
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some("completed"),
    )
    .await?;
    let nodes = vec![
        compaction_summary_seed(&summary),
        lash_core::SessionAppendNode::message(recovery_record_message(
            OverflowRecoveryRecord::Completed,
        )),
    ];
    append_recovery_record(
        session_id,
        history_snapshot,
        &request_snapshot,
        &prompt_text,
        session_graph,
        execution_scope,
        "completed".to_string(),
        nodes,
    )
    .await?;

    Ok(Some(window))
}

/// Marker directive the `after_turn` hook queues for a persisted overflow.
use lash_core::facade_support::{TurnOutcome, TurnStop};
pub async fn overflow_recovery_marker(
    turn: &lash_core::plugin::TurnHookReport,
) -> Option<lash_core::plugin::EnqueueMessagesDirective> {
    if matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::ContextOverflow)
    ) {
        Some(lash_core::plugin::EnqueueMessagesDirective {
            messages: vec![recovery_pending_marker()],
        })
    } else {
        None
    }
}

/// The `after_turn` trigger. Best-effort fire, after the durable commit: no
/// error propagates into the turn; a lost marker is only a lost *attempt*,
/// never a duplicated one, and restore re-derives from the state appended
/// together with the overflow outcome.
pub(crate) async fn overflow_recovery_after_turn(
    ctx: &lash_core::plugin::TurnResultHookContext,
) -> Result<Vec<lash_core::plugin::AfterTurnPluginDirective>, lash_core::plugin::PluginError> {
    match overflow_recovery_marker(&ctx.turn).await {
        Some(messages) => Ok(vec![
            lash_core::plugin::AfterTurnPluginDirective::Ambient(
                lash_core::plugin::PluginDirective::emit_trace(
                    TRACE_OVERFLOW_RECOVERY_TRIGGER,
                    serde_json::json!({
                        "trigger": "persisted_context_overflow",
                        "marker": "queued",
                    }),
                ),
            ),
            lash_core::plugin::AfterTurnPluginDirective::EnqueueMessages(messages),
        ]),
        None => Ok(vec![]),
    }
}
