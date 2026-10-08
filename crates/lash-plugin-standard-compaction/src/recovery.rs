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
//    hooks are best-effort and are never trusted across shifts.
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

pub(crate) const OVERFLOW_RECOVERY_PLUGIN_TYPE: &str = "standard_compaction.overflow_recovery";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RecoveryFailureCause {
    NothingToSummarize,
    RequestExceedsWindow,
    EmptySummary,
    SummarizerRefused { code: lash_core::RuntimeErrorCode },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OverflowRecoveryRecord {
    Pending {},
    Completed {},
    Failed {
        attempt: u32,
        cause: RecoveryFailureCause,
    },
    Exhausted {},
}

/// Version of immutable standard-compaction recovery markers in history.
/// version_surface = "migrate"
/// format_outside_manifest = "optional standard-compaction plugin history; its plugin crate owns the decoder and guarded-surface probes independently of the facade feature set"
/// version_guard(roots(RecoveryEnvelope))
pub(crate) const OVERFLOW_RECOVERY_FORMAT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryEnvelope {
    format: u32,
    record: OverflowRecoveryRecord,
}

fn recovery_body(
    record: OverflowRecoveryRecord,
    schema_version: u32,
) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(RecoveryEnvelope {
        format: schema_version,
        record,
    })
}

pub(crate) fn decode_recovery_body(
    body: serde_json::Value,
) -> Result<OverflowRecoveryRecord, lash_core::StoredDataCorruption> {
    let corrupt = |message: String| lash_core::StoredDataCorruption {
        record_kind: OVERFLOW_RECOVERY_PLUGIN_TYPE.into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp =
        serde_json::from_value(body.clone()).map_err(|error| corrupt(error.to_string()))?;
    if !lash_core::store::upcast_chain_covers(
        lash_core::surface_format!(OVERFLOW_RECOVERY_FORMAT_VERSION),
        stamp.format,
        OVERFLOW_RECOVERY_FORMAT_VERSION,
    ) {
        return Err(corrupt(format!(
            "unsupported recovery format {}",
            stamp.format
        )));
    }
    serde_json::from_value::<RecoveryEnvelope>(body)
        .map(|envelope| envelope.record)
        .map_err(|error| corrupt(error.to_string()))
}

pub(crate) fn recovery_record_node(
    record: OverflowRecoveryRecord,
    schema_version: u32,
) -> Result<lash_core::SessionAppendNode, ContextError> {
    let body = recovery_body(record, schema_version)
        .map_err(|error| ContextError::Session(error.to_string()))?;
    Ok(lash_core::SessionAppendNode::plugin(
        OVERFLOW_RECOVERY_PLUGIN_TYPE,
        body,
    ))
}

/// Recovery state derived purely from committed history. Nothing else carries
/// recovery across shifts, so a crash, a restore, or a redelivered marker
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
                OverflowRecoveryRecord::Pending {} => state = Self::Pending { attempts: 0 },
                OverflowRecoveryRecord::Completed {} | OverflowRecoveryRecord::Exhausted {} => {
                    state = Self::Idle;
                }
                OverflowRecoveryRecord::Failed { attempt, .. } => {
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

    pub(crate) fn exhausted(&self, config: &StandardCompactionConfig) -> bool {
        matches!(self, Self::Pending { attempts } if *attempts >= config.overflow_max_attempts.get() as usize)
    }
}

pub(crate) fn history_recovery_records(
    state: &lash_core::plugin::SessionReadView,
) -> Result<Vec<OverflowRecoveryRecord>, lash_core::StoredDataCorruption> {
    use lash_core::facade_support::{SessionGraphFacadeOps as _, SessionNodeProjection as _};
    state
        .session_graph()
        .active_path_nodes()
        .into_iter()
        .filter_map(|node| {
            let (plugin_type, body) = node.plugin()?;
            (plugin_type == OVERFLOW_RECOVERY_PLUGIN_TYPE)
                .then(|| decode_recovery_body(body.clone()))
        })
        .collect()
}

/// Elide each oversized part's body so the out-of-band summarization request
/// itself fits the model's window. Decision-local: durable history keeps the
/// original body, and the summary prompt names what was dropped.
pub(crate) fn elide_oversized_parts(
    messages: &mut [Message],
    config: &StandardCompactionConfig,
) -> usize {
    let mut elided = 0usize;
    for message in messages {
        for part in std::sync::Arc::make_mut(&mut message.parts).iter_mut() {
            let mut text = part.content().into_owned();
            if config.approx_token_count(&text) < config.overflow_elide_part_threshold_tokens {
                continue;
            }
            if let Some((index, _)) = text
                .char_indices()
                .nth(config.overflow_elided_retained_chars)
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

pub(crate) fn recovery_instructions(
    elided_parts: usize,
    config: &StandardCompactionConfig,
) -> String {
    if elided_parts == 0 {
        config.overflow_instructions.clone()
    } else {
        format!(
            "{}\n\n{n} part(s) in the history below were elided for size; their bodies are intentionally absent.",
            config.overflow_instructions,
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

/// A settled failed attempt: its `Failed` record and, at the cap, the
/// `Exhausted` record that closes the recovery with no frame.
fn recovery_failure_decision(
    traces: &lash_core::plugin::PluginTraceEmitter,
    trace_context: lash_core::TraceContext,
    attempt_no: usize,
    cause: RecoveryFailureCause,
    schema_version: u32,
    config: &StandardCompactionConfig,
) -> Result<ContextPressureDecision, ContextError> {
    let exhausted = attempt_no >= config.overflow_max_attempts.get() as usize;
    let outcome = if exhausted {
        "exhausted:recoverable_failure".to_string()
    } else {
        format!("failed:{cause:?}")
    };
    emit_recovery_trace(
        traces,
        trace_context,
        TRACE_OVERFLOW_RECOVERY_OUTCOME,
        None,
        Some(outcome.as_str()),
    );
    let mut nodes = vec![recovery_record_node(
        OverflowRecoveryRecord::Failed {
            attempt: attempt_no as u32,
            cause,
        },
        schema_version,
    )?];
    if exhausted {
        nodes.push(recovery_record_node(
            OverflowRecoveryRecord::Exhausted {},
            schema_version,
        )?);
    }
    Ok(ContextPressureDecision::Record { nodes })
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
    config: &StandardCompactionConfig,
) -> Result<ContextPressureDecision, ContextError> {
    let trace_context = lash_core::TraceContext::default().for_session(ctx.session_id.clone());
    if state.exhausted(config) {
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
        return recovery_failure_decision(
            &ctx.traces,
            trace_context,
            attempt_no,
            RecoveryFailureCause::NothingToSummarize,
            ctx.writer_formats.writer_version(
                "OVERFLOW_RECOVERY_FORMAT_VERSION",
                OVERFLOW_RECOVERY_FORMAT_VERSION,
            ),
            config,
        );
    }

    // Elide first: the summarizer request itself must fit its window, and it
    // must never be the rejected oversized request with an instruction
    // appended.
    let mut summarizer_prefix = summary_prefix;
    let elided_parts = elide_oversized_parts(&mut summarizer_prefix, config);

    let summarizer_budget_tokens = config.compaction_threshold(ctx.max_context_tokens.unwrap_or(0));
    let projected_tokens = summarizer_prefix
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(|part| {
            config.approx_token_count(&part.content()).saturating_add(
                config
                    .attachment_tokens
                    .saturating_mul(part.attachments().count()),
            )
        })
        .fold(0usize, usize::saturating_add)
        .saturating_add(config.approx_token_count(&config.overflow_instructions))
        .saturating_add(config.recovery_request_overhead_tokens);
    let (_, prompt_text) = prepare_compaction_request(
        &history_snapshot,
        summarizer_prefix.clone(),
        Some(&recovery_instructions(elided_parts, config)),
        config,
    )?;
    let request_tokens = projected_tokens.saturating_add(config.approx_token_count(&prompt_text));
    if request_tokens > summarizer_budget_tokens {
        return recovery_failure_decision(
            &ctx.traces,
            trace_context,
            attempt_no,
            RecoveryFailureCause::RequestExceedsWindow,
            ctx.writer_formats.writer_version(
                "OVERFLOW_RECOVERY_FORMAT_VERSION",
                OVERFLOW_RECOVERY_FORMAT_VERSION,
            ),
            config,
        );
    }

    // The summarizer is one direct completion on the same seam the ordinary
    // compaction policy uses (FIG-3374), composing the session's compaction
    // sections only when it is admitted, so their renderers never run on
    // turns that recover without summarizing.
    let summary = match summarize_compaction_prefix(
        &ctx.session_id,
        &history_snapshot,
        summarizer_prefix,
        Some(&recovery_instructions(elided_parts, config)),
        config,
        &ctx.direct_completions,
        &ctx.scoped_effect_controller,
    )
    .await
    {
        Ok(Some(summary)) => summary,
        Ok(None) => {
            return recovery_failure_decision(
                &ctx.traces,
                trace_context,
                attempt_no,
                RecoveryFailureCause::EmptySummary,
                ctx.writer_formats.writer_version(
                    "OVERFLOW_RECOVERY_FORMAT_VERSION",
                    OVERFLOW_RECOVERY_FORMAT_VERSION,
                ),
                config,
            );
        }
        Err(error) if error.aborts_invocation() => return Err(error),
        Err(error) => {
            return recovery_failure_decision(
                &ctx.traces,
                trace_context,
                attempt_no,
                RecoveryFailureCause::SummarizerRefused {
                    code: error
                        .into_turn_failure(lash_core::RuntimeErrorCode::ContextCompaction)
                        .code,
                },
                ctx.writer_formats.writer_version(
                    "OVERFLOW_RECOVERY_FORMAT_VERSION",
                    OVERFLOW_RECOVERY_FORMAT_VERSION,
                ),
                config,
            );
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
        records: vec![recovery_record_node(
            OverflowRecoveryRecord::Completed {},
            ctx.writer_formats.writer_version(
                "OVERFLOW_RECOVERY_FORMAT_VERSION",
                OVERFLOW_RECOVERY_FORMAT_VERSION,
            ),
        )?],
        task: OVERFLOW_RECOVERY_TASK.to_string(),
        seed: vec![compaction_summary_seed(&summary)],
    })
}

/// Appends the pending recovery node with the overflowing turn's commit.
pub(crate) async fn overflow_recovery_after_turn(
    ctx: &lash_core::plugin::TurnResultHookContext,
    config: &StandardCompactionConfig,
) -> Result<lash_core::plugin::AfterTurnContributions, PluginError> {
    use lash_core::facade_support::{TurnOutcome, TurnStop};
    use lash_core::plugin::{AfterTurnContributions, PluginRecordContribution};
    if !config.overflow_recovery
        || !matches!(
            ctx.turn.outcome,
            TurnOutcome::Stopped(TurnStop::ContextOverflow)
        )
    {
        return Ok(AfterTurnContributions::default());
    }
    let body = recovery_body(
        OverflowRecoveryRecord::Pending {},
        ctx.writer_formats.writer_version(
            "OVERFLOW_RECOVERY_FORMAT_VERSION",
            OVERFLOW_RECOVERY_FORMAT_VERSION,
        ),
    )
    .map_err(|error| PluginError::Invoke(error.to_string()))?;
    Ok(AfterTurnContributions {
        events: vec![lash_core::PluginRuntimeEvent::Custom {
            name: TRACE_OVERFLOW_RECOVERY_TRIGGER.to_string(),
            payload: serde_json::json!({"trigger": "persisted_context_overflow", "marker": "queued"}),
        }],
        records: vec![PluginRecordContribution {
            plugin_type: OVERFLOW_RECOVERY_PLUGIN_TYPE.to_string(),
            body,
        }],
        ..AfterTurnContributions::default()
    })
}

#[cfg(test)]
mod guarded_surface_tests {
    use super::*;
    use lash_core::testing::guarded_surfaces::{self as laws, SurfaceProbe};
    const OWNER: &str = "lash-plugin-standard-compaction";
    fn write(fleet: lash_core::FleetFormat) -> Vec<u8> {
        let node = recovery_record_node(
            OverflowRecoveryRecord::Pending {},
            fleet.writer_version(lash_core::surface_format!(OVERFLOW_RECOVERY_FORMAT_VERSION)),
        )
        .expect("recovery marker");
        let lash_core::SessionAppendNode::Plugin { body, .. } = node else {
            panic!("plugin marker")
        };
        serde_json::to_vec(&body).expect("JSON")
    }
    fn read(bytes: &[u8], _fleet: lash_core::FleetFormat) -> Result<String, String> {
        decode_recovery_body(serde_json::from_slice(bytes).map_err(|error| error.to_string())?)
            .map(|record| format!("{record:?}"))
            .map_err(|error| error.to_string())
    }
    fn restamp(bytes: &[u8], version: u32) -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("JSON");
        value["format"] = serde_json::json!(version);
        serde_json::to_vec(&value).expect("JSON")
    }
    fn probes() -> Vec<SurfaceProbe> {
        vec![SurfaceProbe {
            constant: "OVERFLOW_RECOVERY_FORMAT_VERSION",
            newest: OVERFLOW_RECOVERY_FORMAT_VERSION,
            write,
            read,
            restamp,
        }]
    }
    #[test]
    fn every_guarded_surface_decodes_its_supported_range() {
        laws::every_guarded_surface_decodes_its_supported_range(OWNER, &probes());
    }
    #[test]
    fn unknown_version_is_refused_with_zero_mutation() {
        laws::unknown_version_is_refused_with_zero_mutation(OWNER, &probes());
    }
    #[test]
    fn upcast_preserves_immutable_bytes_and_hashes() {
        laws::upcast_preserves_immutable_bytes_and_hashes(OWNER, &probes());
    }
}
