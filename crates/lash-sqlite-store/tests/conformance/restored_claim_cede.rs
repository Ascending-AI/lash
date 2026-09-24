//! FIG-3552 restored-claim cede laws on SQLite.
//!
//! A redrive whose journal-restored checkpoint claim another driver answered
//! cedes instead of committing the same words again, so SQLite owes the same
//! law as every other backend.

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::restored_claim_cede_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.dyn_backend();
    (backend, "sqlite", law_backend)
});
