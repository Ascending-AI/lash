//! FIG-3531 cancelled-turn withheld-input laws on SQLite.
//!
//! A cancelled turn settles input it withheld from its terminal checkpoint
//! through the cancellation's undelivered disposition, so SQLite owes the same
//! law as every other backend.

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::cancelled_turn_withheld_input_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.dyn_backend();
    (backend, "sqlite", law_backend)
});
