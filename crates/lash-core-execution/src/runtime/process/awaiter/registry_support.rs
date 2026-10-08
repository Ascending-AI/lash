use super::*;
use crate::ProcessId;

impl WatchedProcessRegistry {
    pub(super) fn event_path(&self, process_id: &ProcessId) -> Arc<tokio::sync::Mutex<()>> {
        let mut paths = self.publication.event_paths.lock_recover();
        paths.retain(|_, path| path.strong_count() > 0);
        if let Some(path) = paths.get(process_id).and_then(Weak::upgrade) {
            return path;
        }
        let path = Arc::new(tokio::sync::Mutex::new(()));
        paths.insert(process_id.clone(), Arc::downgrade(&path));
        path
    }

    /// Where an append's emission starts, read before the append under the
    /// process's event path: this node's mark; with none, the durable mark
    /// when an activation records it from this node's marks; otherwise the
    /// log as it stands, so the append's own events are emitted.
    pub(super) async fn sink_cursor(&self, process_id: &ProcessId) -> Option<u64> {
        if self.sinks.lock_recover().is_empty() {
            return None;
        }
        if let Some(mark) = self.publication.emitted.lock_recover().get(process_id) {
            return Some(*mark);
        }
        if let Some(durable) = self.publication.durable.get() {
            return match durable.process(process_id).await {
                Ok(Some(row)) => Some(row.published_event_sequence),
                Ok(None) | Err(_) => None,
            };
        }
        // The record's high-water mark, not the newest retained event: a
        // host release can leave no event to read the position from.
        match self.inner.get_process(process_id).await {
            Ok(Some(record)) => Some(record.last_event_sequence),
            Ok(None) | Err(_) => None,
        }
    }

    /// Emit to the sinks every event of `process_id`'s log after the last one
    /// any path emitted on this node, or after `cursor` when none did yet,
    /// and never at or before `floor`. Answers the last sequence read, or
    /// `None` when nothing was read: no cursor, no sink, or no page.
    ///
    /// The mark only moves forward, and an ended process keeps it: a
    /// publisher whose own cursor is older than what another path emitted
    /// meets the mark, never its cursor (`WatchedRegistry::forget_published`
    /// drops it once the durable mark covers it).
    pub(super) async fn emit_event_pages_since(
        &self,
        process_id: &ProcessId,
        cursor: Option<u64>,
        floor: u64,
    ) -> Option<u64> {
        let sinks = self.sinks.lock_recover().clone();
        let cursor = cursor?;
        if sinks.is_empty() {
            return None;
        }
        let emitted = self
            .publication
            .emitted
            .lock_recover()
            .get(process_id)
            .copied();
        let start = emitted.unwrap_or(cursor).max(floor);
        let mut after_sequence = start;
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
            }
            match page.more {
                crate::ProcessEventPageMore::Complete => break,
                crate::ProcessEventPageMore::More {
                    after_sequence: more,
                } => after_sequence = after_sequence.max(more),
            }
        }
        if emitted.is_some() || after_sequence > start {
            let mut marks = self.publication.emitted.lock_recover();
            let mark = marks.entry(process_id.clone()).or_insert(after_sequence);
            *mark = (*mark).max(after_sequence);
        }
        Some(after_sequence)
    }
}
