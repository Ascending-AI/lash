//! FIG-3552 restored-claim cede laws on PostgreSQL.
//!
//! A redrive whose journal-restored checkpoint claim another driver answered
//! cedes instead of committing the same words again, so PostgreSQL owes the
//! same law as every other backend.

use std::sync::Arc;

use lash_core::store::RuntimePersistence;

use super::{reset, storage};

lash_conformance::restored_claim_cede_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres restored-claim cede conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, backend) = super::pg_law_backend(&storage);
    (
        (database_lock, attachments),
        "postgres",
        backend,
        Arc::new(storage.session_store(lash_conformance::RESTORED_CLAIM_CEDE_SESSION_ID))
            as Arc<dyn RuntimePersistence>,
    )
});
