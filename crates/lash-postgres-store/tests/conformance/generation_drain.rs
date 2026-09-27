//! The build-generation drain laws (FIG-3799, FIG-3884) on PostgreSQL.

use super::{pg_law_stores, reset, storage};

lash_conformance::generation_drain_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres generation drain conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_lock, attachments),
        lash_conformance::GenerationDrainLawFixture {
            stores,
            prefix: "postgres".to_owned(),
        },
    )
});
