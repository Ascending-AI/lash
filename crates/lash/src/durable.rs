//! The durable substrate's backend (ADR 0132 §1). I0 (FIG-5194) pins this
//! builder's full shape, and L3 (FIG-5172) makes a built backend serve.

use std::sync::Arc;

use lash_core::{Backend, StoreSet};

/// Builds the one [`Backend`] a [`LashCore`](crate::LashCore) takes: lash's
/// own durable engine over one store set.
pub struct DurableBackendBuilder {
    stores: Arc<dyn StoreSet>,
}

impl DurableBackendBuilder {
    /// A builder of the durable backend over `stores`.
    pub fn new(stores: Arc<dyn StoreSet>) -> Self {
        Self { stores }
    }

    /// The durable backend over this builder's store set.
    ///
    /// # Errors
    /// [`DurableBuildError`] when the backend cannot be assembled.
    pub fn build(self) -> Result<Backend, DurableBuildError> {
        let Self { stores: _stores } = self;
        todo!("I0 (FIG-5194): assemble the durable Backend over its store set")
    }
}

/// Why [`DurableBackendBuilder::build`] refused. I0 (FIG-5194) replaces it
/// with the pinned enum.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DurableBuildError {
    message: String,
}
