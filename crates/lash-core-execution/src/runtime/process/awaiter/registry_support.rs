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
        self.inner
            .recent_events(process_id, 1)
            .await
            .ok()
            .map(|events| {
                events
                    .into_iter()
                    .map(|event| event.sequence)
                    .max()
                    .unwrap_or(0)
            })
    }

    pub(super) async fn emit_event_pages_since(&self, process_id: &ProcessId, cursor: Option<u64>) {
        let sinks = self.sinks.lock_recover().clone();
        let Some(cursor) = cursor else {
            return;
        };
        if sinks.is_empty() {
            return;
        }
        let Ok(process_ref) = self.inner.resolve_process_ref(process_id).await else {
            return;
        };
        let limit = std::num::NonZeroUsize::new(128).unwrap_or(std::num::NonZeroUsize::MIN);
        let mut after_sequence = cursor;
        loop {
            let Ok(crate::ProcessEventReadOutcome::Retained(page)) = self
                .inner
                .event_page_ref(
                    &process_ref,
                    after_sequence,
                    limit,
                    crate::ProcessEventQueryMode::Full,
                )
                .await
            else {
                return;
            };
            let crate::ProcessEventPageEvents::Full(events) = page.events else {
                return;
            };
            for event in events {
                for sink in &sinks {
                    sink.emit(&event).await;
                }
            }
            after_sequence = match page.more {
                crate::ProcessEventPageMore::Complete => return,
                crate::ProcessEventPageMore::More { after_sequence } => after_sequence,
            };
        }
    }
}
