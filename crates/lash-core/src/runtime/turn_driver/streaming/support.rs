use super::*;

pub(super) fn emit_plugin_runtime_events_runtime(
    forwarder: &mut ProviderHostForwarder<'_>,
    plugin_id: &str,
    events: Vec<crate::PluginRuntimeEvent>,
) {
    for event in crate::plugin::plugin_runtime_session_events(plugin_id, events) {
        forwarder.send_semantic_session_event(event);
    }
}

/// Report the clamp on every disposition the call produced.
///
/// The adapter knows only that it put the cap it was handed on the wire, so it
/// reports `Applied`; the runtime is the only layer that saw the larger number
/// the caller asked for. This narrows that report on every carrier of it — the
/// response, each attempt of the ledger, and the partial response an error
/// carries when the adapter salvaged one — so no two accounts of the same
/// request disagree. An adapter that reports nothing keeps reporting nothing:
/// `None` means unreported, not "nothing happened".
pub(super) fn record_clamped_output_token_cap(
    result: &mut Result<LlmResponse, LlmCallError>,
    call_record: Option<&mut crate::LlmCallRecord>,
) {
    fn narrow(disposition: Option<&mut crate::GenerationReceipt>) {
        if let Some(disposition) = disposition
            && disposition.output_token_cap == crate::GenerationOptionOutcome::Applied
        {
            disposition.output_token_cap = crate::GenerationOptionOutcome::ClampedToCapacity;
        }
    }

    match result {
        Ok(response) => narrow(response.generation_disposition.as_mut()),
        Err(error) => narrow(
            error
                .partial_response
                .as_deref_mut()
                .and_then(|partial| partial.generation_disposition.as_mut()),
        ),
    }
    if let Some(call_record) = call_record {
        for attempt in &mut call_record.attempts {
            narrow(attempt.generation_disposition.as_mut());
        }
    }
}

/// Narrow the adapter's wire-level report when protocol projection suppressed
/// caller-owned stop sequences before the request reached the adapter.
pub(super) fn record_protocol_owned_stop_suppression(
    result: &mut Result<LlmResponse, LlmCallError>,
    call_record: Option<&mut crate::LlmCallRecord>,
) {
    fn suppress(disposition: &mut Option<crate::GenerationReceipt>) {
        if let Some(disposition) = disposition {
            disposition.stop_sequences = crate::GenerationOptionOutcome::SuppressedProtocolOwned;
        }
    }

    match result {
        Ok(response) => suppress(&mut response.generation_disposition),
        Err(error) => {
            if let Some(partial) = error.partial_response.as_deref_mut() {
                suppress(&mut partial.generation_disposition);
            }
        }
    }
    if let Some(call_record) = call_record {
        for attempt in &mut call_record.attempts {
            suppress(&mut attempt.generation_disposition);
        }
    }
}

pub(super) fn assistant_stream_finish_reason(
    result: &Result<LlmResponse, LlmCallError>,
    abort_requested: bool,
) -> crate::plugin::AssistantStreamFinishReason {
    use crate::plugin::AssistantStreamFinishReason;

    if abort_requested && result.is_ok() {
        return AssistantStreamFinishReason::Aborted;
    }
    match result {
        Ok(_) => AssistantStreamFinishReason::Complete,
        Err(err) if err.terminal_reason == crate::LlmTerminalReason::Cancelled => {
            AssistantStreamFinishReason::Cancelled
        }
        Err(_) => AssistantStreamFinishReason::ProviderError,
    }
}

/// Which of the recorded step's drive sources resolved first in one poll.
pub(super) enum DrivePollOutcome<J> {
    Cancelled,
    Stream(Box<Option<LlmStreamEvent>>),
    Joined(J),
}

/// Poll the drive's three in-step sources once, in written order: a ready
/// cancellation is never shadowed by a randomly polled stream event, and the
/// branch futures drop with the call so the caller's handlers can borrow the
/// task and receiver again. A drained stream channel stays dormant — the same
/// observation retired that branch under the old select.
pub(super) async fn poll_drive_sources<T>(
    cancel: &CancellationToken,
    llm_task: &mut crate::task::JoinHandle<T>,
    llm_stream_rx: &mut crate::session_model::LlmStreamEventRx,
    stream_closed: bool,
) -> DrivePollOutcome<Result<T, crate::task::JoinError>> {
    let cancelled = cancel.cancelled();
    futures_util::pin_mut!(cancelled);
    let stream_recv = async {
        if stream_closed {
            futures_util::future::pending().await
        } else {
            llm_stream_rx.recv().await
        }
    };
    futures_util::pin_mut!(stream_recv);
    futures_util::select_biased! {
        _ = cancelled.fuse() => DrivePollOutcome::Cancelled,
        stream_event = stream_recv.fuse() => DrivePollOutcome::Stream(Box::new(stream_event)),
        join = (&mut *llm_task).fuse() => DrivePollOutcome::Joined(join),
    }
}

pub(super) struct AbortOnDrop {
    handle: crate::task::AbortHandle,
    armed: bool,
}

impl AbortOnDrop {
    pub(super) fn new(handle: crate::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

pub(super) fn response_usage_is_empty(usage: &LlmUsage) -> bool {
    usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_input_tokens == 0
        && usage.cache_write_input_tokens == 0
        && usage.reasoning_output_tokens == 0
}

pub(super) fn provider_item_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("item_id")
        .or_else(|| value.get("item").and_then(|item| item.get("id")))
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("id"))
        })
        .or_else(|| value.get("id"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

pub(super) fn provider_output_index(value: &serde_json::Value) -> Option<i64> {
    value
        .get("output_index")
        .or_else(|| value.get("index"))
        .and_then(|value| value.as_i64())
}

/// Ordinal band for runtime-minted plugin reasoning blocks, above the
/// provider mint's `0..` space so persisted order stays unambiguous.
pub(super) const PLUGIN_BLOCK_ORDINAL_BASE: u64 = 1 << 62;

pub(super) fn remember_attempt_correlation(
    correlations: &mut Vec<TurnActivityId>,
    correlation_id: &TurnActivityId,
) {
    if !correlations.contains(correlation_id) {
        correlations.push(correlation_id.clone());
    }
}
