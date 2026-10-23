//! The latency gate's deterministic provider fixture.
//!
//! The gated fixture is the "fast provider": an immediate deterministic
//! completion whose own duration is recorded inside the provider's
//! `complete` callback — the `provider` span in the report — so overhead is
//! `send→completion − provider`, exactly what the ticket names. Variants add
//! streamed deltas, a benchmark tool call, a scripted transport failure and
//! the real HTTP path through `OpenAiCompatibleProvider` against the local
//! OpenAI-compatible SSE fixture.
//!
//! A provider call parks when its lane's [`LaneHold`] is armed — the `busy`
//! case's way of keeping a run in flight behind the measured input.

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core::LlmTerminalReason;
use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, LlmUsage,
};
use lash_core::provider::ProviderOptions;
use lash_core::testing::TestProvider;
use lash_protocol_types::sync::MutexExt;
use lash_transport_failure::LlmTransportError;
use lash_vocabulary::identity::SessionId;

use super::loop_spans::CallStamp;
use lash_core_ids::perf_witness::latency::Stamp;

/// The provider fixtures the latency cases select.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LatencyProviderKind {
    /// Immediate small text completion — the gated fixture.
    Text,
    /// The same completion delivered as streamed deltas.
    Stream,
    /// One `benchmark_echo` tool call, then text once its result lands.
    Tool,
    /// A scripted provider transport failure.
    Fail,
    /// The real `OpenAiCompatibleProvider` over loopback HTTP against the
    /// local SSE fixture — the "controlled real provider" leg.
    OpenAiCompat,
    /// Streamed deltas paced [`DELIVERY_GAP`] apart, so each delta's
    /// emit→host delivery is measured on its own rather than in a burst.
    Delivery,
    /// A held answer, then the call that carries the steered input.
    Steer,
    /// An RLM code cell calling `benchmark_echo`, then a finishing cell.
    Cell,
    /// One realistic code-mode cell per history turn.
    HistoryCell {
        continue_as_every: Option<usize>,
        turns: usize,
        growing_heap: bool,
    },
}

/// Provider-side call durations, keyed by session id so concurrent lanes
/// attribute their calls correctly. `held` marks the parked `busy` call so
/// its park time never counts as the measured input's provider time.
#[derive(Default)]
pub(crate) struct ProviderTiming {
    calls: Mutex<HashMap<String, Vec<TimedCall>>>,
    /// Absolute call, delta and tool clocks for the chat-loop spans.
    pub(crate) stamps: super::loop_spans::LoopStamps,
    history_frame: Mutex<String>,
    pub(crate) vm_pool: Mutex<Option<lash_vm_client::WorkerPool>>,
    pub(super) history_vm_service: Mutex<Option<lash_vm_client::service::Service>>,
    pub(super) history_requests: Mutex<Vec<super::history::RequestHistory>>,
}

struct TimedCall {
    duration_ms: f64,
    held: bool,
}

impl ProviderTiming {
    fn record(&self, session: &str, duration_ms: f64, held: bool) {
        if !held {
            lash_core::perf_witness::latency::provider(session, duration_ms);
        }
        self.calls
            .lock_recover()
            .entry(session.to_string())
            .or_default()
            .push(TimedCall { duration_ms, held });
    }

    /// Drain and sum one lane's unheld provider-call durations.
    pub(crate) fn take_ms(&self, session: &SessionId) -> Option<f64> {
        let calls = self
            .calls
            .lock_recover()
            .get_mut(session.as_str())
            .map(std::mem::take)?;
        let total: f64 = calls
            .iter()
            .filter(|call| !call.held)
            .map(|call| call.duration_ms)
            .sum();
        Some(total)
    }
}

/// One lane's park-release control for the `busy` case: the armed call
/// notifies `started` once the provider holds it, then waits on `release`.
pub(crate) struct LaneHold {
    armed: AtomicBool,
    pub(crate) started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl LaneHold {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn take_armed(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn release_one(&self) {
        self.release.notify_one();
    }
}

/// Lane-keyed hold controls the provider closure reads per request.
#[derive(Default)]
pub(crate) struct HoldRegistry {
    lanes: Mutex<HashMap<String, Arc<LaneHold>>>,
}

impl HoldRegistry {
    pub(crate) fn lane(&self, session: &SessionId) -> Arc<LaneHold> {
        self.lanes
            .lock_recover()
            .entry(session.as_str().to_string())
            .or_insert_with(|| Arc::new(LaneHold::new()))
            .clone()
    }
}

/// The deterministic provider for `kind`, recording every call's own
/// duration into `timing` and honouring per-lane holds in `holds`.
pub(crate) fn latency_provider(
    kind: LatencyProviderKind,
    timing: Arc<ProviderTiming>,
    holds: Option<Arc<HoldRegistry>>,
) -> TestProvider {
    let history_turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    TestProvider::builder()
        .kind("latency-gate")
        .requires_streaming(true)
        .options(ProviderOptions {
            // One attempt, no retry fuzz: the gate measures the shift path,
            // not the reliability layer's backoff.
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..ProviderOptions::default()
        })
        .serialize_config(move || serde_json::json!({ "fixture": format!("{kind:?}") }))
        .complete(move |request| {
            let timing = Arc::clone(&timing);
            let holds = holds.clone();
            let history_turn = Arc::clone(&history_turn);
            async move {
                let started = Instant::now();
                let started_stamp = Stamp::now();
                let session = request
                    .session_id()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let mut held = false;
                let hold = holds
                    .as_ref()
                    .and_then(|registry| registry.lanes.lock_recover().get(&session).cloned());
                if let Some(hold) = hold
                    && hold.take_armed()
                {
                    // A steer hold is the running call the steered input
                    // waits behind: provider time, not a parked predecessor.
                    held = kind != LatencyProviderKind::Steer;
                    hold.started.notify_one();
                    hold.release.notified().await;
                }
                let deltas = kind.emit_stream_events(&request).await;
                let history_first = if lash_core_ids::perf_witness::history::enabled() {
                    let _measurement = lash_core_ids::perf_witness::history::Scope::enter(
                        lash_core_ids::perf_witness::history::Component::Measurement,
                    );
                    let mut frame = timing.history_frame.lock_recover();
                    let first = *frame != request.scope.agent_frame_id;
                    *frame = request.scope.agent_frame_id.clone();
                    first
                } else {
                    false
                };
                let response = if let LatencyProviderKind::HistoryCell {
                    continue_as_every,
                    turns,
                    growing_heap,
                } = kind
                {
                    let follow_on = crate::runtime_perf::providers::latest_request_item_contains(
                        &request,
                        super::history::CONTINUE_TASK,
                    );
                    let turn = if follow_on {
                        0
                    } else {
                        history_turn.fetch_add(1, Ordering::Relaxed) + 1
                    };
                    let switch = turn > 0
                        && turn < turns
                        && continue_as_every.is_some_and(|k| turn.is_multiple_of(k));
                    Ok(response(vec![LlmOutputPart::Text {
                        text: super::history::cell_text(
                            switch,
                            follow_on,
                            growing_heap,
                            history_first,
                        ),
                        response_meta: None,
                    }]))
                } else {
                    answer(kind, &request)
                };
                if lash_core_ids::perf_witness::history::enabled() {
                    let _measurement = lash_core_ids::perf_witness::history::Scope::enter(
                        lash_core_ids::perf_witness::history::Component::Measurement,
                    );
                    struct Bytes(usize);
                    impl std::io::Write for Bytes {
                        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                            self.0 += b.len();
                            Ok(b.len())
                        }
                        fn flush(&mut self) -> std::io::Result<()> {
                            Ok(())
                        }
                    }
                    let mut bytes = Bytes(0);
                    serde_json::to_writer(&mut bytes, &request.messages).map_err(|error| {
                        LlmTransportError::new(format!("measure request history: {error}"))
                    })?;
                    timing.history_requests.lock_recover().push(
                        super::history::RequestHistory::new(&request, bytes.0, history_first),
                    );
                    lash_core_ids::perf_witness::history::held(
                        bytes.0,
                        request.messages.len(),
                        format!("{:?}", request.scope.agent_frame_id),
                    );
                }

                timing.stamps.call(
                    &session,
                    CallStamp {
                        started: started_stamp,
                        returned: Stamp::now(),
                        steered: carries_steer(&request),
                        deltas,
                    },
                );
                timing.record(&session, started.elapsed().as_secs_f64() * 1000.0, held);
                response
            }
        })
        .build()
}

fn answer(
    kind: LatencyProviderKind,
    request: &LlmRequest,
) -> Result<LlmResponse, LlmTransportError> {
    match kind {
        LatencyProviderKind::Fail => Err(LlmTransportError::new(
            "latency gate scripted provider failure",
        )),
        LatencyProviderKind::Tool if !request_has_tool_result(request) => tool_call(),
        // A tool call keeps the held turn working, so its after-work
        // checkpoint applies the steered input; a plain answer would finish
        // the turn first and defer the input to the next one.
        LatencyProviderKind::Steer if !carries_steer(request) => tool_call(),
        _ => Ok(response(vec![LlmOutputPart::Text {
            text: kind.text(request),
            response_meta: None,
        }])),
    }
}

fn tool_call() -> Result<LlmResponse, LlmTransportError> {
    Ok(response(vec![LlmOutputPart::ToolCall {
        call_id: "latency-echo-call".to_string(),
        tool_name: "tools__benchmark_echo".to_string(),
        input_json: serde_json::json!({
            "value": "latency gate tool reply",
            "ordinal": 1,
        })
        .to_string(),
        replay: None,
    }]))
}

/// The messages since the model's last reply: the current step's input.
fn since_last_reply(request: &LlmRequest) -> &[lash_core::llm::types::LlmMessage] {
    let start = request
        .messages
        .iter()
        .rposition(|message| message.role == lash_core::llm::types::LlmRole::Assistant)
        .map_or(0, |index| index + 1);
    &request.messages[start..]
}

/// The steered input's text, which the `steer` case's provider recognises.
pub(crate) const STEERED_TEXT: &str = "latency steered input";
/// The `cell` case's user text prefix.
pub(crate) const CELL_SAMPLE_TEXT: &str = "latency cell sample";

/// The current step's input carries the steered input.
fn carries_steer(request: &LlmRequest) -> bool {
    since_last_reply(request).iter().any(|message| {
        message.blocks.iter().any(
            |block| matches!(block, LlmContentBlock::Text { text, .. } if text.contains(STEERED_TEXT)),
        )
    })
}

/// The `cell` case's first call answers the sample's own request with a cell
/// that calls the tool; the call after that cell finishes the turn.
fn cell_text(request: &LlmRequest) -> String {
    let first =
        crate::runtime_perf::providers::latest_request_item_contains(request, CELL_SAMPLE_TEXT);
    let source = if first {
        r#"const echoed = await tools.benchmarkEcho({ value: "latency cell", ordinal: 1 });
console.log(echoed.value);"#
    } else {
        r#"await control.finish("latency cell done");"#
    };
    format!("<typescript>\n{source}\n</typescript>")
}

fn response_text(request: &LlmRequest) -> String {
    format!(
        "latency gate reply for {}",
        request
            .session_id()
            .map(ToString::to_string)
            .unwrap_or_default()
    )
}

/// The current step answers a tool call: every `tool` sample's turn makes
/// one, not only a lane's first (earlier turns' results are history).
fn request_has_tool_result(request: &LlmRequest) -> bool {
    since_last_reply(request).iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    })
}

pub(super) fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    let usage = LlmUsage {
        input_tokens: 1_024,
        output_tokens: 64,
        cache_read_input_tokens: 512,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 48,
        unknown_calls: 0,
    };
    LlmResponse {
        parts,
        usage,
        terminal_reason: LlmTerminalReason::Stop,
        terminal_diagnostic: None,
        discarded_after_cell: None,
        provider_usage: None,
        request_body: None,
        http_summary: None,
        execution_evidence: None,
        generation_disposition: None,
        response_metadata: Default::default(),
        expose_thinking: Some(true),
    }
}

/// The deltas the `stream` case emits before its final part.
const STREAM_DELTAS: usize = 64;
/// The deltas the `delivery` case emits, and the gap between them: about a
/// fast model's token cadence.
const DELIVERY_DELTAS: usize = 32;
pub(crate) const DELIVERY_GAP: std::time::Duration = std::time::Duration::from_millis(5);

impl LatencyProviderKind {
    /// The name a cross-worker child is launched with.
    pub(crate) fn cli_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Tool => "tool",
            _ => "unsupported",
        }
    }

    /// The fixture a cross-worker child serves; only the cross-worker
    /// cases' fixtures are launched in a child.
    pub(crate) fn from_cli_name(name: &str) -> anyhow::Result<Self> {
        match name {
            "text" => Ok(Self::Text),
            "tool" => Ok(Self::Tool),
            other => anyhow::bail!("latency worker cannot serve provider `{other}`"),
        }
    }

    /// The text a non-tool answer carries.
    fn text(self, request: &LlmRequest) -> String {
        match self {
            Self::Cell => cell_text(request),
            _ => response_text(request),
        }
    }

    /// Emit this kind's stream events for `request`, when it streams, and
    /// return each delta's emission clock in order.
    pub(crate) async fn emit_stream_events(&self, request: &LlmRequest) -> Vec<Stamp> {
        let Some(tx) = request.stream_events.as_ref() else {
            return Vec::new();
        };
        let delta = |text: String| {
            let at = Stamp::now();
            tx.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text,
            }));
            at
        };
        let mut deltas = Vec::new();
        match self {
            Self::Stream => {
                for index in 0..STREAM_DELTAS {
                    deltas.push(delta(format!("delta-{index:03} ")));
                }
            }
            Self::Delivery => {
                for index in 0..DELIVERY_DELTAS {
                    if index > 0 {
                        tokio::time::sleep(DELIVERY_GAP).await;
                    }
                    deltas.push(delta(format!("delta-{index:03} ")));
                }
            }
            _ => deltas.push(delta(self.text(request))),
        }
        tx.send(LlmStreamEvent::Usage(LlmUsage {
            input_tokens: 1_024,
            output_tokens: 64,
            cache_read_input_tokens: 512,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 48,
            unknown_calls: 0,
        }));
        deltas
    }
}
