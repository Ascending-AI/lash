//! Durable store-recovery laws over fresh persistence handles.

use super::*;

/// How store-recovery conformance executes the predecessor lease to expiry.
#[derive(Clone)]
pub enum StoreRecoveryLeaseTiming {
    /// Let a realtime backend's authoritative clock advance explicitly.
    Realtime,
    /// Advance the injected embedded-backend clock by the exact semantic TTL.
    Controlled(std::sync::Arc<dyn Fn(u64) + Send + Sync>),
}

impl StoreRecoveryLeaseTiming {
    pub fn controlled(advance: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self::Controlled(std::sync::Arc::new(advance))
    }
}

/// The backend's maker hands every recovery law its own persistence handle
/// rather than one shared instance.
pub async fn store_recovery_fresh_instances<F>(make: &F, label: &str)
where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
{
    let first = make(label);
    let second = make(label);
    assert_fresh_instances(&first, &second, "store_recovery");
}
