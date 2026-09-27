//! The build-generation drain laws (FIG-3799, FIG-3884) on SQLite.

use std::sync::Arc;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::generation_drain_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let stores: Arc<dyn lash_core_execution::StoreSet> = Arc::new((*backend).clone());
    (
        backend,
        lash_conformance::GenerationDrainLawFixture {
            stores,
            prefix: format!("sqlite-{SUBSTRATE:?}"),
        },
    )
});
