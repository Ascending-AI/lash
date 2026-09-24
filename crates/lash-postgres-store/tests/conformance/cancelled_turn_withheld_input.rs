//! FIG-3531 cancelled-turn withheld-input laws on PostgreSQL.
//!
//! A cancelled turn settles input it withheld from its terminal checkpoint
//! through the cancellation's undelivered disposition, so PostgreSQL owes the
//! same law as every other backend.

use super::law_backend;

lash_conformance::cancelled_turn_withheld_input_tests!({
    let Some((guard, backend)) = law_backend().await else {
        eprintln!(
            "skipping Postgres cancelled-turn withheld-input conformance: database is not configured"
        );
        return;
    };
    (guard, "postgres", backend)
});
