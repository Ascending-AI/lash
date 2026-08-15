use super::*;

impl WatchedProcessRegistry {
    pub(super) fn event_path(&self, process_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut paths = self.event_paths.lock_recover();
        paths.retain(|_, path| path.strong_count() > 0);
        if let Some(path) = paths.get(process_id).and_then(Weak::upgrade) {
            return path;
        }
        let path = Arc::new(tokio::sync::Mutex::new(()));
        paths.insert(process_id.to_string(), Arc::downgrade(&path));
        path
    }

    pub(super) async fn sink_cursor(&self, process_id: &str) -> Option<u64> {
        self.sink.as_ref()?;
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

    pub(super) async fn emit_events_after(&self, process_id: &str, cursor: Option<u64>) {
        let (Some(sink), Some(cursor)) = (self.sink.as_ref(), cursor) else {
            return;
        };
        let Ok(events) = self.inner.events_after(process_id, cursor).await else {
            return;
        };
        for event in events {
            sink.emit(&event).await;
        }
    }
}
