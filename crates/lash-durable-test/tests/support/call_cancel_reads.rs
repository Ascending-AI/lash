//! Read tools keep the same admitted-call cancellation law.
use super::*;

impl CancelCase {
    fn reading(mut self) -> Self {
        self.read = true;
        self
    }
}

cancellation_cases! {
    read_host_cancel_on_postgres => host(CommitOrder::Cancel, None, true).reading();
    read_host_final_on_postgres => host(CommitOrder::PlainFinal, None, true).reading();
    read_python_cancel_on_postgres => CancelCase::python(CommitOrder::Cancel, None, true).reading();
    read_python_final_on_postgres => CancelCase::python(CommitOrder::PlainFinal, None, true).reading();
    read_typescript_cancel_on_postgres => CancelCase::typescript(CommitOrder::Cancel, None, true).reading();
    read_typescript_final_on_postgres => CancelCase::typescript(CommitOrder::PlainFinal, None, true).reading();
    read_control_on_postgres => CancelCase::typescript(CommitOrder::Cancel, None, false).reading();
}

pub(super) async fn run_law() {
    run_cases(Dialect::SqliteMemory, None, &cases()).await;
}

#[async_trait::async_trait]
impl lash_core::ToolReads for ExtTools {
    async fn read(&self, call: lash_core::ReadCall<'_>) -> Result<Value, lash_core::ReadError> {
        let nodes = self.world.nodes.lock_recover().upgrade().unwrap();
        assert!(
            nodes
                .script()
                .trace()
                .iter()
                .filter(|write| write.committed())
                .flat_map(|write| &write.facts.rows)
                .any(|row| row.call.as_ref() == Some(call.call_id)
                    && row.kind == lash_durable::domain::RunFactKind::XStart),
            "a cancellable Read body must have a durable admitted call and started attempt"
        );
        self.world
            .entries
            .lock_recover()
            .entry(call.call_id.clone())
            .or_default()
            .push((call.manifest.id.as_str().to_owned(), call.args.clone()));
        let case = self.world.cancel_case.lock_recover().unwrap();
        let x = written_x(call.args).unwrap();
        if case.order.final_wins() {
            if x != 1 {
                loop {
                    let changed = self.world.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if self
                        .world
                        .fast_returned
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        break;
                    }
                    changed.await;
                }
            }
            return Ok(json!({ "final": x }));
        }
        self.world.ready.lock_recover().insert(x);
        self.world.changed.notify_waiters();
        call.cancel.cancelled().await;
        self.world.signalled.lock_recover().push(x);
        self.world
            .cancelled
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The decision already won. A late successful reader cannot replace it.
        Ok(json!({ "late": x }))
    }
}

/// The transcript and the public call reader must retain the same committed
/// read fact, including its admitted identity, caller, provider and ordinal.
pub(super) async fn attribution(turn: &CellTurn) -> Vec<String> {
    let checked = async {
        let durable = turn
            .core()
            .session(session())
            .durable()
            .await
            .map_err(|e| e.to_string())?;
        let transcript = durable.transcript().await.map_err(|e| e.to_string())?;
        let entry = transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.item, lash::transcript::TranscriptItem::Cell(_)))
            .ok_or("the cell is retained")?;
        let lash::transcript::TranscriptItem::Cell(cell) = &entry.item else {
            unreachable!()
        };
        let turn_id = entry
            .provenance
            .turn_id
            .clone()
            .ok_or("the turn is retained")?;
        let calls = durable
            .read_turn_tool_calls(&turn_id)
            .await
            .map_err(|e| e.to_string())?;
        if cell.read_facts.len() != 2 {
            return Err(format!("two admitted Read facts: {:?}", cell.read_facts));
        }
        if cell
            .read_facts
            .iter()
            .map(|fact| fact.ordinal)
            .collect::<Vec<_>>()
            != [1, 2]
        {
            return Err(format!(
                "the two Read admissions lost their ordinal census: {:?}",
                cell.read_facts
            ));
        }
        for fact in &cell.read_facts {
            let records = calls
                .iter()
                .filter(|record| record.call_id == fact.call_id)
                .collect::<Vec<_>>();
            let [record] = records.as_slice() else {
                return Err(format!("one archival authority for {}", fact.call_id));
            };
            let read = record
                .read
                .as_ref()
                .ok_or("the admitted read lost its attribution")?;
            if **record != fact.record()
                || fact.execution != lash_core::ToolCallExecution::Recorded
                || read.ordinal != fact.ordinal
                || read.caller != fact.caller
                || read.provider != fact.provider
                || read.site != fact.site
                || read.origin != fact.origin
                || record.args != fact.args
                || record.output.outcome != fact.outcome
                || fact.site.is_empty()
                || fact.origin.is_empty()
            {
                return Err(format!(
                    "the saved read differs from its one call record: {fact:?}, {record:?}"
                ));
            }
            if !turn
                .world
                .entries()
                .get(&fact.call_id)
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|(tool, args)| tool == SLOW && *args == fact.args)
                })
            {
                return Err(format!(
                    "a Read fact disagrees with its external body entry: {fact:?}"
                ));
            }
        }
        Ok::<(), String>(())
    }
    .await;
    checked.err().into_iter().collect()
}
