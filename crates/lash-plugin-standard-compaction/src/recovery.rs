//! Plugin-owned recovery of an exhausted standard-protocol context (FIG-2950):
//! the third context policy of `lash-plugin-standard-compaction`.

use super::*;

// ---- FIG-2950: plugin-owned recovery of an exhausted context ----
// A persisted turn that stops with the FIG-1272 `context_overflow` outcome
// leaves the task unfinished and unactionable by any instruction inside that
// same exhausted context. This third policy recovers it out of band:
//
// 1. The `after_turn` hook sees the persisted outcome and appends one durable
//    plugin-origin marker that rides the overflow turn's own commit.
// 2. The next turn's context-pressure hook (on restore included) re-derives
//    the pending recovery from that durable marker plus the terminal records;
//    hooks are best-effort and are never trusted across drives.
// 3. Recovery summarizes the whole committed history through one direct LLM
//    completion on the session's own journal lane, with the oversized parts
//    elided first so the summarizer request itself fits the window.
// 4. The hook returns its decision and core writes it (FIG-4110): a failed
//    attempt records its terminal record, and a completed one records the
//    completed record in the frame it leaves and opens a fresh compaction
//    frame seeded with the summary, which the continuing turn runs in
//    (FIG-4029).
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
            plugin_id: STANDARD_COMPACTION_PLUGIN_ID.to_string(),
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
            if plugin_id == STANDARD_COMPACTION_PLUGIN_ID
    ) {
        return None;
    }
    message.parts.iter().find_map(|part| {
        let text = part.text_content()?;
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
            let mut text = part.content().into_owned();
            if approx_token_count(&text) < OVERFLOW_RECOVERY_ELIDE_PART_THRESHOLD_TOKENS {
                continue;
            }
            if let Some((index, _)) = text
                .char_indices()
                .nth(OVERFLOW_RECOVERY_ELIDED_RETAINED_CHARS)
            {
                text.truncate(index);
            }
            text.push_str(OVERFLOW_ELIDED_PART_PLACEHOLDER);
            // A tool result's blocks collapse to the elided text; the
            // summarization request carries no body of the result anyway.
            match part.tool_result_content_mut() {
                Some(blocks) => *blocks = vec![ModelToolReturnPart::text(text)],
                None => {
                    if let Some(content) = part.content_mut() {
                        *content = text;
                    }
                }
            }
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

pub(crate) fn emit_recovery_trace(
    traces: &lash_core::plugin::PluginTraceEmitter,
    trace_context: lash_core::TraceContext,
    event_name: &str,
    trigger: Option<&RecoveryTraceTrigger>,
    outcome: Option<&str>,
) {
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
    traces.emit(
        trace_context,
        lash_core::TraceEvent::Custom {
            name: event_name.to_string(),
            payload,
        },
    );
}

pub(crate) fn recovery_pending_marker() -> lash_core::PluginMessage {
    recovery_record_message(OverflowRecoveryRecord::Pending)
}

fn recovery_record_node(record: OverflowRecoveryRecord) -> lash_core::SessionAppendNode {
    lash_core::SessionAppendNode::message(recovery_record_message(record))
}

/// A settled failed attempt: its `Failed` record and, at the cap, the
/// `Exhausted` record that closes the recovery with no frame.
fn recovery_failure_decision(
    traces: &lash_core::plugin::PluginTraceEmitter,
    trace_context: lash_core::TraceContext,
    attempt_no: usize,
    reason: &str,
) -> ContextPressureDecision {
    let exhausted = attempt_no >= OVERFLOW_RECOVERY_MAX_ATTEMPTS;
    let outcome = if exhausted {
        "exhausted:recoverable_failure".to_string()
    } else {
        format!("failed:{reason}")
    };
    emit_recovery_trace(
        traces,
        trace_context,
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some(outcome.as_str()),
    );
    let mut nodes = vec![recovery_record_node(OverflowRecoveryRecord::Failed {
        attempt: attempt_no as u32,
    })];
    if exhausted {
        nodes.push(recovery_record_node(OverflowRecoveryRecord::Exhausted));
    }
    ContextPressureDecision::Record { nodes }
}

/// One bounded recovery attempt, decided from committed history.
///
/// FIG-3374: the summarizer is one direct LLM completion over the committed
/// history on the session's own journal lane. Success decides `OpenFrame`:
/// the `Completed` record joins the frame the recovery leaves, and the
/// summary seeds a compaction frame the recovering turn runs in (FIG-4029).
/// Failure decides `Record`: the attempt's `Failed` record and, at the cap,
/// the `Exhausted` record, with no frame. Core writes either (FIG-4110).
pub(crate) async fn overflow_recovery_decision(
    ctx: &ContextPressureContext<'_>,
    state: OverflowRecoveryState,
) -> Result<ContextPressureDecision, ContextError> {
    let trace_context = lash_core::TraceContext::default().for_session(ctx.session_id.clone());
    if state.exhausted() {
        emit_recovery_trace(
            &ctx.traces,
            trace_context,
            TRACE_OVERFLOW_RECOVERY_OUTCOME,
            None,
            Some("exhausted:recoverable_failure"),
        );
        return Ok(ContextPressureDecision::Continue);
    }
    let attempt_no = state.attempts() + 1;
    let history_messages = ctx.state.messages();
    let history_snapshot = ctx.state.to_snapshot();

    let prefix_len = leading_system_prefix_len(history_messages);
    let summary_prefix: Vec<Message> =
        history_messages[prefix_len.min(history_messages.len())..].to_vec();

    let trigger = RecoveryTraceTrigger {
        history_messages: history_messages.len(),
        attempts: state.attempts(),
        oversized_elided_parts: 0,
    };
    emit_recovery_trace(
        &ctx.traces,
        trace_context.clone(),
        TRACE_OVERFLOW_RECOVERY_TRIGGER,
        Some(&trigger),
        None,
    );

    if summary_prefix.is_empty() {
        return Ok(recovery_failure_decision(
            &ctx.traces,
            trace_context,
            attempt_no,
            "insufficient_reduction",
        ));
    }

    // Elide first: the summarizer request itself must fit its window, and it
    // must never be the rejected oversized request with an instruction
    // appended.
    let mut summarizer_prefix = summary_prefix;
    let elided_parts = elide_oversized_parts(&mut summarizer_prefix);

    let summarizer_budget_tokens = compaction_threshold(ctx.max_context_tokens.unwrap_or(0));
    let projected_tokens: usize = summarizer_prefix
        .iter()
        .map(|message| {
            message
                .parts
                .iter()
                .map(|part| {
                    approx_token_count(&part.content()) + 1_200 * part.attachment_sources().count()
                })
                .sum::<usize>()
        })
        .sum::<usize>()
        + approx_token_count(OVERFLOW_RECOVERY_INSTRUCTIONS)
        + 512;
    let (_, prompt_text) = prepare_compaction_request(
        &history_snapshot,
        summarizer_prefix.clone(),
        Some(&recovery_instructions(elided_parts)),
    )?;
    let request_tokens = projected_tokens + approx_token_count(&prompt_text);
    if request_tokens > summarizer_budget_tokens {
        return Ok(recovery_failure_decision(
            &ctx.traces,
            trace_context,
            attempt_no,
            "summarizer_request_exceeds_window",
        ));
    }

    // The summarizer is one direct completion on the same seam the ordinary
    // compaction policy uses (FIG-3374). The system prompt resolves here, at
    // the point of use, so its plugin hooks never fire on turns that recover
    // without summarizing.
    let resolved_system_prompt = match &ctx.system_prompt {
        Some(provider) => provider().await.map_err(ContextError::from)?,
        None => None,
    };
    let summary = match summarize_compaction_prefix(
        &ctx.session_id,
        &history_snapshot,
        summarizer_prefix,
        Some(&recovery_instructions(elided_parts)),
        &ctx.direct_completions,
        &ctx.scoped_effect_controller,
        resolved_system_prompt,
    )
    .await
    {
        Ok(Some(summary)) => summary,
        Ok(None) => {
            return Ok(recovery_failure_decision(
                &ctx.traces,
                trace_context,
                attempt_no,
                "insufficient_reduction",
            ));
        }
        Err(error) => {
            return Ok(recovery_failure_decision(
                &ctx.traces,
                trace_context,
                attempt_no,
                &format!("summarizer_failed: {error}"),
            ));
        }
    };

    emit_recovery_trace(
        &ctx.traces,
        trace_context,
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some("completed"),
    );
    // The completed record closes the recovery in the frame it leaves; the
    // summary seeds the recovery frame, a compaction frame core opens before
    // this turn runs and commits with it.
    Ok(ContextPressureDecision::OpenFrame {
        records: vec![recovery_record_node(OverflowRecoveryRecord::Completed)],
        task: OVERFLOW_RECOVERY_TASK.to_string(),
        seed: vec![compaction_summary_seed(&summary)],
    })
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
