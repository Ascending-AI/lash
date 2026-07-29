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
        // Worker lease fencing is produced by the REAL session-execution lease
        // store at generation time (and re-verified by the SQLite/Postgres backend
        // replays). The abstract ModelStore cannot re-derive it, so the model
        // carries the REAL recorded reclaim/fence facts rather than fabricating
        // them: thread the recorded observation in directly instead of projecting.
        let observed = if matches!(
            event.kind,
            BoundaryKind::Worker | BoundaryKind::ProcessLifecycle
        ) {
            store.apply_observed_boundary(&event, &expected.observed);
            expected.observed.clone()
        } else {
            store.apply_boundary(&event)
        };
        let delivered = scheduler
            .deliver_boundary(&expected.boundary_id, observed)
            .ok_or_else(|| ReplayError::MissingBoundary(expected.boundary_id.clone()))?;
        let actual_observed = normalize(&delivered.observed);
        let expected_observed = normalize(&expected.observed);
        if actual_observed != expected_observed {
            return Err(ReplayError::Divergence(format!(
                "boundary `{}` observed payload changed; expected={}; actual={}",
                expected.boundary_id, expected_observed, actual_observed
            )));
        }
        sequence.push(delivered.boundary_id);
    }

    if !scheduler.is_empty() {
        return Err(ReplayError::Divergence(format!(
            "{} boundaries remained pending after replay",
            scheduler.pending_len()
        )));
    }

    store.apply_durable_writes(&trace.durable_writes);
    let final_summary = store.summary();
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
            let graph: RuntimeGraphInvariantFacts =
                serde_json::from_value(graph.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable graph invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = graph.duplicate_node_ids.is_empty()
                && graph.missing_parent_links.is_empty()
                && graph.cycle_node_ids.is_empty()
                && graph.leaf_exists;
            require_reverified(
                event,
                "graph",
                recomputed,
                graph.passed,
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
            let agent_frame: RuntimeAgentFrameInvariantFacts =
                serde_json::from_value(agent_frame.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable agent-frame invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = agent_frame.active_frame_ids.len() == 1
                && agent_frame.active_frame_ids.first() == Some(&agent_frame.current_frame_node_id)
                && agent_frame.current_frame_exists
                && agent_frame.current_frame_active
                && agent_frame.nodes_without_agent_frame.is_empty()
                && agent_frame.node_agent_frame_ids_without_record.is_empty();
            require_reverified(
                event,
                "agent_frame",
                recomputed,
                agent_frame.passed,
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
            let usage: RuntimeUsageInvariantFacts =
                serde_json::from_value(usage.clone()).map_err(|err| {
                    ReplayError::Divergence(format!(
                        "boundary `{}` recorded an unreadable usage invariant fact: {err}",
                        event.boundary_id
                    ))
                })?;
            let recomputed = usage.negative_fields.is_empty()
                && usage.non_negative
                && usage.usage_events_monotonic;
            require_reverified(
                event,
                "usage",
                recomputed,
                usage.passed,
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

fn normalize(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("provider_parser_matrix");
        object.remove("runtime_effect");
        object.remove("runtime_effect_outcome");
        object.remove("runtime_tool_record");
        object.remove("runtime_queued_work");
        object.remove("runtime_worker_store");
        object.remove("runtime_process_lifecycle");
        object.remove("runtime_active_lease");
        object.remove("runtime_stale_completion");
        object.remove("runtime_lease_probe");
        object.remove("runtime_suspend");
        object.remove("runtime_invariant_facts");
        object.remove("runtime_final_value_facts");
        object.remove("sim_clock");
        // The cancel outcome (`cancelled`/`cancel_outcome`) depends on whether
        // the real runtime had already consumed the targeted input by the time
        // the cancellation arrived — a fact the abstract ModelStore cannot
        // reconstruct from the boundary stream. Coverage is preserved because
        // the cancellation oracle keys off `final_summary.cancellation_count`,
        // which the model still tracks, not this per-boundary string.
        object.remove("cancel_outcome");
        object.remove("cancelled");
        // Provider-event release evidence is liveness/timing dependent: a gate
        // can be released while the turn is parked, or skipped as a no-op once
        // the turn has already finished. The abstract model cannot reconstruct
        // which, so these fields are excluded from cross-backend equality.
        object.remove("scripted_transport_release");
        object.remove("active_turn_pending_before_release");
        object.remove("released_while_turn_pending");
        object.remove("provider_event_release_noop_turn_finished");
        if let Some(runtime_invariants) = object
            .get_mut("runtime_invariants")
            .and_then(Value::as_object_mut)
        {
            runtime_invariants.remove("graph_acyclic");
            runtime_invariants.remove("single_active_agent_frame");
            runtime_invariants.remove("usage_monotonic");
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[tokio::test]
    async fn replay_reproduces_boundary_sequence_and_summary() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let report = replay_trace(Path::new("trace.json"), &trace).expect("replay");

        assert_eq!(report.delivered_event_count, trace.events.len());
        assert_eq!(report.final_summary, trace.final_summary);
        assert!(report.terminal_verdict.is_passed());
        let reverification = &report.runtime_invariant_reverification;
        assert!(
            reverification.reverified_turn_count > 0,
            "replay must re-verify at least one runtime turn's invariant facts"
        );
        assert_eq!(
            reverification.graph_invariant_checks,
            reverification.reverified_turn_count
        );
        assert_eq!(
            reverification.agent_frame_invariant_checks,
            reverification.reverified_turn_count
        );
        assert_eq!(
            reverification.usage_invariant_checks,
            reverification.reverified_turn_count
        );
    }

    #[tokio::test]
    async fn replay_reverification_rejects_tampered_runtime_invariant_facts() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        // Corrupt a recorded runtime invariant fact so the structural re-derivation
        // contradicts the stored `passed` flag; replay must surface it as a
        // runtime-level divergence even though the abstract summary still matches.
        let tampered = trace
            .events
            .iter_mut()
            .find(|event| {
                event.kind == BoundaryKind::Provider
                    && event.observed.get("runtime_invariant_facts").is_some()
            })
            .expect("a provider turn with recorded invariant facts");
        tampered.observed["runtime_invariant_facts"]["graph"]["cycle_node_ids"] =
            serde_json::json!(["node-a", "node-b"]);
        let err = replay_trace(Path::new("trace.json"), &trace)
            .expect_err("tampered runtime invariant facts must diverge");
        assert!(
            matches!(err, ReplayError::Divergence(message) if message.contains("graph")),
            "expected a graph invariant divergence"
        );
    }

    #[test]
    fn promoted_queued_active_turn_cancel_regression_replays() {
        let trace_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("replays/queued-active-turn-cancel-race/trace.json");
        let trace = read_trace(&trace_path).expect("read promoted replay fixture");
        let report = replay_trace(&trace_path, &trace).expect("replay promoted fixture");

        assert!(report.terminal_verdict.is_passed());
        assert_eq!(report.delivered_event_count, trace.events.len());
        assert!(trace.oracles.iter().any(|verdict| {
            verdict.oracle_id
                == "sim.oracle.scenario-mini.runtime.queued-input-hidden-while-live.v1"
                && verdict.is_passed()
        }));
        assert!(trace.oracles.iter().any(|verdict| {
            verdict.oracle_id
                == "sim.oracle.scenario-mini.runtime.cancellation-prevents-idle-claim.v1"
                && verdict.is_passed()
        }));
        assert!(
            report.final_summary.sessions.iter().any(|session| {
                session.queued_ingress_count > 0 && session.cancellation_count > 0
            })
        );
    }

    // A full-random fixture (seed 14123330213291275571) that pins the active-turn
    // queued-input cancel + subsequent-turn shape. This shape once *appeared* to
    // diverge cross-backend, but that was a replay-FIDELITY gap in the harness's
    // old gated, fixed-exchange-count serial SQLite re-drive — NOT a product bug:
    // driven directly through one driver, the in-memory and SQLite stores commit
    // identical output (proved by the backend-equivalence test
    // `tests/cross_backend_active_turn_divergence.rs`), and the cross-backend lane
    // now re-runs the workload through the single unified driver. This guard keeps
    // the trace model-replayable and pins the shape so it stays exercised.
    #[test]
    fn cross_backend_active_turn_fixture_model_replays_and_pins_shape() {
        let trace_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("replays/cross-backend-sqlite-active-turn-divergence/trace.json");
        let trace = read_trace(&trace_path).expect("read active-turn fixture");
        assert_eq!(trace.seed, 14_123_330_213_291_275_571);
        assert_eq!(trace.profile, "full-random");

        // Model replay must pass: the trace and the abstract model are sound; the
        // former cross-backend mismatch lived only in the old gated re-drive.
        let report = replay_trace(&trace_path, &trace).expect("model-replay active-turn fixture");
        assert!(report.terminal_verdict.is_passed());
        assert_eq!(report.delivered_event_count, trace.events.len());

        // Pin the shape: a session that takes an active-turn queued-input cancel
        // and then runs subsequent provider turns.
        assert!(
            report.final_summary.sessions.iter().any(|session| {
                session.queued_ingress_count > 0
                    && session.cancellation_count > 0
                    && session.provider_outputs.len() >= 2
            }),
            "fixture must retain an active-turn cancel followed by later turns"
        );
    }
}
