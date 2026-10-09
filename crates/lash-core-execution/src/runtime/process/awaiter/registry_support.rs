use super::*;
use crate::ProcessId;

impl WatchedProcessRegistry {
    /// A commit grew `process_id`'s log: tick this node's change hub, and
    /// the other nodes' through the node hints, once named.
    pub(super) fn appended(&self, process_id: &ProcessId) {
        self.hub.notify(process_id);
        if let Some(hints) = self.publication.hints.get()
            && let Ok(actor) = lash_durable::ActorKey::process(process_id.as_str())
        {
            hints.appended(actor);
        }
    }

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
                Ok(row) => {
                    self.feed_read_recovered(process_id, "durable_cursor");
                    row.map(|row| row.published_event_sequence)
                }
                Err(error) => {
                    match &error {
                        lash_durable::DurableError::Store(failure) => self.feed_read_failed(
                            process_id,
                            "durable_cursor",
                            failure.kind,
                            None,
                            &error,
                        ),
                        _ => self.feed_read_failed(
                            process_id,
                            "durable_cursor",
                            "DurableError",
                            None,
                            &error,
                        ),
                    }
                    None
                }
            };
        }
        // The record's high-water mark, not the newest retained event: a
        // host release can leave no event to read the position from.
        match self.inner.get_process(process_id).await {
            Ok(record) => {
                self.feed_read_recovered(process_id, "process_cursor");
                record.map(|record| record.last_event_sequence)
            }
            Err(error) => {
                self.feed_plugin_read_failed(process_id, "process_cursor", &error);
                None
            }
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
            let read = self
                .inner
                .event_page_after(
                    process_id,
                    after_sequence,
                    limit,
                    crate::ProcessEventQueryMode::Full,
                )
                .await;
            let page = match read {
                Ok(outcome) => {
                    self.feed_read_recovered(process_id, "event_page");
                    match outcome {
                        crate::ProcessEventReadOutcome::Retained(page) => page,
                        _ => break,
                    }
                }
                Err(error) => {
                    self.feed_plugin_read_failed(process_id, "event_page", &error);
                    break;
                }
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

    fn feed_plugin_read_failed(
        &self,
        process_id: &ProcessId,
        operation: &'static str,
        error: &crate::PluginError,
    ) {
        let failure = crate::ToolIntentCommandFailure::from(error);
        self.feed_read_failed(
            process_id,
            operation,
            failure.failure_class(),
            Some(&failure.code()),
            error,
        );
    }

    fn feed_read_failed(
        &self,
        process_id: &ProcessId,
        operation: &'static str,
        error_type: impl std::fmt::Debug,
        error_code: Option<&str>,
        error: &dyn std::fmt::Display,
    ) {
        let mut failures = self.publication.read_failures.lock_recover();
        let count = failures.entry((process_id.clone(), operation)).or_default();
        *count = count.saturating_add(1);
        if *count == 1 {
            tracing::warn!(event = "process_feed.degraded", %process_id, operation,
                ?error_type, error_code, %error, failure_count = *count,
                "process event feed read failed; publication remains best effort");
        }
    }

    fn feed_read_recovered(&self, process_id: &ProcessId, operation: &'static str) {
        if let Some(failure_count) = self
            .publication
            .read_failures
            .lock_recover()
            .remove(&(process_id.clone(), operation))
        {
            tracing::info!(event = "process_feed.recovered", %process_id, operation,
                failure_count, "process event feed read recovered");
        }
    }
}
