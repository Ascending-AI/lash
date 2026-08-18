use super::*;
use crate::{ProcessWorklistCursor, ProcessWorklistPage};

const CURSOR_BACKEND: &str = "in_memory";

pub(super) async fn list_non_terminal_page(
    registry: &TestLocalProcessRegistry,
    limit: std::num::NonZeroUsize,
    continuation: Option<ProcessWorklistCursor>,
) -> Result<ProcessWorklistPage, PluginError> {
    registry
        .worklist_page_reads
        .lock()
        .await
        .push((limit.get(), continuation.clone()));
    registry.pause_worklist_page().await;
    {
        let mut plan = registry.worklist_page_error_plan.lock().await;
        if let Some((successful_reads, errors)) = plan.as_mut() {
            if *successful_reads > 0 {
                *successful_reads -= 1;
            } else if let Some(error) = errors.pop_front() {
                if errors.is_empty() {
                    *plan = None;
                }
                return Err(error);
            }
        }
    }
    if let Some(cursor) = continuation.as_ref()
        && cursor.backend() != CURSOR_BACKEND
    {
        return Err(PluginError::ProcessWorklistCursorBackendMismatch {
            expected: CURSOR_BACKEND.to_string(),
            actual: cursor.backend().to_string(),
        });
    }
    let managed = registry.managed.lock().await;
    let through_process_id = match continuation.as_ref() {
        Some(cursor) => cursor.through_process_id().to_string(),
        None => match managed
            .values()
            .filter(|record| !record.record.status.is_retired())
            .map(|record| record.record.id.as_str())
            .max()
        {
            Some(process_id) => process_id.to_string(),
            None => {
                return Ok(ProcessWorklistPage {
                    records: Vec::new(),
                    continuation: None,
                });
            }
        },
    };
    let after_process_id = continuation
        .as_ref()
        .map(ProcessWorklistCursor::after_process_id);
    let mut records: Vec<ProcessRecord> = managed
        .values()
        .filter(|record| !record.record.status.is_retired())
        .filter(|record| record.record.id.as_str() <= through_process_id.as_str())
        .filter(|record| after_process_id.is_none_or(|after| record.record.id.as_str() > after))
        .map(|record| record.record.clone())
        .collect();
    records.sort_by(|a, b| a.id.cmp(&b.id));
    let has_more = records.len() > limit.get();
    records.truncate(limit.get());
    let continuation = has_more.then(|| {
        ProcessWorklistCursor::new(
            CURSOR_BACKEND,
            records.last().expect("non-empty bounded page").id.clone(),
            through_process_id,
        )
    });
    Ok(ProcessWorklistPage {
        records,
        continuation,
    })
}
