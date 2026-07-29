use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

#[derive(Clone, Default)]
pub struct ProcessChangeHub {
    inner: Arc<Mutex<HashMap<String, watch::Sender<u64>>>>,
}

impl ProcessChangeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe before reading a process row. The receiver carries only a
    /// version counter; waiters always re-read the registry after a bump.
    pub fn subscribe(&self, process_id: &str) -> watch::Receiver<u64> {
        let mut guard = self.inner.lock().expect("process change hub lock poisoned");
        guard
            .entry(process_id.to_string())
            .or_insert_with(|| {
                let (tx, _rx) = watch::channel(0);
                tx
            })
            .subscribe()
    }

    pub fn notify(&self, process_id: &str) {
        let mut guard = self.inner.lock().expect("process change hub lock poisoned");
        let mut remove = false;
        if let Some(tx) = guard.get(process_id) {
            if tx.receiver_count() == 0 {
                remove = true;
            } else {
                let next = (*tx.borrow()).wrapping_add(1);
                if tx.send(next).is_err() {
                    remove = true;
                }
            }
        }
        if remove {
            guard.remove(process_id);
        }
    }

    #[cfg(test)]
    pub(super) fn tracked_processes(&self) -> usize {
        self.inner
            .lock()
            .expect("process change hub lock poisoned")
            .len()
    }
}
