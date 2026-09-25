//! Raw lineage diagnostics for backend fixtures.
pub use lash_core_store::testing::lineage::{GraphFactObservation, LineageConformanceInjector};

pub type LineageConformanceHandles =
    lash_core_store::testing::lineage::LineageConformanceHandles<dyn crate::SessionStoreFactory>;
