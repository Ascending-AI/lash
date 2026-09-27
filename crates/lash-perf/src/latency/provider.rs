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
//! case's way of keeping a root in flight behind the measured input.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core::LlmTerminalReason;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, LlmUsage,
};
use lash_core::provider::ProviderOptions;
use lash_core::testing::TestProvider;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

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
}

/// Provider-side call durations, keyed by session id so concurrent lanes
/// attribute their calls correctly. `held` marks the parked `busy` call so
/// its park time never counts as the measured input's provider time.
#[derive(Default)]
pub(crate) struct ProviderTiming {
    calls: Mutex<HashMap<String, Vec<TimedCall>>>,
}

struct TimedCall {
    duration_ms: f64,
    held: bool,
}

impl ProviderTiming {
    fn record(&self, session: &str, duration_ms: f64, held: bool) {
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
    TestProvider::builder()
        .kind("latency-gate")
        .requires_streaming(true)
        .options(ProviderOptions {
            // One attempt, no retry fuzz: the gate measures the drive path,
            // not the reliability layer's backoff.
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..ProviderOptions::default()
        })
        .serialize_config(move || serde_json::json!({ "fixture": format!("{kind:?}") }))
        .complete(move |request| {
            let timing = Arc::clone(&timing);
            let holds = holds.clone();
            async move {
                let started = Instant::now();
                let session = request.session_id().to_string();
                let mut held = false;
                let hold = holds
                    .as_ref()
                    .and_then(|registry| registry.lanes.lock_recover().get(&session).cloned());
                if let Some(hold) = hold
                    && hold.take_armed()
                {
                    held = true;
                    hold.started.notify_one();
                    hold.release.notified().await;
                }
                kind.emit_stream_events(&request);
                let response = answer(kind, &request);
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
        LatencyProviderKind::Tool if !request_has_tool_result(request) => {
            Ok(response(vec![LlmOutputPart::ToolCall {
                call_id: "latency-echo-call".to_string(),
                tool_name: "benchmark_echo".to_string(),
                input_json: serde_json::json!({
                    "value": "latency gate tool reply",
                    "ordinal": 1,
                })
                .to_string(),
                replay: None,
            }]))
        }
        _ => Ok(response(vec![LlmOutputPart::Text {
            text: response_text(request),
            response_meta: None,
        }])),
    }
}

fn response_text(request: &LlmRequest) -> String {
    format!("latency gate reply for {}", request.session_id())
}

fn request_has_tool_result(request: &LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    })
}

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    let usage = LlmUsage {
        input_tokens: 1_024,
        output_tokens: 64,
        cache_read_input_tokens: 512,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 48,
    };
    LlmResponse {
        parts,
        usage,
        terminal_reason: LlmTerminalReason::Stop,
        terminal_diagnostic: None,
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

impl LatencyProviderKind {
    /// Emit this kind's stream events for `request`, when it streams.
    pub(crate) fn emit_stream_events(&self, request: &LlmRequest) {
        let Some(tx) = request.stream_events.as_ref() else {
            return;
        };
        match self {
            Self::Stream => {
                for index in 0..STREAM_DELTAS {
                    tx.send(LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: format!("delta-{index:03} "),
                    });
                }
            }
            _ => {
                tx.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: response_text(request),
                });
            }
        }
        tx.send(LlmStreamEvent::Usage(LlmUsage {
            input_tokens: 1_024,
            output_tokens: 64,
            cache_read_input_tokens: 512,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 48,
        }));
    }
}
