//! Keyed archival call reads. Execution replay and presentation use their own facts.

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::GraphDecoder;
use lash_durable::domain::tool_calls::ToolCallSettlement;
use lash_durable::domain::{
    CallOwner, ToolCallCursor, ToolCallDecision, ToolCallSelection, ToolCalls,
};
use lash_durable::{DurableError, DurableReads, StoreFailure, StoreFailureKind};

/// A durable answer is visible only after its publishing commit.
#[derive(Clone, Debug, PartialEq)]
pub enum CallRead {
    Unknown,
    Pending {
        decision: Option<ToolCallDecision>,
    },
    Settled {
        record: Option<Box<crate::ToolCallRecord>>,
        outcome: ToolCallSettlement,
    },
}

fn corrupt(message: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Corrupt,
        message: message.to_owned(),
    })
}

fn decode(selected: &ToolCalls) -> Result<Vec<CallRead>, DurableError> {
    let mut decoder = GraphDecoder::new(&selected.graph, selected.graph.read_limits);
    selected
        .rows
        .iter()
        .map(|row| {
            let Some(answer) = &row.answer else {
                return Ok(CallRead::Pending {
                    decision: row.decision,
                });
            };
            if !answer.valid() {
                return Err(corrupt("a call settlement and its body disagree"));
            }
            let record = answer
                .body
                .map(|body| {
                    let record =
                        crate::runtime::actor::round::graph::CallBody::load(&mut decoder, body)?
                            .record;
                    if record.call_id != row.call {
                        return Err(corrupt("call row and archival body identities differ"));
                    }
                    Ok(Box::new(record))
                })
                .transpose()?;
            if answer.outcome == ToolCallSettlement::Read
                && record.as_ref().is_none_or(|record| {
                    !matches!(
                        record.classification,
                        lash_protocol_types::ToolCallClassification::UnrecordedRead { .. }
                    )
                })
            {
                return Err(corrupt("a saved Read has no attributed archival body"));
            }
            Ok(CallRead::Settled {
                record,
                outcome: answer.outcome.clone(),
            })
        })
        .collect()
}

/// Read selected calls in input order, preserving duplicates and misses.
pub async fn calls(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    ids: &[crate::ToolCallId],
) -> Result<Vec<CallRead>, DurableError> {
    let selected = reads
        .tool_calls(owner, &ToolCallSelection::Calls(ids.to_vec()))
        .await?;
    let answered = decode(&selected)?;
    let by_id: BTreeMap<_, _> = selected
        .rows
        .iter()
        .map(|row| &row.call)
        .zip(answered)
        .collect();
    Ok(ids
        .iter()
        .map(|id| by_id.get(id).cloned().unwrap_or(CallRead::Unknown))
        .collect())
}

/// One indexed lookup, with no journal or caller-state reads.
pub async fn call(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    id: &crate::ToolCallId,
) -> Result<CallRead, DurableError> {
    Ok(calls(reads, owner, std::slice::from_ref(id))
        .await?
        .into_iter()
        .next()
        .unwrap_or(CallRead::Unknown))
}

/// One indexed page, retaining each row's cursor for the next page.
pub async fn list(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    after: Option<ToolCallCursor>,
    limit: u32,
) -> Result<Vec<(ToolCallCursor, CallRead)>, DurableError> {
    page(reads, owner, ToolCallSelection::List { after, limit }).await
}

/// One indexed page of caller-saved Reads, without hydrating recorded calls.
pub async fn list_reads(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    after: Option<ToolCallCursor>,
    limit: u32,
) -> Result<Vec<(ToolCallCursor, CallRead)>, DurableError> {
    page(reads, owner, ToolCallSelection::SavedReads { after, limit }).await
}

async fn page(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    selection: ToolCallSelection,
) -> Result<Vec<(ToolCallCursor, CallRead)>, DurableError> {
    let selected = reads.tool_calls(owner, &selection).await?;
    let answered = decode(&selected)?;
    Ok(selected
        .rows
        .into_iter()
        .zip(answered)
        .map(|(row, answer)| {
            (
                ToolCallCursor {
                    sequence: row.sequence,
                    call: row.call,
                },
                answer,
            )
        })
        .collect())
}

/// Every read `execution` saved under the caller `owner`, in admission
/// order, each with its archival node: what an in-turn run that starts
/// again is answered instead of reading again (FIG-7585).
///
/// # Errors
///
/// A store failure, or a saved read whose body does not decode.
pub async fn execution_reads(
    reads: &dyn DurableReads,
    owner: &CallOwner,
    execution: &lash_durable::domain::OwnerKey,
) -> Result<
    Vec<(
        lash_core_store::checkpoint_graph::NodeId,
        lash_protocol_types::ReadFact,
    )>,
    DurableError,
> {
    const PAGE: u32 = 256;
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let selected = reads
            .tool_calls(
                owner,
                &ToolCallSelection::SavedReads {
                    after: after.clone(),
                    limit: PAGE,
                },
            )
            .await?;
        let Some(last) = selected.rows.last() else {
            return Ok(out);
        };
        after = Some(ToolCallCursor {
            sequence: last.sequence,
            call: last.call.clone(),
        });
        let mut decoder = GraphDecoder::new(&selected.graph, selected.graph.read_limits);
        for row in &selected.rows {
            if &row.execution != execution {
                continue;
            }
            let Some(body) = row.answer.as_ref().and_then(|answer| answer.body) else {
                return Err(corrupt("a saved Read has no attributed archival body"));
            };
            let fact = crate::runtime::actor::round::graph::load_read_fact(&mut decoder, body)
                .map_err(DurableError::from)?;
            out.push((body, fact));
        }
        if selected.rows.len() < PAGE as usize {
            return Ok(out);
        }
    }
}
