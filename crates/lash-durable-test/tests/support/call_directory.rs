//! Call lookup agreement across the existing crash matrices.
use lash_core_execution::call_store::{self, CallRead};
use lash_core_execution::runtime::actor::round::{CompletedCall, graph::ArchivalCall};
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::{CallOwner, ToolCallAnswer, ToolCallDecision};
use lash_protocol_types::{ToolCallId, ToolCallRecord};

/// Whether `record`, read from the directory's canonical body, holds the
/// answer `expected` names. The fold reads `expected` from the run facts and
/// checks its payload against the material digest, so it is the oracle.
fn holds(expected: &SettledOutput, record: &ToolCallRecord) -> bool {
    if let Some(ArchivalCall(archived)) = expected.typed::<ArchivalCall>() {
        return archived == record;
    }
    expected.typed::<CompletedCall>().is_some_and(|completed| {
        completed.call_id == record.call_id
            && completed.provider_call_id == record.provider_call_id
            && completed.tool_name == record.tool
            && completed.args == record.args
            && completed.output == record.output
    })
}

pub async fn check(
    reads: &dyn lash_durable::DurableReads,
    owner: &CallOwner,
    expected: &[(ToolCallId, Option<SettledOutput>)],
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
                (Some(expected), CallRead::Settled(ToolCallAnswer::Execution(actual))) => expected.outcome() == *actual.as_ref(),
                // A materialized answer lives in its canonical body; the row keeps only the winner.
                // Only an output naming material has a body, so its winner is Final.
                (Some(expected), CallRead::Settled(ToolCallAnswer::Materialized { decision, body: record })) => {
                    *decision == ToolCallDecision::Final && holds(expected, record)
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
