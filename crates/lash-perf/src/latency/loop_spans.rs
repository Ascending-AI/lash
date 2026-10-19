//! The chat-loop spans (FIG-6841): what lash adds between the steps of a
//! chat, measured from absolute clocks the harness's own fixtures take.
//!
//! The deterministic provider stamps each call's start (the engine's request
//! reaching the provider), each delta it emits and its return (the model
//! having answered). The benchmark tool stamps its own execution. The host's
//! activity sink stamps what reaches the host. Every stamp is Linux
//! `CLOCK_MONOTONIC`, common to the parent and a cross-worker child, so the
//! spans below join both topologies without engine changes.
//!
//! A sample owns the stamps whose start lies inside its request→completion
//! window on its session: lanes send sequentially, so windows never overlap,
//! and a held predecessor's call (the `busy` case) starts before the window.

use std::collections::{BTreeMap, HashMap};

use lash_core_ids::perf_witness::latency::Stamp;
use lash_protocol_types::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::perf_support::time::round3;

/// One provider call as the fixture saw it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CallStamp {
    pub(crate) started: Stamp,
    pub(crate) returned: Stamp,
    /// The call's request carried the `steer` case's steered input.
    pub(crate) steered: bool,
    /// Each emitted delta, in emission order; the index is the ordinal the
    /// delta's `delta-NNN` text names.
    pub(crate) deltas: Vec<Stamp>,
}

/// One benchmark tool execution as the tool saw it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolStamp {
    pub(crate) started: Stamp,
    pub(crate) returned: Stamp,
}

/// Every fixture stamp of one session, in the order taken.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SessionStamps {
    pub(crate) calls: Vec<CallStamp>,
    pub(crate) tools: Vec<ToolStamp>,
}

/// Session-keyed fixture stamps. A cross-worker child exports its registry
/// on clean shutdown and the parent merges it before deriving spans.
#[derive(Default)]
pub(crate) struct LoopStamps {
    sessions: std::sync::Mutex<HashMap<String, SessionStamps>>,
}

impl LoopStamps {
    pub(crate) fn call(&self, session: &str, call: CallStamp) {
        self.sessions
            .lock_recover()
            .entry(session.to_owned())
            .or_default()
            .calls
            .push(call);
    }

    pub(crate) fn tool(&self, session: &str, tool: ToolStamp) {
        self.sessions
            .lock_recover()
            .entry(session.to_owned())
            .or_default()
            .tools
            .push(tool);
    }

    pub(crate) fn export(&self) -> HashMap<String, SessionStamps> {
        self.sessions.lock_recover().clone()
    }

    pub(crate) fn merge(&self, other: HashMap<String, SessionStamps>) {
        let mut sessions = self.sessions.lock_recover();
        for (session, stamps) in other {
            let entry = sessions.entry(session).or_default();
            entry.calls.extend(stamps.calls);
            entry.tools.extend(stamps.tools);
        }
    }

    /// The session's stamps that start inside `[from, to]`.
    pub(crate) fn window(&self, session: &str, from: Stamp, to: Stamp) -> SessionStamps {
        let inside = |stamp: &Stamp| {
            stamp.monotonic_ns >= from.monotonic_ns && stamp.monotonic_ns <= to.monotonic_ns
        };
        let sessions = self.sessions.lock_recover();
        let Some(stamps) = sessions.get(session) else {
            return SessionStamps::default();
        };
        SessionStamps {
            calls: stamps
                .calls
                .iter()
                .filter(|call| inside(&call.started))
                .cloned()
                .collect(),
            tools: stamps
                .tools
                .iter()
                .filter(|tool| inside(&tool.started))
                .cloned()
                .collect(),
        }
    }
}

/// What the host's activity sink saw, stamped on receipt.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum HostEvent {
    /// An assistant-text delta, with the `delta-NNN` ordinals its
    /// (possibly coalesced) text carries.
    Delta { at: Stamp, ordinals: Vec<usize> },
    /// The engine published a protocol iteration's recorded checkpoint.
    CheckpointRecorded { at: Stamp },
}

impl HostEvent {
    fn at(&self) -> Stamp {
        match self {
            Self::Delta { at, .. } | Self::CheckpointRecorded { at } => *at,
        }
    }
}

/// The `delta-NNN` ordinals a delta's text names.
pub(crate) fn delta_ordinals(text: &str) -> Vec<usize> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix("delta-")?.parse().ok())
        .collect()
}

/// The host-side clocks a chat-loop case adds to a sample.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LoopClocks {
    /// `cold`: session creation and open began here, before the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_open_started: Option<Stamp>,
    /// `steer`: the steered input's send and its acceptance acknowledgement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) steer_request: Option<Stamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) steer_accepted: Option<Stamp>,
    pub(crate) host_events: Vec<HostEvent>,
    /// The fixture stamps this sample's window owns, filled after the case.
    pub(crate) provider_calls: Vec<CallStamp>,
    pub(crate) tools: Vec<ToolStamp>,
}

/// Every chat-loop span of one sample, in milliseconds. `None` where the
/// sample's case does not exercise the step.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LoopSpans {
    /// Send → the engine's first provider request of the turn.
    pub(crate) request_to_provider: Option<f64>,
    /// Back-to-back next turn: the previous turn's host completion → this
    /// turn's first provider request (includes the harness's own re-send).
    pub(crate) previous_completion_to_provider: Option<f64>,
    /// `cold`: session creation and open, before the first send.
    pub(crate) session_open: Option<f64>,
    /// Send → the first delta at the host, less the provider's own time
    /// before it emitted that delta.
    pub(crate) first_delta_overhead: Option<f64>,
    /// Provider emitted a delta → the host received it, one per delta.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) delta_delivery: Vec<f64>,
    /// The model returned a tool call (or a cell) → the tool started.
    pub(crate) model_to_tool_start: Option<f64>,
    pub(crate) tool: Option<f64>,
    /// The tool returned → the host saw the next recorded checkpoint.
    pub(crate) tool_to_recorded: Option<f64>,
    /// The tool returned → the next provider request started.
    pub(crate) tool_to_next_request: Option<f64>,
    /// The model returned a tool call → the next provider request, less the
    /// tool's own execution.
    pub(crate) tool_loop_overhead: Option<f64>,
    /// `steer`: the steered send → its acceptance acknowledgement.
    pub(crate) steer_request_to_accept: Option<f64>,
    /// `steer`: the running call returned (the earliest boundary) → the
    /// provider request that carries the steered input.
    pub(crate) boundary_to_steer_seen: Option<f64>,
    /// `steer`: the steered send → the provider request that carries it.
    pub(crate) steer_send_to_model: Option<f64>,
}

/// Which loop a case's samples run, so only its own spans are derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopShape {
    /// One model call; the next sample follows the completion directly.
    BackToBack,
    /// A model tool call (or code cell) then a second model call.
    ToolLoop,
    /// One held model call, a steered input, then the call that sees it.
    Steer,
    /// Each sample opens a fresh session before its send.
    Cold,
    /// A predecessor run is held in flight: no loop span is derived.
    Held,
}

/// Derive one sample's spans from its own clocks.
pub(crate) fn spans_of(
    shape: LoopShape,
    request: Stamp,
    host_first_delta: Option<Stamp>,
    clocks: &LoopClocks,
) -> LoopSpans {
    let mut spans = LoopSpans::default();
    if shape == LoopShape::Held {
        return spans;
    }
    let calls = &clocks.provider_calls;
    let first = calls.first();
    spans.request_to_provider = first.map(|call| call.started.since(request));
    spans.session_open = clocks
        .session_open_started
        .map(|started| request.since(started));
    if let (Some(host), Some(call)) = (host_first_delta, first)
        && let Some(emitted) = call.deltas.first()
    {
        spans.first_delta_overhead = Some(host.since(request) - emitted.since(call.started));
    }
    spans.delta_delivery = delta_delivery(calls, &clocks.host_events);
    match shape {
        LoopShape::ToolLoop => {
            if let (Some(model), Some(tool)) = (first, clocks.tools.first()) {
                spans.model_to_tool_start = Some(tool.started.since(model.returned));
                spans.tool = Some(tool.returned.since(tool.started));
                spans.tool_to_recorded = clocks
                    .host_events
                    .iter()
                    .filter(|event| matches!(event, HostEvent::CheckpointRecorded { .. }))
                    .map(HostEvent::at)
                    .find(|at| at.monotonic_ns >= tool.returned.monotonic_ns)
                    .map(|at| at.since(tool.returned));
                if let Some(next) = calls.get(1) {
                    spans.tool_to_next_request = Some(next.started.since(tool.returned));
                    spans.tool_loop_overhead = Some(
                        next.started.since(model.returned) - tool.returned.since(tool.started),
                    );
                }
            }
        }
        LoopShape::Steer => {
            if let (Some(sent), Some(accepted)) = (clocks.steer_request, clocks.steer_accepted) {
                spans.steer_request_to_accept = Some(accepted.since(sent));
            }
            let seen = calls.iter().find(|call| call.steered);
            let boundary = calls.iter().rfind(|call| {
                !call.steered
                    && seen
                        .is_some_and(|seen| call.returned.monotonic_ns <= seen.started.monotonic_ns)
            });
            if let (Some(seen), Some(boundary)) = (seen, boundary) {
                spans.boundary_to_steer_seen = Some(seen.started.since(boundary.returned));
            }
            if let (Some(seen), Some(sent)) = (seen, clocks.steer_request) {
                spans.steer_send_to_model = Some(seen.started.since(sent));
            }
        }
        LoopShape::BackToBack | LoopShape::Cold | LoopShape::Held => {}
    }
    spans
}

/// Pair each emitted delta with the first host receipt that names its
/// ordinal. Coalescing may deliver several ordinals in one host delta.
fn delta_delivery(calls: &[CallStamp], events: &[HostEvent]) -> Vec<f64> {
    let Some(call) = calls.first() else {
        return Vec::new();
    };
    let mut received: BTreeMap<usize, Stamp> = BTreeMap::new();
    for event in events {
        if let HostEvent::Delta { at, ordinals } = event {
            for ordinal in ordinals {
                received.entry(*ordinal).or_insert(*at);
            }
        }
    }
    call.deltas
        .iter()
        .enumerate()
        .filter_map(|(ordinal, emitted)| received.get(&ordinal).map(|at| at.since(*emitted)))
        .collect()
}

/// Back-to-back lanes: each turn's first provider request measured from the
/// previous turn's host completion. `lane` holds one lane's samples in send
/// order as `(completion, spans)`.
pub(crate) fn chain_next_turns(lane: &mut [(Stamp, Stamp, &mut LoopSpans)]) {
    for index in 1..lane.len() {
        let previous_completion = lane[index - 1].0;
        let (_, request, spans) = &mut lane[index];
        spans.previous_completion_to_provider = spans
            .request_to_provider
            .map(|to_provider| to_provider + request.since(previous_completion));
    }
}

/// Give each sample the fixture stamps its window owns, derive its chat-loop
/// spans, and chain back-to-back lanes' next-turn starts.
pub(crate) fn fill(
    shape: LoopShape,
    samples: &mut [super::runner::Sample],
    timing: &super::provider::ProviderTiming,
) {
    for sample in samples.iter_mut() {
        let owned = timing.stamps.window(
            &sample.session_id,
            sample.clocks.request,
            sample.clocks.completion,
        );
        sample.loop_clocks.provider_calls = owned.calls;
        sample.loop_clocks.tools = owned.tools;
        sample.loop_spans = spans_of(
            shape,
            sample.clocks.request,
            sample.clocks.host_first_delta,
            &sample.loop_clocks,
        );
    }
    if matches!(shape, LoopShape::BackToBack | LoopShape::ToolLoop) {
        // Samples are sorted by (lane, index): each lane is one contiguous run.
        for lane in samples.chunk_by_mut(|a, b| a.lane == b.lane) {
            let mut chain: Vec<_> = lane
                .iter_mut()
                .map(|sample| {
                    (
                        sample.clocks.completion,
                        sample.clocks.request,
                        &mut sample.loop_spans,
                    )
                })
                .collect();
            chain_next_turns(&mut chain);
        }
    }
    for sample in samples.iter_mut() {
        rounded(&mut sample.loop_spans);
    }
}

/// Percentiles of every chat-loop span one case reports.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct LoopSummaries {
    pub(crate) request_to_provider: Option<super::runner::LatencySummary>,
    pub(crate) previous_completion_to_provider: Option<super::runner::LatencySummary>,
    pub(crate) session_open: Option<super::runner::LatencySummary>,
    pub(crate) first_delta_overhead: Option<super::runner::LatencySummary>,
    pub(crate) delta_delivery: Option<super::runner::LatencySummary>,
    pub(crate) model_to_tool_start: Option<super::runner::LatencySummary>,
    pub(crate) tool: Option<super::runner::LatencySummary>,
    pub(crate) tool_to_recorded: Option<super::runner::LatencySummary>,
    pub(crate) tool_to_next_request: Option<super::runner::LatencySummary>,
    pub(crate) tool_loop_overhead: Option<super::runner::LatencySummary>,
    pub(crate) steer_request_to_accept: Option<super::runner::LatencySummary>,
    pub(crate) boundary_to_steer_seen: Option<super::runner::LatencySummary>,
    pub(crate) steer_send_to_model: Option<super::runner::LatencySummary>,
}

impl LoopSummaries {
    pub(crate) fn of<'a>(spans: impl Iterator<Item = &'a LoopSpans> + Clone) -> Self {
        let column = |pick: &dyn Fn(&LoopSpans) -> Option<f64>| {
            super::runner::summarize(spans.clone().filter_map(pick))
        };
        Self {
            request_to_provider: column(&|s| s.request_to_provider),
            previous_completion_to_provider: column(&|s| s.previous_completion_to_provider),
            session_open: column(&|s| s.session_open),
            first_delta_overhead: column(&|s| s.first_delta_overhead),
            delta_delivery: super::runner::summarize(
                spans.clone().flat_map(|s| s.delta_delivery.iter().copied()),
            ),
            model_to_tool_start: column(&|s| s.model_to_tool_start),
            tool: column(&|s| s.tool),
            tool_to_recorded: column(&|s| s.tool_to_recorded),
            tool_to_next_request: column(&|s| s.tool_to_next_request),
            tool_loop_overhead: column(&|s| s.tool_loop_overhead),
            steer_request_to_accept: column(&|s| s.steer_request_to_accept),
            boundary_to_steer_seen: column(&|s| s.boundary_to_steer_seen),
            steer_send_to_model: column(&|s| s.steer_send_to_model),
        }
    }

    /// The populated rows, for the human summary.
    pub(crate) fn rows(&self) -> Vec<(&'static str, &super::runner::LatencySummary)> {
        [
            ("send→provider request ms", &self.request_to_provider),
            (
                "prev completion→next provider ms",
                &self.previous_completion_to_provider,
            ),
            ("session open ms", &self.session_open),
            ("first delta overhead ms", &self.first_delta_overhead),
            ("delta delivery ms", &self.delta_delivery),
            ("model→tool start ms", &self.model_to_tool_start),
            ("tool ms", &self.tool),
            ("tool→recorded ms", &self.tool_to_recorded),
            ("tool→next request ms", &self.tool_to_next_request),
            ("tool loop overhead ms", &self.tool_loop_overhead),
            ("steer send→accept ms", &self.steer_request_to_accept),
            ("boundary→steer seen ms", &self.boundary_to_steer_seen),
            ("steer send→model ms", &self.steer_send_to_model),
        ]
        .into_iter()
        .filter_map(|(label, summary)| summary.as_ref().map(|summary| (label, summary)))
        .collect()
    }
}

/// Round a derived span the way the report rounds phases.
pub(crate) fn rounded(spans: &mut LoopSpans) {
    for value in [
        &mut spans.request_to_provider,
        &mut spans.previous_completion_to_provider,
        &mut spans.session_open,
        &mut spans.first_delta_overhead,
        &mut spans.model_to_tool_start,
        &mut spans.tool,
        &mut spans.tool_to_recorded,
        &mut spans.tool_to_next_request,
        &mut spans.tool_loop_overhead,
        &mut spans.steer_request_to_accept,
        &mut spans.boundary_to_steer_seen,
        &mut spans.steer_send_to_model,
    ] {
        *value = value.map(round3);
    }
    for value in &mut spans.delta_delivery {
        *value = round3(*value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Stamp {
        Stamp {
            monotonic_ns: ms * 1_000_000,
            unix_ns: 1,
        }
    }

    fn call(started: u64, returned: u64, steered: bool, deltas: &[u64]) -> CallStamp {
        CallStamp {
            started: at(started),
            returned: at(returned),
            steered,
            deltas: deltas.iter().copied().map(at).collect(),
        }
    }

    /// TOOL-LOOP: the model's tool call returns → the tool starts → it
    /// returns → the next provider request; the loop's overhead excludes the
    /// tool's own time, and "recorded" is the first checkpoint after the tool.
    #[test]
    fn tool_loop_spans_run_from_the_model_return_through_the_tool_to_the_next_request() {
        let clocks = LoopClocks {
            host_events: vec![
                HostEvent::CheckpointRecorded { at: at(108) },
                HostEvent::CheckpointRecorded { at: at(125) },
            ],
            provider_calls: vec![call(110, 112, false, &[111]), call(140, 141, false, &[])],
            tools: vec![ToolStamp {
                started: at(117),
                returned: at(120),
            }],
            ..LoopClocks::default()
        };

        let spans = spans_of(LoopShape::ToolLoop, at(100), None, &clocks);

        assert_eq!(spans.request_to_provider, Some(10.0));
        assert_eq!(spans.model_to_tool_start, Some(5.0));
        assert_eq!(spans.tool, Some(3.0));
        assert_eq!(spans.tool_to_recorded, Some(5.0));
        assert_eq!(spans.tool_to_next_request, Some(20.0));
        assert_eq!(spans.tool_loop_overhead, Some(25.0));
    }

    /// NEXT-TURN: a back-to-back turn's start runs from the previous turn's
    /// host completion to its own first provider request, harness re-send
    /// included; a lane's first turn has no predecessor.
    #[test]
    fn next_turn_start_runs_from_the_previous_completion_to_the_next_provider_request() {
        let mut first = LoopSpans {
            request_to_provider: Some(4.0),
            ..LoopSpans::default()
        };
        let mut second = LoopSpans {
            request_to_provider: Some(6.0),
            ..LoopSpans::default()
        };
        let mut lane = [(at(50), at(10), &mut first), (at(90), at(52), &mut second)];

        chain_next_turns(&mut lane);

        assert_eq!(first.previous_completion_to_provider, None);
        assert_eq!(second.previous_completion_to_provider, Some(8.0));
    }

    /// STEER: a steered input reaches the model at the first request that
    /// carries it, measured from the send and from the boundary — the
    /// return of the call it could not join.
    #[test]
    fn steer_delivery_runs_to_the_request_that_carries_the_steered_input() {
        let clocks = LoopClocks {
            steer_request: Some(at(105)),
            steer_accepted: Some(at(109)),
            provider_calls: vec![call(102, 111, false, &[]), call(118, 119, true, &[])],
            ..LoopClocks::default()
        };

        let spans = spans_of(LoopShape::Steer, at(100), None, &clocks);

        assert_eq!(spans.steer_request_to_accept, Some(4.0));
        assert_eq!(spans.boundary_to_steer_seen, Some(7.0));
        assert_eq!(spans.steer_send_to_model, Some(13.0));
    }

    /// DELTA-DELIVERY: each emitted delta pairs with the first host delta
    /// naming its ordinal, so a coalesced host delta delivers several; the
    /// first-delta overhead subtracts the provider's own time to emit it.
    #[test]
    fn delta_delivery_pairs_each_emitted_delta_with_its_first_host_receipt() {
        assert_eq!(delta_ordinals("delta-000 delta-001 "), vec![0, 1]);
        let clocks = LoopClocks {
            host_events: vec![
                HostEvent::Delta {
                    at: at(115),
                    ordinals: vec![0, 1],
                },
                HostEvent::Delta {
                    at: at(130),
                    ordinals: vec![1, 2],
                },
            ],
            provider_calls: vec![call(103, 125, false, &[105, 110, 120])],
            ..LoopClocks::default()
        };

        let spans = spans_of(LoopShape::BackToBack, at(100), Some(at(115)), &clocks);

        assert_eq!(spans.delta_delivery, vec![10.0, 5.0, 10.0]);
        assert_eq!(spans.first_delta_overhead, Some(13.0));
    }

    /// WINDOW: a sample owns only the stamps that start inside its
    /// request→completion window on its own session.
    #[test]
    fn a_sample_owns_the_stamps_that_start_inside_its_window() {
        let stamps = LoopStamps::default();
        stamps.call("a", call(5, 20, false, &[]));
        stamps.call("a", call(12, 14, false, &[]));
        stamps.call("b", call(13, 15, false, &[]));
        stamps.tool(
            "a",
            ToolStamp {
                started: at(31),
                returned: at(32),
            },
        );

        let owned = stamps.window("a", at(10), at(30));

        assert_eq!(owned.calls.len(), 1);
        assert_eq!(owned.calls[0].started.monotonic_ns, at(12).monotonic_ns);
        assert!(owned.tools.is_empty());
    }
}
