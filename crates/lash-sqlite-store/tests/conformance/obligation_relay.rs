//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on
//! SQLite.

use std::sync::Arc;

use lash_core_execution::StoreSet as _;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::obligation_relay_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let stores: Arc<dyn lash_core_execution::StoreSet> = Arc::new((*backend).clone());
    (
        backend,
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "sqlite".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.recovery_leader();
    (
        backend,
        lash_conformance::LeaseLawFixture {
            store,
            name: format!("recovery:{label}"),
        },
    )
});
