//! ADR 0069 direct-turn ingress laws on SQLite.
//!
//! A direct turn is one durable acceptance followed by a drive, so SQLite owes
//! the same acceptance and recovery laws as every other backend.

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::direct_turn_acceptance_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.dyn_backend();
    (backend, "sqlite", law_backend)
});
