//! Counts actual round-fact decoder entries in one activation, including
//! acknowledged facts delivered without a store reread.
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

tokio::task_local! {
    static DECODES: FactDecodes;
}

/// A law's counter, scoped to its activation future rather than the process.
#[derive(Clone, Default)]
pub struct FactDecodes(Arc<AtomicU64>);

impl FactDecodes {
    pub async fn within<T>(&self, future: impl Future<Output = T>) -> T {
        DECODES.scope(self.clone(), future).await
    }

    pub fn count(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

pub(crate) fn entered() {
    let _ = DECODES.try_with(|counter| counter.0.fetch_add(1, Ordering::Relaxed));
}
