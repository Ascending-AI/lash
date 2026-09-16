use std::sync::Arc;

use lash_core_store::turn_control_binding::StoreTurnCancellationAuthority;

use crate::core_internal::AwaitEventRegistry;

#[derive(Clone)]
pub struct NativeAwaitEventAuthority {
    binding_id: String,
    registry: Arc<AwaitEventRegistry>,
}

impl NativeAwaitEventAuthority {
    pub fn new(binding_id: impl Into<String>) -> Self {
        Self {
            binding_id: binding_id.into(),
            registry: Arc::new(AwaitEventRegistry::new()),
        }
    }

    pub fn registry(&self) -> Arc<AwaitEventRegistry> {
        Arc::clone(&self.registry)
    }
}

impl StoreTurnCancellationAuthority for NativeAwaitEventAuthority {
    fn binding_id(&self) -> &str {
        &self.binding_id
    }
}
