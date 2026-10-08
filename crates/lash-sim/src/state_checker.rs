use lash_sansio::SessionId;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::runtime_contracts::RuntimeUsageTotals;
#[cfg(test)]
use crate::scheduler::BoundaryKind;
use crate::scheduler::DeliveredBoundary;
use crate::store::CheckpointWriteEvent;
use crate::trace::{OracleVerdict, WorkloadExpectations};

#[derive(Default)]
struct CheckedSession {
    nodes: BTreeMap<String, Value>,
    leaf_node_id: Option<String>,
    /// The last accepted read model's turn usage: what the committed state
    /// reports for context-window tracking. Hosts meter spend at the
    /// Provider seam (ADR 0127).
    current_turn_usage: RuntimeUsageTotals,
    checked_commits: usize,
}

/// Jepsen-style checker over the checkpoint-write history. This module does not
/// call `ModelStore`, `SessionGraph::read_model`, or a backend API: it folds the
/// serialized commit events into its own graph and active transcript, then
/// compares that independent reconstruction with both the accepted
/// raw rows and the accepted read-model projection captured at the commit seam.
/// Verdict id of the independent checkpoint-state checker.
pub const INDEPENDENT_CHECKPOINT_STATE_ORACLE: crate::trace::OracleId<'static> =
    crate::trace::OracleId::real("sim.oracle.independent-checkpoint-state.v1");

pub fn checkpoint_state_consistency(
    events: &[DeliveredBoundary],
    writes: &[CheckpointWriteEvent],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    match check_checkpoint_state(events, writes, expectations) {
        Ok((sessions, commits, runtime_facts)) => OracleVerdict::passed(
            INDEPENDENT_CHECKPOINT_STATE_ORACLE,
            format!(
                "independent checkpoint checker matched raw rows, read models, and {runtime_facts} runtime-facts observations across {commits} commits in {sessions} sessions (workload declared {} session(s))",
                expectations.session_count()
            ),
        ),
        Err(message) => OracleVerdict::failed(INDEPENDENT_CHECKPOINT_STATE_ORACLE, message),
    }
}

fn check_checkpoint_state(
    events: &[DeliveredBoundary],
    writes: &[CheckpointWriteEvent],
    expectations: &WorkloadExpectations,
) -> Result<(usize, usize, usize), String> {
    let mut sessions = BTreeMap::<String, CheckedSession>::new();
    for write in writes.iter().filter(|write| write.attribution.is_none()) {
        let Some(state) = &write.state else {
            return Err(format!(
                "checkpoint checker `{}` commit {} carries no checker state",
                write.attributed_session(),
                write.commit_index
            ));
        };
        let session_id = write.attributed_session().to_string();
        let attributed_session = SessionId::fixture(session_id.clone());
        let checked = sessions.entry(session_id.clone()).or_default();
        fold_graph_append(checked, &state.submitted_graph_append, &attributed_session)?;
        checked.checked_commits += 1;

        let accepted_raw = state.accepted_raw_rows.as_ref().ok_or_else(|| {
            format!(
                "checkpoint checker `{session_id}` commit {} has no accepted raw-row projection",
                write.commit_index
            )
        })?;
        compare_raw_rows(
            checked,
            accepted_raw,
            &state.submitted_turn_state,
            &attributed_session,
        )?;

        let accepted_read = state.accepted_read_model.as_ref().ok_or_else(|| {
            format!(
                "checkpoint checker `{session_id}` commit {} has no accepted read-model projection",
                write.commit_index
            )
        })?;
        compare_read_model(checked, accepted_read, &SessionId::fixture(session_id))?;
    }

    // Every session the workload declared must reach the checker, checked by
    // identity rather than by count. Zero committing sessions used to read as
    // "consistent across 0 commits in 0 sessions" — a total loss of checkpoint
    // coverage indistinguishable from compliance. A cardinality floor would not
    // fix that here: this population is strictly wider than the declared one
    // (it also reconstructs suspend- and worker-attributed commits), so
    // `reconstructed >= declared` still passes a run in which every declared
    // session lost its checkpoints and the undeclared attributions made up the
    // difference.
    let missing = expectations.sessions_missing_from(sessions.keys().map(String::as_str));
    if !missing.is_empty() {
        return Err(format!(
            "workload declared {} session(s) but the independent checkpoint checker reconstructed no commits for {:?} (reconstructed {:?}); the declared observation class is absent or incomplete",
            expectations.session_count(),
            missing,
            sessions.keys().collect::<Vec<_>>()
        ));
    }

    let mut runtime_facts_checked = 0usize;
    for (session_id, checked) in &sessions {
        let Some(runtime) = events.iter().rev().find(|event| {
            event.actor_alias == *session_id
                && event.observed.get("runtime_invariant_facts").is_some()
        }) else {
            return Err(format!(
                "checkpoint checker `{session_id}` checked {} commits but found no matching runtime-facts observation",
                checked.checked_commits
            ));
        };
        compare_runtime_facts(checked, runtime, &SessionId::fixture(session_id))?;
        runtime_facts_checked += 1;
    }

    let checked_commits = sessions
        .values()
        .map(|session| session.checked_commits)
        .sum();
    if checked_commits == 0 {
        return Err(
            "independent checkpoint checker checked 0 commits; generated lane is vacuous"
                .to_string(),
        );
    }
    Ok((sessions.len(), checked_commits, runtime_facts_checked))
}

fn fold_graph_append(
    checked: &mut CheckedSession,
    append: &Value,
    session_id: &SessionId,
) -> Result<(), String> {
    let nodes = append
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("checkpoint checker `{session_id}` graph append has no node rows")
        })?;
    for node in nodes {
        let node_id = node
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("checkpoint checker `{session_id}` found a node without id"))?;
        if checked
            .nodes
            .insert(node_id.to_string(), node.clone())
            .is_some()
        {
            return Err(format!(
                "checkpoint checker `{session_id}` saw duplicate graph row `{node_id}`"
            ));
        }
    }
    if let Some(leaf) = append.get("leaf_node_id").and_then(Value::as_str) {
        checked.leaf_node_id = Some(leaf.to_string());
    }
    Ok(())
}

fn compare_raw_rows(
    checked: &CheckedSession,
    raw: &Value,
    submitted_turn_state: &Value,
    session_id: &SessionId,
) -> Result<(), String> {
    let raw_nodes = raw
        .get("graph_nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("checkpoint checker `{session_id}` raw graph rows are missing"))?;
    let raw_by_id = rows_by_id(raw_nodes, session_id)?;
    if checked.nodes != raw_by_id {
        return Err(format!(
            "checkpoint checker `{session_id}` graph reconstruction diverged from accepted raw rows"
        ));
    }
    if checked.leaf_node_id.as_deref() != raw.get("graph_leaf_node_id").and_then(Value::as_str) {
        return Err(format!(
            "checkpoint checker `{session_id}` graph leaf diverged from accepted raw rows"
        ));
    }
    if raw.get("turn_state") != Some(submitted_turn_state) {
        return Err(format!(
            "checkpoint checker `{session_id}` submitted turn state diverged from accepted raw rows"
        ));
    }
    Ok(())
}

fn compare_read_model(
    checked: &mut CheckedSession,
    read: &Value,
    session_id: &SessionId,
) -> Result<(), String> {
    let graph_count = read
        .get("graph_node_count")
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize;
    if graph_count != checked.nodes.len() {
        return Err(format!(
            "checkpoint checker `{session_id}` graph has {} nodes but read model reports {graph_count}",
            checked.nodes.len()
        ));
    }
    let transcript = reconstruct_active_transcript(checked, session_id)?;
    if read.get("messages").and_then(Value::as_array) != Some(&transcript) {
        return Err(format!(
            "checkpoint checker `{session_id}` transcript reconstruction diverged from read model"
        ));
    }
    checked.current_turn_usage = RuntimeUsageTotals::from_value(
        read.get("token_usage").unwrap_or(&Value::Null),
    )
    .map_err(|field| {
        format!("checkpoint checker `{session_id}` read-model usage has no integer `{field}`")
    })?;
    Ok(())
}

fn compare_runtime_facts(
    checked: &CheckedSession,
    runtime: &DeliveredBoundary,
    session_id: &SessionId,
) -> Result<(), String> {
    if runtime
        .observed
        .pointer("/runtime_invariant_facts/graph/leaf_node_id")
        .and_then(Value::as_str)
        != checked.leaf_node_id.as_deref()
    {
        return Err(format!(
            "checkpoint checker `{session_id}` store leaf {:?} diverged from runtime-facts leaf {:?}",
            checked.leaf_node_id,
            runtime
                .observed
                .pointer("/runtime_invariant_facts/graph/leaf_node_id")
        ));
    }
    if runtime
        .observed
        .get("graph_node_count")
        .and_then(Value::as_u64)
        != Some(checked.nodes.len() as u64)
    {
        return Err(format!(
            "checkpoint checker `{session_id}` graph reconstruction diverged from runtime read facts"
        ));
    }
    let transcript = reconstruct_active_transcript(checked, session_id)?;
    if runtime
        .observed
        .get("transcript_message_count")
        .and_then(Value::as_u64)
        != Some(transcript.len() as u64)
    {
        return Err(format!(
            "checkpoint checker `{session_id}` transcript reconstruction diverged from runtime read facts"
        ));
    }
    let usage = json!(checked.current_turn_usage);
    if runtime
        .observed
        .pointer("/runtime_invariant_facts/usage/total_usage")
        != Some(&usage)
    {
        return Err(format!(
            "checkpoint checker `{session_id}` usage reconstruction diverged from runtime facts: checker={usage}; runtime={}",
            runtime
                .observed
                .pointer("/runtime_invariant_facts/usage/total_usage")
                .unwrap_or(&Value::Null)
        ));
    }
    Ok(())
}

fn reconstruct_active_transcript(
    checked: &CheckedSession,
    session_id: &SessionId,
) -> Result<Vec<Value>, String> {
    let mut path = Vec::new();
    let mut cursor = checked.leaf_node_id.as_deref();
    while let Some(node_id) = cursor {
        let node = checked.nodes.get(node_id).ok_or_else(|| {
            format!("checkpoint checker `{session_id}` leaf path misses graph row `{node_id}`")
        })?;
        path.push(node);
        cursor = node.get("parent_node_id").and_then(Value::as_str);
        if path.len() > checked.nodes.len() {
            return Err(format!(
                "checkpoint checker `{session_id}` detected a graph cycle"
            ));
        }
    }
    path.reverse();
    Ok(path
        .into_iter()
        .filter_map(|node| node.pointer("/event/Conversation").cloned())
        .collect())
}

fn rows_by_id(rows: &[Value], session_id: &SessionId) -> Result<BTreeMap<String, Value>, String> {
    rows.iter()
        .map(|row| {
            let id = row
                .get("node_id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("checkpoint checker `{session_id}` raw row has no id"))?;
            Ok((id.to_string(), row.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[test]
    fn independent_usage_fold_detects_a_corrupted_runtime_fact() {
        let checked = CheckedSession {
            current_turn_usage: RuntimeUsageTotals::new(5, 2, 0, 0, 0),
            ..CheckedSession::default()
        };
        let runtime = DeliveredBoundary {
            schema: "test".to_string(),
            sequence: 1,
            scheduler: Default::default(),
            boundary_id: "provider".to_string(),
            actor_alias: "session-001".to_string(),
            kind: BoundaryKind::Provider,
            at: 1,
            label: "provider".to_string(),
            payload: Value::Null,
            observed: json!({
                "graph_node_count": 0,
                "transcript_message_count": 0,
                "runtime_invariant_facts": {"usage": {"total_usage": {
                    "input_tokens": 999,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 7
                }}}
            }),
        };

        let error = compare_runtime_facts(&checked, &runtime, &SessionId::from("session-001"))
            .expect_err("corrupted runtime usage must fail the checker");
        assert!(error.contains("usage reconstruction diverged"));
    }

    #[tokio::test]
    async fn seeded_checker_rejects_corrupted_runtime_usage() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let runtime = trace
            .events
            .iter_mut()
            .rev()
            .find(|event| event.kind == BoundaryKind::Provider)
            .expect("seed 5 includes a provider turn");
        runtime.observed["runtime_invariant_facts"]["usage"]["total_usage"]["input_tokens"] =
            json!(999);

        let verdict =
            checkpoint_state_consistency(&trace.events, &trace.durable_writes, &trace.expectations);
        assert!(!verdict.is_passed(), "corrupted runtime usage must be red");
        assert!(verdict.message.contains("usage reconstruction diverged"));
    }

    fn declared_sessions(aliases: &[&str]) -> WorkloadExpectations {
        WorkloadExpectations::new(
            aliases.iter().map(|alias| (*alias).to_string()).collect(),
            20,
            2,
        )
    }

    #[test]
    fn independent_checker_rejects_a_declared_workload_that_committed_nothing() {
        // Red-side proof: the workload declared five sessions and the checker
        // reconstructed none. Before the declaration rode the trace this read
        // as "consistent across 0 commits in 0 sessions".
        let declared = declared_sessions(&[
            "session-001",
            "session-002",
            "session-003",
            "session-004",
            "session-005",
        ]);
        let verdict = checkpoint_state_consistency(&[], &[], &declared);
        assert!(!verdict.is_passed(), "an absent commit class must be red");
        assert!(
            verdict.message.contains("workload declared 5 session(s)")
                && verdict.message.contains("session-001"),
            "{}",
            verdict.message
        );
    }

    /// Red-side proof for the identity floor. The checker's reconstructed
    /// population is strictly wider than the declared one — a real
    /// `default-random` run also reconstructs suspend- and worker-attributed
    /// commits — so a cardinality floor (`reconstructed >= declared`) still
    /// passes a run in which a declared session lost every checkpoint and the
    /// undeclared attributions covered the count. Dropping one declared
    /// session's commits must be red, and must name that session.
    #[tokio::test]
    async fn checker_names_a_declared_session_whose_checkpoints_all_vanished() {
        let workload = generate_workload(5, "default-random", 96).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let baseline =
            checkpoint_state_consistency(&trace.events, &trace.durable_writes, &trace.expectations);
        assert!(baseline.is_passed(), "{}", baseline.message);

        let dropped = trace
            .expectations
            .sessions
            .first()
            .expect("declared session")
            .clone();
        let reconstructed_before = trace
            .durable_writes
            .iter()
            .filter(|write| write.attribution.is_none() && write.state.is_some())
            .map(|write| write.attributed_session().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            reconstructed_before.len() > trace.expectations.session_count(),
            "this proof needs the reconstructed population to be wider than the declared one, got {reconstructed_before:?} for {} declared",
            trace.expectations.session_count()
        );

        let mut without_declared_session = trace.durable_writes.clone();
        without_declared_session.retain(|write| write.attributed_session() != dropped);
        // A cardinality floor would not fire here: the surviving undeclared
        // attributions still outnumber the declared sessions.
        let surviving = without_declared_session
            .iter()
            .filter(|write| write.attribution.is_none() && write.state.is_some())
            .map(|write| write.attributed_session().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            surviving.len() >= trace.expectations.session_count(),
            "a count-only floor must still be satisfied for this proof to mean anything: {surviving:?}"
        );

        let verdict = checkpoint_state_consistency(
            &trace.events,
            &without_declared_session,
            &trace.expectations,
        );
        assert!(
            !verdict.is_passed(),
            "a declared session losing every checkpoint must be red: {}",
            verdict.message
        );
        assert!(
            verdict.message.contains(&dropped),
            "the verdict must name the missing declared session `{dropped}`: {}",
            verdict.message
        );
    }

    #[test]
    fn independent_checker_rejects_zero_commits() {
        let verdict = checkpoint_state_consistency(&[], &[], &WorkloadExpectations::default());
        assert!(!verdict.is_passed(), "zero checked commits must be red");
        assert!(verdict.message.contains("checked 0 commits"));
    }

    #[tokio::test]
    async fn standard_generated_run_checks_commits_and_requires_their_state() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let baseline =
            checkpoint_state_consistency(&trace.events, &trace.durable_writes, &trace.expectations);
        assert!(baseline.is_passed(), "{}", baseline.message);
        assert!(!baseline.message.contains("across 0 commits"));
        assert!(
            baseline.message.contains(&format!(
                "workload declared {} session(s)",
                trace.expectations.session_count()
            )),
            "{}",
            baseline.message
        );

        let runtime_write = trace
            .durable_writes
            .iter_mut()
            .find(|write| {
                write.schema == crate::store::CHECKPOINT_WRITE_EVENT_SCHEMA
                    && write.attribution.is_none()
            })
            .expect("generated runtime write");
        runtime_write.state = None;
        let verdict =
            checkpoint_state_consistency(&trace.events, &trace.durable_writes, &trace.expectations);
        assert!(
            !verdict.is_passed(),
            "a commit without checker state must be red"
        );
        assert!(verdict.message.contains("carries no checker state"));
    }

    #[tokio::test]
    async fn checker_rejects_commit_session_without_runtime_facts_and_leaf_mismatch() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let session = trace
            .durable_writes
            .iter()
            .find(|write| write.attribution.is_none() && write.state.is_some())
            .expect("runtime write")
            .attributed_session()
            .to_string();

        let mut missing = trace.events.clone();
        missing
            .retain(|event| event.kind != BoundaryKind::Provider || event.actor_alias != session);
        let verdict =
            checkpoint_state_consistency(&missing, &trace.durable_writes, &trace.expectations);
        assert!(!verdict.is_passed(), "missing runtime facts must be red");
        assert!(verdict.message.contains("no matching runtime-facts"));

        let mut wrong_leaf = trace.events.clone();
        let runtime = wrong_leaf
            .iter_mut()
            .rev()
            .find(|event| event.kind == BoundaryKind::Provider && event.actor_alias == session)
            .expect("matching provider facts");
        runtime.observed["runtime_invariant_facts"]["graph"]["leaf_node_id"] =
            json!("mutated-leaf");
        let verdict =
            checkpoint_state_consistency(&wrong_leaf, &trace.durable_writes, &trace.expectations);
        assert!(
            !verdict.is_passed(),
            "runtime/store leaf mismatch must be red"
        );
        assert!(verdict.message.contains("runtime-facts leaf"));
    }
}
