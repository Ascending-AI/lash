//! ADR 0069 direct-turn ingress laws on PostgreSQL.
//!
//! A direct turn is one durable acceptance followed by a drive, so PostgreSQL
//! owes the same acceptance and recovery laws as every other backend.

use std::sync::Arc;

use lash_core::store::RuntimePersistence;

use super::{reset, storage};

lash_conformance::direct_turn_acceptance_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres direct-turn acceptance conformance: database is not configured"
        );
        return;
    };
    reset(&storage).await;
    (
        database_lock,
        "postgres",
        Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
    )
});
