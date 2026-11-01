//! Call lookup agreement across the existing crash matrices.
use lash_core_execution::call_store::{self, CallRead};
use lash_core_store::tool_run::AttemptOutcome;
use lash_durable::domain::tool_calls::ToolCallSettlement;
use lash_durable::domain::{CallOwner, ToolCallDecision};
use lash_protocol_types::{ToolCallId, ToolCallOutcome};

pub async fn check(
    reads: &dyn lash_durable::DurableReads,
    owner: &CallOwner,
    expected: &[(ToolCallId, Option<AttemptOutcome>)],
) -> Vec<String> {
    let checked = async {
        let ids = expected.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let mut selected = ids.to_vec();
        selected.extend(ids.first().cloned());
        selected.push(ToolCallId::fixture("directory-missing-call"));
        let batch = call_store::calls(reads, owner, &selected).await.map_err(|e| e.to_string())?;
        let mut listed = Vec::new();
        let mut after = None;
        loop {
            let page = call_store::list(reads, owner, after, 2).await.map_err(|e| e.to_string())?;
            let done = page.len() < 2;
            after = page.last().map(|(cursor, _)| cursor.clone());
            listed.extend(page);
            if done { break; }
        }
        if listed.iter().map(|(cursor, _)| &cursor.call).collect::<Vec<_>>() != ids.iter().collect::<Vec<_>>() {
            return Err(format!("the directory lost admission order: {listed:?}, expected {ids:?}"));
        }
        for (id, outcome) in expected {
            let answer = call_store::call(reads, owner, id).await.map_err(|e| e.to_string())?;
            let valid = match (outcome, &answer) {
                (Some(expected), CallRead::Settled { outcome: ToolCallSettlement::Execution(actual), record: None }) => expected == actual.as_ref(),
                // A materialized answer lives in its canonical body; the row keeps only the winner.
                (Some(expected), CallRead::Settled { outcome: ToolCallSettlement::Materialized(decision), record: Some(record) }) => {
                    let cancelled = matches!(expected, AttemptOutcome::Cancelled { .. });
                    *decision == if cancelled { ToolCallDecision::Cancel } else { ToolCallDecision::Final }
                        && match expected {
                            AttemptOutcome::Completed(_) => matches!(record.output.outcome, ToolCallOutcome::Success(_)),
                            AttemptOutcome::Cancelled { .. } => matches!(record.output.outcome, ToolCallOutcome::Cancelled(_)),
                            _ => matches!(record.output.outcome, ToolCallOutcome::Failure(_)),
                        }
                }
                (None, CallRead::Pending { .. }) => true,
                _ => false,
            };
            if !valid { return Err(format!("directory settlement differs from committed replay for {id}: {answer:?}, expected {outcome:?}")); }
        }
        for (id, answer) in selected.iter().zip(batch) {
            let scalar = call_store::call(reads, owner, id).await.map_err(|e| e.to_string())?;
            let listed = listed.iter().find(|(cursor, _)| &cursor.call == id).map_or(CallRead::Unknown, |(_, answer)| answer.clone());
            if scalar != answer || scalar != listed {
                return Err(format!("scalar, batch and paged list disagree on {id}: {scalar:?}, {answer:?}, {listed:?}"));
            }
        }
        Ok::<(), String>(())
    }.await;
    checked.err().into_iter().collect()
}
