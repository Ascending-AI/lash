use super::*;
use crate::ProcessId;

impl WatchedProcessRegistry {
    pub(super) fn event_path(&self, process_id: &ProcessId) -> Arc<tokio::sync::Mutex<()>> {
        let mut paths = self.event_paths.lock_recover();
        paths.retain(|_, path| path.strong_count() > 0);
        if let Some(path) = paths.get(process_id).and_then(Weak::upgrade) {
            return path;
        }
        let path = Arc::new(tokio::sync::Mutex::new(()));
        paths.insert(process_id.clone(), Arc::downgrade(&path));
        path
    }

    pub(super) async fn sink_cursor(&self, process_id: &ProcessId) -> Option<u64> {
        if self.sinks.lock_recover().is_empty() {
            return None;
        }
        // The record's high-water mark, not the newest retained event: a
        // host release can leave no event to read the position from.
        match self.inner.get_process(process_id).await {
            Ok(Some(record)) => Some(record.last_event_sequence),
            Ok(None) | Err(_) => None,
        }
    }

    /// Emit to the sinks every event of `process_id`'s log after the last one
    /// any path emitted on this node, or after `cursor` when none did yet.
    /// Answers the last sequence read, or `None` when nothing was read: no
    /// cursor, no sink, or no page.
    pub(super) async fn emit_event_pages_since(
        &self,
        process_id: &ProcessId,
        cursor: Option<u64>,
    ) -> Option<u64> {
        let sinks = self.sinks.lock_recover().clone();
        let cursor = cursor?;
        if sinks.is_empty() {
            return None;
        }
        let emitted = self.emitted.lock_recover().get(process_id).copied();
        let mut after_sequence = emitted.unwrap_or(cursor);
        let mut terminal = false;
        let limit = std::num::NonZeroUsize::new(128).unwrap_or(std::num::NonZeroUsize::MIN);
        loop {
            let Ok(crate::ProcessEventReadOutcome::Retained(page)) = self
                .inner
                .event_page_after(
                    process_id,
                    after_sequence,
                    limit,
                    crate::ProcessEventQueryMode::Full,
                )
                .await
            else {
                break;
            };
            let crate::ProcessEventPageEvents::Full(events) = page.events else {
                break;
            };
            for event in events {
                for sink in &sinks {
                    sink.emit(&event).await;
                }
                after_sequence = after_sequence.max(event.sequence);
                terminal |= event.semantics.terminal.is_some();
            }
            match page.more {
                crate::ProcessEventPageMore::Complete => break,
                crate::ProcessEventPageMore::More {
                    after_sequence: more,
                } => after_sequence = after_sequence.max(more),
            }
        }
        // An ended process's mark is dropped with it: a later append's path
        // reads its own cursor, past everything emitted.
        let mut marks = self.emitted.lock_recover();
        if terminal {
            marks.remove(process_id);
        } else {
            marks.insert(process_id.clone(), after_sequence);
        }
        Some(after_sequence)
    }
}
