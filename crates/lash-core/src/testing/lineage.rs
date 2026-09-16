//! Raw lineage diagnostics for backend fixtures.
pub use lash_core_store::testing::lineage::{GraphFactObservation, LineageConformanceInjector};

pub type LineageConformanceHandles =
    lash_core_store::testing::lineage::LineageConformanceHandles<dyn crate::SessionStoreFactory>;

pub(crate) fn handles_from_concrete<F>(
    handles: lash_core_store::testing::lineage::LineageConformanceHandles<F>,
) -> LineageConformanceHandles
where
    F: crate::SessionStoreFactory + 'static,
{
    LineageConformanceHandles {
        factory: handles.factory,
        injector: handles.injector,
    }
}
