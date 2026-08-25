use std::collections::BTreeMap;

use serde_json::Value;

use crate::runtime_contracts::RuntimeUsageTotals;
use crate::store::CheckpointWriteEvent;
use crate::trace::OracleVerdict;

pub const RUNTIME_USAGE_CONSERVATION_ORACLE: &str = "sim.oracle.runtime-usage-conservation.v1";

type UsageKey = (String, String);

/// Conservation law over the checkpoint v3 usage-write stream. Submitted
/// contributions are folded independently by `(source, model)` and compared
/// with the accepted durable ledger after every commit and at the final state.
/// The current commit's summed contribution must also equal the accepted read
/// model's turn usage, so deleting a stream entry cannot pass as monotonic.
pub fn checkpoint_usage_conservation(writes: &[CheckpointWriteEvent]) -> OracleVerdict {
    match check_usage_conservation(writes) {
        Ok((sessions, commits, entries)) => OracleVerdict::passed(
            RUNTIME_USAGE_CONSERVATION_ORACLE,
            format!(
                "{entries} usage contributions conserved across {commits} commits in {sessions} sessions"
            ),
        ),
        Err(message) => OracleVerdict::failed(RUNTIME_USAGE_CONSERVATION_ORACLE, message),
    }
}

fn check_usage_conservation(
    writes: &[CheckpointWriteEvent],
) -> Result<(usize, usize, usize), String> {
    let mut submitted_by_session =
        BTreeMap::<String, BTreeMap<UsageKey, RuntimeUsageTotals>>::new();
    let mut final_accepted_by_session =
        BTreeMap::<String, BTreeMap<UsageKey, RuntimeUsageTotals>>::new();
    let mut checked_commits = 0usize;
    let mut checked_entries = 0usize;

    for write in writes
        .iter()
        .filter(|write| write.cause_boundary_id.is_none())
    {
        let Some(state) = &write.state else {
            // Promoted v1/v2 replay fixtures predate checkpoint state. Schema
            // v3 state presence is enforced by the independent checker.
            continue;
        };
        let session_id = write.attributed_session().to_string();
        let context = format!(
            "usage conservation `{session_id}` commit {}",
            write.commit_index
        );
        let submitted = fold_rows(&state.submitted_usage_rows, &context)?;
        checked_entries += state.submitted_usage_rows.as_array().map_or(0, Vec::len);
        let cumulative = submitted_by_session.entry(session_id.clone()).or_default();
        merge_usage(cumulative, &submitted);

        let accepted_raw = state
            .accepted_raw_rows
            .as_ref()
            .ok_or_else(|| format!("{context} has no accepted raw-row projection"))?;
        let accepted_ledger = fold_rows(
            accepted_raw.get("token_ledger").unwrap_or(&Value::Null),
            &format!("{context} accepted token ledger"),
        )?;
        require_equal(cumulative, &accepted_ledger, &context, "durable ledger")?;

        let accepted_read = state
            .accepted_read_model
            .as_ref()
            .ok_or_else(|| format!("{context} has no accepted read-model projection"))?;
        let submitted_turn_total = sum_by_key(&submitted);
        let read_turn_usage = accepted_read.get("token_usage").unwrap_or(&Value::Null);
        if submitted_turn_total.fields_value() != *read_turn_usage {
            return Err(format!(
                "{context} submitted usage diverged from read model: submitted={}; read={read_turn_usage}",
                submitted_turn_total.fields_value()
            ));
        }

        final_accepted_by_session.insert(session_id, accepted_ledger);
        checked_commits += 1;
    }

    for (session_id, submitted) in &submitted_by_session {
        let accepted = final_accepted_by_session.get(session_id).ok_or_else(|| {
            format!("usage conservation `{session_id}` has no final accepted ledger")
        })?;
        require_equal(submitted, accepted, session_id, "final durable ledger")?;
    }

    if checked_commits == 0 {
        return Err("usage conservation checked 0 checkpoint commits".to_string());
    }
    if checked_entries == 0 {
        return Err(format!(
            "usage conservation folded 0 contributions across {checked_commits} checkpoint commits"
        ));
    }

    Ok((submitted_by_session.len(), checked_commits, checked_entries))
}

fn fold_rows(
    rows: &Value,
    context: &str,
) -> Result<BTreeMap<UsageKey, RuntimeUsageTotals>, String> {
    let rows = rows
        .as_array()
        .ok_or_else(|| format!("{context} rows are not an array"))?;
    let mut by_key = BTreeMap::<UsageKey, RuntimeUsageTotals>::new();
    for row in rows {
        let source = row
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{context} usage row has no source"))?;
        let model = row
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{context} usage row has no model"))?;
        let usage = row.get("usage").unwrap_or(&Value::Null);
        let contribution = RuntimeUsageTotals::from_value(usage)
            .map_err(|field| format!("{context} usage row has no integer `{field}`"))?;
        by_key
            .entry((source.to_string(), model.to_string()))
            .or_default()
            .saturating_add_assign(&contribution);
    }
    Ok(by_key)
}

fn merge_usage(
    target: &mut BTreeMap<UsageKey, RuntimeUsageTotals>,
    contribution: &BTreeMap<UsageKey, RuntimeUsageTotals>,
) {
    for (key, totals) in contribution {
        target
            .entry(key.clone())
            .or_default()
            .saturating_add_assign(totals);
    }
}

fn sum_by_key(usage: &BTreeMap<UsageKey, RuntimeUsageTotals>) -> RuntimeUsageTotals {
    let mut total = RuntimeUsageTotals::default();
    for contribution in usage.values() {
        total.saturating_add_assign(contribution);
    }
    total
}

fn require_equal(
    submitted: &BTreeMap<UsageKey, RuntimeUsageTotals>,
    accepted: &BTreeMap<UsageKey, RuntimeUsageTotals>,
    context: &str,
    projection: &str,
) -> Result<(), String> {
    if submitted == accepted {
        return Ok(());
    }
    Err(format!(
        "usage conservation `{context}` diverged from {projection}: submitted={submitted:?}; accepted={accepted:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;

    #[tokio::test]
    async fn standard_generated_run_has_nonzero_usage_evidence_and_vacuous_inputs_fail() {
        let empty = checkpoint_usage_conservation(&[]);
        assert!(!empty.is_passed());
        assert!(empty.message.contains("checked 0 checkpoint commits"));

        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let (sessions, commits, contributions) =
            check_usage_conservation(&trace.durable_writes).expect("standard generated evidence");
        assert!(sessions > 0);
        assert!(commits > 0);
        assert!(contributions > 0);

        for write in &mut trace.durable_writes {
            write.state = None;
        }
        let stripped = checkpoint_usage_conservation(&trace.durable_writes);
        assert!(!stripped.is_passed());
        assert!(stripped.message.contains("checked 0 checkpoint commits"));
    }

    #[tokio::test]
    async fn seeded_dropped_usage_entry_mutation_fails_conservation_oracle() {
        let workload = generate_workload(5, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let baseline = checkpoint_usage_conservation(&trace.durable_writes);
        assert!(
            baseline.is_passed(),
            "unmutated generated usage must conserve: {}",
            baseline.message
        );
        let submitted = trace
            .durable_writes
            .iter_mut()
            .filter_map(|write| write.state.as_mut())
            .filter_map(|state| state.submitted_usage_rows.as_array_mut())
            .find(|rows| !rows.is_empty())
            .expect("seed 5 records a checkpoint usage entry");

        submitted.pop();

        let verdict = checkpoint_usage_conservation(&trace.durable_writes);
        assert!(!verdict.is_passed(), "dropped usage entry must be red");
        assert!(verdict.message.contains("diverged from durable ledger"));
    }
}
