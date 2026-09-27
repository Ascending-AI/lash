//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on
//! PostgreSQL.

use super::{pg_law_stores, reset, storage};

lash_conformance::obligation_relay_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres obligation relay conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_lock, attachments),
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "postgres".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres recovery leader conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_lock, attachments),
        lash_conformance::LeaseLawFixture {
            store: stores.recovery_leader(),
            name: format!("recovery:{label}"),
        },
    )
});
