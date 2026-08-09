use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::store::CheckpointWriteEvent;
use crate::trace::OracleVerdict;

#[derive(Default)]
struct CheckedSession {
    nodes: BTreeMap<String, Value>,
    leaf_node_id: Option<String>,
    usage_rows: Vec<Value>,
    current_turn_usage: Value,
    checked_commits: usize,
}

/// Jepsen-style checker over the checkpoint-write history. This module does not
/// call `ModelStore`, `SessionGraph::read_model`, or a backend API: it folds the
/// serialized commit events into its own graph, active transcript, and token
/// ledger, then compares that independent reconstruction with both the accepted
/// raw rows and the accepted read-model projection captured at the commit seam.
pub fn checkpoint_state_consistency(
    events: &[DeliveredBoundary],
    writes: &[CheckpointWriteEvent],
) -> OracleVerdict {
    match check_checkpoint_state(events, writes) {
        Ok((sessions, commits)) => OracleVerdict::passed(
            "sim.oracle.independent-checkpoint-state.v1",
            format!(
                "independent checkpoint checker matched raw rows, read models, and runtime facts across {commits} commits in {sessions} sessions"
            ),
        ),
        Err(message) => {
            OracleVerdict::failed("sim.oracle.independent-checkpoint-state.v1", message)
        }
    }
}

fn check_checkpoint_state(
    events: &[DeliveredBoundary],
    writes: &[CheckpointWriteEvent],
) -> Result<(usize, usize), String> {
    let mut sessions = BTreeMap::<String, CheckedSession>::new();
    for write in writes
        .iter()
        .filter(|write| write.cause_boundary_id.is_none())
    {
        let Some(state) = &write.state else {
            // Promoted v1/v2 replay fixtures predate checker state. Their normal
            // replay remains supported; newly generated v3 events are checked.
            continue;
        };
        let session_id = write.attributed_session().to_string();
        let checked = sessions.entry(session_id.clone()).or_default();
        fold_graph_append(checked, &state.submitted_graph_append, &session_id)?;
        fold_usage_rows(checked, &state.submitted_usage_rows, &session_id)?;
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
            &session_id,
        )?;

        let accepted_read = state.accepted_read_model.as_ref().ok_or_else(|| {
            format!(
                "checkpoint checker `{session_id}` commit {} has no accepted read-model projection",
                write.commit_index
            )
        })?;
        compare_read_model(checked, accepted_read, &session_id)?;
    }

    for (session_id, checked) in &sessions {
        let Some(runtime) = events
            .iter()
            .rev()
            .find(|event| event.kind == BoundaryKind::Provider && event.actor_alias == *session_id)
        else {
            continue;
        };
        compare_runtime_facts(checked, runtime, session_id)?;
    }

    let checked_commits = sessions
        .values()
        .map(|session| session.checked_commits)
        .sum();
    Ok((sessions.len(), checked_commits))
}

fn fold_graph_append(
    checked: &mut CheckedSession,
    append: &Value,
    session_id: &str,
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

fn fold_usage_rows(
    checked: &mut CheckedSession,
    rows: &Value,
    session_id: &str,
) -> Result<(), String> {
    let rows = rows
        .as_array()
        .ok_or_else(|| format!("checkpoint checker `{session_id}` usage rows are not an array"))?;
    checked.current_turn_usage = sum_usage(rows, false);
    for row in rows {
        let source = row.get("source");
        let model = row.get("model");
        if let Some(existing) = checked
            .usage_rows
            .iter_mut()
            .find(|existing| existing.get("source") == source && existing.get("model") == model)
        {
            for field in USAGE_FIELDS {
                let total = existing
                    .pointer(&format!("/usage/{field}"))
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    .saturating_add(
                        row.pointer(&format!("/usage/{field}"))
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                    );
                existing["usage"][*field] = json!(total);
            }
        } else {
            checked.usage_rows.push(row.clone());
        }
    }
    Ok(())
}

fn compare_raw_rows(
    checked: &CheckedSession,
    raw: &Value,
    submitted_turn_state: &Value,
    session_id: &str,
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
    if raw.get("token_ledger").and_then(Value::as_array) != Some(&checked.usage_rows) {
        return Err(format!(
            "checkpoint checker `{session_id}` usage reconstruction diverged from accepted raw rows: checker={}; raw={}",
            Value::Array(checked.usage_rows.clone()),
            raw.get("token_ledger").unwrap_or(&Value::Null)
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
    checked: &CheckedSession,
    read: &Value,
    session_id: &str,
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
    let usage = &checked.current_turn_usage;
    if read.get("token_usage") != Some(usage) {
        return Err(format!(
            "checkpoint checker `{session_id}` usage reconstruction diverged from read model: checker={usage}; read={}",
            read.get("token_usage").unwrap_or(&Value::Null)
        ));
    }
    Ok(())
}

fn compare_runtime_facts(
    checked: &CheckedSession,
    runtime: &DeliveredBoundary,
    session_id: &str,
) -> Result<(), String> {
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
    let usage = with_total(&checked.current_turn_usage);
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
    let ledger_usage = sum_usage(&checked.usage_rows, true);
    if runtime
        .observed
        .pointer("/runtime_invariant_facts/usage/token_ledger_total")
        != Some(&ledger_usage)
    {
        return Err(format!(
            "checkpoint checker `{session_id}` cumulative usage reconstruction diverged from runtime ledger facts: checker={ledger_usage}; runtime={}",
            runtime
                .observed
                .pointer("/runtime_invariant_facts/usage/token_ledger_total")
                .unwrap_or(&Value::Null)
        ));
    }
    Ok(())
}

fn reconstruct_active_transcript(
    checked: &CheckedSession,
    session_id: &str,
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

fn rows_by_id(rows: &[Value], session_id: &str) -> Result<BTreeMap<String, Value>, String> {
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

fn sum_usage(rows: &[Value], include_total: bool) -> Value {
    let mut totals = Map::new();
    let mut total = 0i64;
    for field in USAGE_FIELDS {
        let value = rows
            .iter()
            .filter_map(|row| {
                row.pointer(&format!("/usage/{field}"))
                    .and_then(Value::as_i64)
            })
            .sum::<i64>();
        if *field != "reasoning_output_tokens" {
            total += value;
        }
        totals.insert((*field).to_string(), json!(value));
    }
    if include_total {
        totals.insert("total_tokens".to_string(), json!(total));
    }
    Value::Object(totals)
}

fn with_total(usage: &Value) -> Value {
    let mut usage = usage.as_object().cloned().unwrap_or_default();
    let total = USAGE_FIELDS
        .iter()
        .filter(|field| **field != "reasoning_output_tokens")
        .filter_map(|field| usage.get(*field).and_then(Value::as_i64))
        .sum::<i64>();
    usage.insert("total_tokens".to_string(), json!(total));
    Value::Object(usage)
}

const USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_write_input_tokens",
    "reasoning_output_tokens",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[test]
    fn independent_usage_fold_detects_a_corrupted_runtime_fact() {
        let checked = CheckedSession {
            usage_rows: vec![json!({"usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "reasoning_output_tokens": 0
            }})],
            current_turn_usage: json!({
                "input_tokens": 5,
                "output_tokens": 2,
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "reasoning_output_tokens": 0
            }),
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

        let error = compare_runtime_facts(&checked, &runtime, "session-001")
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

        let verdict = checkpoint_state_consistency(&trace.events, &trace.durable_writes);
        assert!(!verdict.is_passed(), "corrupted runtime usage must be red");
        assert!(verdict.message.contains("usage reconstruction diverged"));
    }
}
