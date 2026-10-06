use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::oracles::replay_determinism;
use crate::runtime_contracts::{
    RuntimeAgentFrameInvariantFacts, RuntimeGraphInvariantFacts, RuntimeUsageInvariantFacts,
};
use crate::scheduler::{BoundaryKind, BoundaryScheduler, DeliveredBoundary};
use crate::store::ModelStore;
use crate::trace::{
    ReplayReport, RuntimeInvariantReverification, SimulationTrace, TRACE_SCHEMA, TraceIoError,
    read_trace, write_replay_report,
};

#[derive(Debug)]
#[non_exhaustive]
pub enum ReplayError {
    TraceIo(TraceIoError),
    IncompatibleTrace(String),
    MissingBoundary(String),
    Divergence(String),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::IncompatibleTrace(message) => write!(f, "incompatible replay trace: {message}"),
            Self::MissingBoundary(id) => write!(f, "replay boundary `{id}` was not scheduled"),
            Self::Divergence(message) => write!(f, "replay diverged: {message}"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<TraceIoError> for ReplayError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

pub fn replay_trace_file(
    trace_path: &Path,
    report_path: Option<&Path>,
) -> Result<ReplayReport, ReplayError> {
    let trace = read_trace(trace_path)?;
    let report = replay_trace(trace_path, &trace)?;
    if let Some(report_path) = report_path {
        write_replay_report(report_path, &report)?;
    }
    Ok(report)
}

pub fn replay_trace(
    trace_path: &Path,
    trace: &SimulationTrace,
) -> Result<ReplayReport, ReplayError> {
    if trace.schema != TRACE_SCHEMA {
        return Err(ReplayError::IncompatibleTrace(format!(
            "expected schema `{TRACE_SCHEMA}`, got `{}`",
            trace.schema
        )));
    }
    let mut scheduler = BoundaryScheduler::with_events(
        trace.seed,
        trace.events.iter().map(|event| event.as_event()),
    );
    let mut store = ModelStore::default();
    let mut sequence = Vec::new();

    for expected in &trace.events {
        let event = expected.as_event();
        // A backend fault is produced by the script over the REAL store at
        // generation time, and a suspend resume by the real parked turn. The abstract
        // ModelStore cannot re-derive either, so the model carries the recorded
        // observation rather than fabricating it.
        let observed = if event.kind == BoundaryKind::BackendFailure
            || event.payload.get("suspend_resume").and_then(Value::as_bool) == Some(true)
        {
            store.apply_observed_boundary(&event, &expected.observed);
            expected.observed.clone()
        } else {
            store.apply_boundary(&event)
        };
        let delivered = scheduler
            .deliver_boundary(&expected.boundary_id, observed)
            .ok_or_else(|| ReplayError::MissingBoundary(expected.boundary_id.clone()))?;
        let actual_observed = normalize(event.kind, &delivered.observed);
        let expected_observed = normalize(event.kind, &expected.observed);
        if actual_observed != expected_observed {
            return Err(ReplayError::Divergence(format!(
                "boundary `{}` observed payload changed; expected={}; actual={}",
                expected.boundary_id, expected_observed, actual_observed
            )));
        }
        if let Some(admissions) = event
            .payload
            .get("provider_admissions")
            .and_then(Value::as_array)
        {
            store.apply_provider_admissions(admissions);
        }
        sequence.push(delivered.boundary_id);
    }

    if !scheduler.is_empty() {
        return Err(ReplayError::Divergence(format!(
            "{} boundaries remained pending after replay",
            scheduler.pending_len()
        )));
    }

    // The abstract model cannot execute a runtime commit. It therefore carries
    // the trace's checkpoint observations as recorded evidence; real backend
    // replay lanes independently re-execute and compare their runtime-turn
    // subset before using observed commits in their summaries.
    let final_summary = store
        .summarize_with_trace_checkpoint_writes(&trace.events, &trace.durable_writes)
        .map_err(ReplayError::Divergence)?;
    let terminal_verdict = replay_determinism(&trace.final_summary, &final_summary);
    if !terminal_verdict.is_passed() {
        return Err(ReplayError::Divergence(terminal_verdict.message.clone()));
    }

    // Boundary-equality replay normalizes the real-runtime invariant facts away
    // (they are not reproducible from the abstract `ModelStore` projection), so
    // model-store agreement alone never re-proves the runtime-level invariants.
    // Re-derive each turn's graph/agent-frame/usage verdict from its recorded
    // structural facts so reproduction is proven at the runtime level, not only
    // at the abstract-store level.
    let runtime_invariant_reverification = reverify_runtime_invariant_facts(&trace.events)?;

    Ok(ReplayReport::new(
        trace_path,
        terminal_verdict,
        sequence,
        final_summary,
        runtime_invariant_reverification,
    ))
}

/// Re-derive the pass/fail of every recorded runtime invariant from its
/// structural facts (cycle/duplicate/missing-parent node sets, active-frame
/// cardinality, negative/non-monotonic usage). A trace whose recorded `passed`
/// flag disagrees with its own structure — or whose facts reveal a violation —
/// is a runtime-level reproduction failure and diverges.
pub fn reverify_runtime_invariant_facts(
    events: &[DeliveredBoundary],
) -> Result<RuntimeInvariantReverification, ReplayError> {
    let mut reverification = RuntimeInvariantReverification {
        schema: "lash.sim.runtime-invariant-reverification.v1".to_string(),
        ..RuntimeInvariantReverification::default()
    };
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = event.observed.get("runtime_invariant_facts") else {
            continue;
        };
        reverification.reverified_turn_count += 1;

        if let Some(graph) = facts.get("graph") {
            let recorded = recorded_passed(event, "graph", graph)?;
            let graph: RuntimeGraphInvariantFacts =
                serde_json::from_value(graph.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable graph invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = graph.passed();
            require_reverified(
                event,
                "graph",
                recomputed,
                recorded,
                format!(
                    "duplicates={:?} missing_parents={:?} cycles={:?} leaf_exists={}",
                    graph.duplicate_node_ids,
                    graph.missing_parent_links,
                    graph.cycle_node_ids,
                    graph.leaf_exists
                ),
            )?;
            require_invariants_flag(event, "graph_acyclic", graph.cycle_node_ids.is_empty())?;
            reverification.graph_invariant_checks += 1;
        }

        if let Some(agent_frame) = facts.get("agent_frame") {
            let recorded = recorded_passed(event, "agent_frame", agent_frame)?;
            let agent_frame: RuntimeAgentFrameInvariantFacts =
                serde_json::from_value(agent_frame.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable agent-frame invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = agent_frame.passed();
            require_reverified(
                event,
                "agent_frame",
                recomputed,
                recorded,
                format!(
                    "active_frames={:?} current={} exists={} active={} orphan_frames={:?}",
                    agent_frame.active_frame_ids,
                    agent_frame.current_frame_node_id,
                    agent_frame.current_frame_exists,
                    agent_frame.current_frame_active,
                    agent_frame.node_agent_frame_ids_without_record
                ),
            )?;
            require_invariants_flag(
                event,
                "single_active_agent_frame",
                agent_frame.active_frame_ids.len() == 1,
            )?;
            reverification.agent_frame_invariant_checks += 1;
        }

        if let Some(usage) = facts.get("usage") {
            let recorded = recorded_passed(event, "usage", usage)?;
            let usage: RuntimeUsageInvariantFacts =
                serde_json::from_value(usage.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable usage invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = usage.passed();
            require_reverified(
                event,
                "usage",
                recomputed,
                recorded,
                format!(
                    "negative_fields={:?} non_negative={} monotonic={}",
                    usage.negative_fields, usage.non_negative, usage.usage_events_monotonic
                ),
            )?;
            require_invariants_flag(event, "usage_monotonic", usage.usage_events_monotonic)?;
            reverification.usage_invariant_checks += 1;
        }
    }
    Ok(reverification)
}

/// The recorded `passed` flag is evidence to check against, never an input to
/// the recompute: it is read from the raw fact JSON so a trace whose stored
/// flag disagrees with its own structural fields still diverges.
fn recorded_passed(
    event: &DeliveredBoundary,
    invariant: &str,
    facts: &Value,
) -> Result<bool, ReplayError> {
    facts.get("passed").and_then(Value::as_bool).ok_or_else(|| {
        ReplayError::Divergence(format!(
            "boundary `{}` recorded an unreadable {invariant} invariant fact: missing field `passed`",
            event.boundary_id
        ))
    })
}

fn require_reverified(
    event: &DeliveredBoundary,
    invariant: &str,
    recomputed: bool,
    recorded: bool,
    detail: String,
) -> Result<(), ReplayError> {
    if recomputed != recorded {
        return Err(ReplayError::Divergence(format!(
            "boundary `{}` recorded {invariant} invariant passed={recorded} but its structural facts re-derive {recomputed} ({detail})",
            event.boundary_id
        )));
    }
    if !recomputed {
        return Err(ReplayError::Divergence(format!(
            "boundary `{}` {invariant} invariant violated on replay ({detail})",
            event.boundary_id
        )));
    }
    Ok(())
}

fn require_invariants_flag(
    event: &DeliveredBoundary,
    flag: &str,
    recomputed: bool,
) -> Result<(), ReplayError> {
    let Some(recorded) = event
        .observed
        .get("runtime_invariants")
        .and_then(|invariants| invariants.get(flag))
        .and_then(Value::as_bool)
    else {
        return Ok(());
    };
    if recorded != recomputed {
        return Err(ReplayError::Divergence(format!(
            "boundary `{}` recorded runtime_invariants.{flag}={recorded} but structural facts re-derive {recomputed}",
            event.boundary_id
        )));
    }
    Ok(())
}

fn normalize(kind: BoundaryKind, value: &Value) -> Value {
    let mut value = value.clone();
    if kind == BoundaryKind::ProviderEvent
        && let Some(blocked) =
            value.pointer_mut("/scripted_transport_release/blocked_before_release")
        && blocked.is_boolean()
    {
        // A release makes bytes available even before the provider polls its
        // gate (c92413ede122). ADR 0044 leaves host task polling uncontrolled;
        // FIG-1152 permits normalizing only such nondeterministic observations.
        // Use the model's parked case for either valid boolean, keeping missing
        // or malformed evidence distinct and every release identity/time intact.
        // The trace retains the actual observation because this is a clone.
        *blocked = Value::Bool(true);
    }
    if kind == BoundaryKind::QueuedIngress
        && let Some(object) = value.as_object_mut()
        && object.get("input_id").and_then(Value::as_str).is_some()
    {
        // Pending-turn-input ids are handed out by the backend in the order it
        // accepts admissions, and every turn — direct or queued — now enters
        // through one acceptance commit (ADR 0069). Which id a queued admission
        // gets therefore depends on how many turns the runtime had already
        // accepted inside a boundary the abstract stream models as one event, so
        // the model cannot predict it. Identity is compared where it is owned:
        // the cross-backend replays mask it the same way, and the admission
        // contract itself is pinned by the store conformance suites. Source key,
        // ingress mode, and input state stay compared here.
        object.insert(
            "input_id".to_string(),
            Value::String("<backend-assigned>".to_string()),
        );
    }
    if let Some(object) = value.as_object_mut() {
        // A completed provider future is harvested on the first host scheduler
        // pass that observes its join handle as finished. Re-running an identical
        // seed can therefore change only the virtual timestamp at which that
        // harvest occurs; the scheduled provider releases and runtime state are
        // compared separately below and by the independent state checker.
        object.remove("sim_clock");
        // The parser matrix is produced by executing four real provider stacks,
        // including transport timeout/disconnect classifications whose outcome
        // depends on host task wakeups outside the abstract boundary model. Its
        // dedicated parser-matrix oracles compare the full result; boundary
        // replay cannot predict it without reusing the implementation it checks.
        object.remove("provider_parser_matrix");
    }
    if let Some(graph) = value
        .pointer_mut("/runtime_invariant_facts/graph")
        .and_then(Value::as_object_mut)
    {
        // History node ids are derived from the runtime's per-turn operation
        // identity, which is allocated from entropy and is intentionally absent
        // from the generated boundary stream. Graph shape, counts, edges, and all
        // invariant outcomes remain compared.
        graph.remove("leaf_node_id");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_release_replay_preserves_the_boundary_across_host_poll_orders() {
        use crate::scheduler::BoundaryEvent;
        use crate::trace::{AbstractSessionView, AbstractWorldView, OracleVerdict};
        use serde_json::json;

        let event = BoundaryEvent::new(
            "session-006:provider:014:provider-event:001:sse",
            "session-006",
            BoundaryKind::ProviderEvent,
            902,
            "provider.event.sse",
            json!({
                "turn_boundary_id": "session-006:provider:014",
                "exchange_index": 13,
                "event_index": 1,
                "event_name": "sse",
                "provider_kind": "openai",
            }),
        );
        // The boundary from full run 36872412476, independent of ModelStore.
        let observed = json!({
            "session": "session-006",
            "provider_event_release": true,
            "turn_boundary_id": "session-006:provider:014",
            "exchange_index": 13,
            "event_index": 1,
            "event_name": "sse",
            "provider_kind": "openai",
            "active_turn_pending_before_release": true,
            "released_while_turn_pending": true,
            "scripted_transport_release": {
                "exchange_index": 13,
                "event_index": 1,
                "event_name": "sse",
                "at": 902,
                "blocked_before_release": false,
            },
        });
        let delivered = BoundaryScheduler::with_events(1, [event.clone()])
            .deliver_boundary(&event.boundary_id, observed)
            .expect("scheduled release");
        let summary = AbstractWorldView::with_digest(
            1,
            1,
            vec![AbstractSessionView {
                alias: "session-006".to_string(),
                opened: false,
                ingress_count: 0,
                provider_turns: Vec::new(),
                tool_outputs: Vec::new(),
                exec_code_outputs: Vec::new(),
                observer_turn_indices: Vec::new(),
                observer_reconnects: 0,
                queued_ingress_count: 0,
                cancellation_count: 0,
                trigger_count: 0,
                backend_failure_count: 0,
                provider_mutation_count: 0,
                durable_effect_keys: Vec::new(),
                checkpoint_commit_count: 0,
                checkpoint_component_stored_count: 0,
                checkpoint_component_ref_count: 0,
                checkpoint_head_revision: 0,
            }],
            Vec::new(),
        );
        let mut trace = SimulationTrace::new(
            1,
            "provider-release-regression",
            "provider-release-regression",
            "1/1",
            "provider-release",
            "provider-release",
            "provider-release",
            Default::default(),
            Default::default(),
            vec![delivered],
            Vec::new(),
            OracleVerdict::passed(
                crate::oracles::REPLAY_DETERMINISM_ORACLE,
                "captured release",
            ),
            Vec::new(),
            summary.clone(),
        );
        let path = Path::new("provider-release-regression.json");
        for blocked in [false, true] {
            trace.events[0].observed["scripted_transport_release"]["blocked_before_release"] =
                json!(blocked);
            let report = replay_trace(path, &trace).expect("host poll order must not diverge");
            assert_eq!(report.final_summary, summary);
            assert_eq!(
                report.delivered_boundary_sequence,
                vec![event.boundary_id.clone()]
            );
            assert_eq!(
                trace.events[0].observed["scripted_transport_release"]["blocked_before_release"],
                json!(blocked),
                "the trace must retain its actual gate observation"
            );
        }

        for (pointer, value) in [
            ("/session", json!("another-session")),
            ("/provider_event_release", json!(false)),
            ("/turn_boundary_id", json!("another-turn")),
            ("/exchange_index", json!(14)),
            ("/event_index", json!(2)),
            ("/event_name", json!("end")),
            ("/provider_kind", json!("another-provider")),
            ("/active_turn_pending_before_release", json!(false)),
            ("/released_while_turn_pending", json!(false)),
            ("/scripted_transport_release/exchange_index", json!(14)),
            ("/scripted_transport_release/event_index", json!(2)),
            ("/scripted_transport_release/event_name", json!("end")),
            ("/scripted_transport_release/at", json!(903)),
            (
                "/scripted_transport_release/blocked_before_release",
                json!("false"),
            ),
            (
                "/scripted_transport_release/blocked_before_release",
                Value::Null,
            ),
        ] {
            let mut tampered = trace.clone();
            *tampered.events[0]
                .observed
                .pointer_mut(pointer)
                .expect("field") = value;
            assert!(
                matches!(replay_trace(path, &tampered), Err(ReplayError::Divergence(message)) if message.contains("observed payload changed")),
                "changed release field {pointer} must diverge"
            );
        }
        let mut missing = trace;
        missing.events[0].observed["scripted_transport_release"]
            .as_object_mut()
            .expect("release object")
            .remove("blocked_before_release");
        assert!(matches!(
            replay_trace(path, &missing),
            Err(ReplayError::Divergence(message)) if message.contains("observed payload changed")
        ));
    }
}
