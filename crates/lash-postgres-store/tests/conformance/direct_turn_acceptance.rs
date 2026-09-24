//! ADR 0069 direct-turn ingress laws on PostgreSQL.
//!
//! A direct turn is one durable acceptance followed by a drive, so PostgreSQL
//! owes the same acceptance and recovery laws as every other backend.

use super::law_backend;

lash_conformance::direct_turn_acceptance_tests!({
    let Some((guard, backend)) = law_backend().await else {
        eprintln!(
            "skipping Postgres direct-turn acceptance conformance: database is not configured"
        );
        return;
    };
    (guard, "postgres", backend)
});
