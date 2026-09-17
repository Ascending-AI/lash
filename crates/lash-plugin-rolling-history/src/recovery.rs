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

/// The prose title selects which plugin messages are recovery records; the
/// record itself is always read back from the serde payload that follows it.
/// Returns the payload of the first record part, or `None` when the message
/// is not a recovery record.
pub(crate) fn recovery_record_payload(message: &Message) -> Option<&str> {
    if !matches!(
        message.origin,
        Some(MessageOrigin::Plugin { ref plugin_id, .. })
            if plugin_id == ROLLING_HISTORY_PLUGIN_ID
    ) {
        return None;
    }
    message.parts.iter().find_map(|part| {
        let text = part.content.as_str();
        [
            OVERFLOW_RECOVERY_MARKER,
            OVERFLOW_RECOVERY_COMPLETED,
            OVERFLOW_RECOVERY_FAILED,
            OVERFLOW_RECOVERY_EXHAUSTED,
        ]
        .iter()
        .find_map(|marker| text.strip_prefix(marker).map(str::trim))
    })
}

/// Read a recovery record back from its serialized form. A message carrying a
/// recovery title whose payload does not deserialize is an error, never a
/// record guessed from the title.
pub(crate) fn recovery_record_kind(
    message: &Message,
) -> Result<Option<OverflowRecoveryRecord>, serde_json::Error> {
    recovery_record_payload(message)
        .map(serde_json::from_str)
        .transpose()
}

/// Recovery state derived purely from committed history. Nothing else carries
/// recovery across drives, so a crash, a restore, or a redelivered marker
/// cannot duplicate it: one still-open pending marker is at most one recovery,
/// every attempt is settled by a terminal record, and the record order says
/// how many attempts an open pending state already spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverflowRecoveryState {
    /// No still-open pending marker: the last recovery settled or never ran.
    Idle,
    /// A pending marker is still open; `attempts` counts the failures it
    /// already spent.
    Pending { attempts: usize },
}

impl OverflowRecoveryState {
    pub(crate) fn derive(records: impl IntoIterator<Item = OverflowRecoveryRecord>) -> Self {
        let mut state = Self::Idle;
        for record in records {
            match record {
                OverflowRecoveryRecord::Pending => state = Self::Pending { attempts: 0 },
                OverflowRecoveryRecord::Completed | OverflowRecoveryRecord::Exhausted => {
                    state = Self::Idle;
                }
                OverflowRecoveryRecord::Failed { attempt } => {
                    if let Self::Pending { attempts } = &mut state {
                        *attempts = attempt as usize;
                    }
                }
            }
        }
        state
    }

    pub(crate) fn pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    /// Attempts the open pending marker already spent; zero when idle.
    pub(crate) fn attempts(&self) -> usize {
        match self {
            Self::Pending { attempts } => *attempts,
            Self::Idle => 0,
        }
    }

    pub(crate) fn exhausted(&self) -> bool {
        matches!(self, Self::Pending { attempts } if *attempts >= OVERFLOW_RECOVERY_MAX_ATTEMPTS)
    }
}

pub(crate) fn history_recovery_records(
    messages: &[Message],
) -> Result<Vec<OverflowRecoveryRecord>, serde_json::Error> {
    messages
        .iter()
        .map(recovery_record_kind)
        .filter_map(Result::transpose)
        .collect()
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

/// One bounded recovery attempt. Success returns the fresh prompt window; failure
/// records the attempt (and, at the cap, the exhausted record) and returns `None`, leaving
/// the turn on the ordinary rolling projection.
///
/// FIG-3107: the summarizer now runs on the real compaction seam — an ordinary
/// `new_runtime_internal_compaction` managed child turn over the committed
/// history — and the summary lands in a durable recovery frame through
/// [`SessionGraphService::switch_agent_frame`], the same durable semantics as
/// the in-turn frame-switch control. Recovery no longer projects a window
/// into the exhausted frame, and no `DirectCompletionClient` is spent: the
/// residual window projection below covers only this running turn's prompt
/// view, while the durable session continues in the switched frame.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_overflow_recovery(
    session_id: &SessionId,
    history_messages: &[Message],
    history_snapshot: &SessionSnapshot,
    session_lifecycle: Arc<dyn lash_core::plugin::SessionLifecycleService>,
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    scoped_effect_controller: &lash_core::ScopedEffectController<'_>,
    current_frame_node_id: Option<&str>,
    trace_context: lash_core::TraceContext,
    state: OverflowRecoveryState,
    max_context_tokens: usize,
    current_request: &[Message],
) -> Result<Option<Vec<Message>>, ContextError> {
    let attempt_no = state.attempts() + 1;

    let prefix_len = leading_system_prefix_len(history_messages);
    let summary_prefix: Vec<Message> =
        history_messages[prefix_len.min(history_messages.len())..].to_vec();

    let trigger = RecoveryTraceTrigger {
        history_messages: history_messages.len(),
        attempts: state.attempts(),
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
            scoped_effect_controller.execution_scope(),
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
            scoped_effect_controller.execution_scope(),
            trace_context,
            attempt_no,
            "summarizer_request_exceeds_window",
        )
        .await;
    }

    // The summarizer runs as the runtime-internal compaction managed child
    // turn: the same seam the ordinary compaction policy uses, hydrated from
    // the parent's durable state (FIG-3107).
    let summary = {
        let summarized = summarize_compaction_prefix(
            session_id,
            history_snapshot,
            summarizer_prefix.clone(),
            Some(&recovery_instructions(elided_parts)),
            session_lifecycle,
            scoped_effect_controller.clone(),
        )
        .await;
        match summarized {
            Ok(Some(summary)) => summary,
            Ok(None) => {
                return record_and_project_failure(
                    session_id,
                    history_snapshot,
                    &request_snapshot,
                    &prompt_text,
                    session_graph,
                    scoped_effect_controller.execution_scope(),
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
                    scoped_effect_controller.execution_scope(),
                    trace_context,
                    attempt_no,
                    Box::leak(format!("summarizer_failed: {error}").into_boxed_str()),
                )
                .await;
            }
        }
    };

    emit_recovery_trace(
        session_graph,
        trace_context.clone(),
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some("completed"),
    )
    .await?;
    // The terminal record is durable in the old frame; the summary itself is
    // the switched frame's seed, journaled with the continuing turn's commit.
    append_recovery_record(
        session_id,
        history_snapshot,
        &request_snapshot,
        &prompt_text,
        session_graph,
        scoped_effect_controller.execution_scope(),
        "completed".to_string(),
        vec![lash_core::SessionAppendNode::message(
            recovery_record_message(OverflowRecoveryRecord::Completed),
        )],
    )
    .await?;

    // Switch to the recovery frame: a compaction-derived durable frame whose
    // seed is the recovered summary, materialized by this turn's own commit.
    switch_recovery_frame(
        session_id,
        history_snapshot,
        &request_snapshot,
        &prompt_text,
        session_graph,
        scoped_effect_controller,
        current_frame_node_id,
        &summary,
    )
    .await?;

    let window = recovered_prompt_window(&summary, history_messages, current_request);
    Ok(Some(window))
}

/// Opens the recovery frame through the plugin-visible frame-switch seam
/// (FIG-3107). The operation id materializes from the same compaction child
/// identity as the append above, so re-deriving the recovery answers the same
/// switch commit idempotently.
#[allow(clippy::too_many_arguments)]
async fn switch_recovery_frame(
    session_id: &SessionId,
    history_snapshot: &SessionSnapshot,
    request_snapshot: &SessionSnapshot,
    prompt_text: &str,
    session_graph: &dyn lash_core::plugin::SessionGraphService,
    scoped_effect_controller: &lash_core::ScopedEffectController<'_>,
    current_frame_node_id: Option<&str>,
    summary: &str,
) -> Result<(), ContextError> {
    let (child_session, _) = compaction_child_ids(
        session_id,
        history_snapshot,
        request_snapshot,
        prompt_text,
        scoped_effect_controller.execution_scope(),
    )?;
    let discriminator = child_session
        .as_str()
        .split_once("-compaction:")
        .map_or_else(|| child_session.to_string(), |(_, tail)| tail.to_string());
    let frame_key = lash_core::FrameKey::from_compaction_material(
        session_id,
        &format!("rolling-history-overflow-recovery:{discriminator}"),
        current_frame_node_id.unwrap_or_default(),
    );
    session_graph
        .switch_agent_frame(
            session_id,
            lash_core::SwitchAgentFrameRequest::new(
                format!("rolling-history-overflow-recovery/switch/{discriminator}"),
                frame_key,
                lash_core::AgentFrameReason::compaction(),
            )
            .with_task("context-overflow recovery")
            .with_initial_nodes(vec![compaction_summary_seed(summary)]),
        )
        .await
        .map_err(ContextError::from)?;
    Ok(())
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
