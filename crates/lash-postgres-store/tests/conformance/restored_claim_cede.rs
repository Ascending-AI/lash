//! FIG-3552 restored-claim cede laws on PostgreSQL.
//!
//! A redrive whose journal-restored checkpoint claim another driver answered
//! cedes instead of committing the same words again, so PostgreSQL owes the
//! same law as every other backend.

use super::law_backend;

lash_conformance::restored_claim_cede_tests!({
    let Some((guard, backend)) = law_backend().await else {
        eprintln!("skipping Postgres restored-claim cede conformance: database is not configured");
        return;
    };
    (guard, "postgres", backend)
});
