//! Component 118 (FIG-3544): every pending turn input carries an immutable
//! submitted ingress and submission digest written once at admission, and
//! source-key replay compares only the digest.
//!
//! A component-117 catalog holds rows with neither column, and the digest is a
//! Rust-computed value no DDL can backfill, so the boundary is destructive: the
//! whole store is refused at open rather than replayed against a missing
//! digest.

use lash_core_execution::{StoreError, compat::CompatRefusal};
use lash_postgres_store::PostgresStorage;

use crate::support::{IsolatedSchema, database_url};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_refuses_pre_submission_digest_catalog_at_open() {
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres pre-submission-digest open gate: database is not configured");
        return;
    };
    let scratch = IsolatedSchema::provision(&url).await;
    let pool = scratch.pool.clone();

    // The old shape is unsafe even if a later compatible release stamps it as
    // an expansion that this build should otherwise be able to read.
    sqlx::query(
        "ALTER TABLE lash_pending_turn_inputs
             DROP COLUMN submitted_ingress_json,
             DROP COLUMN submission_digest",
    )
    .execute(&pool)
    .await
    .expect("rewrite the pending-input table to its component-117 shape");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = 2, min_reader = 1
         WHERE component = 'lash-postgres-store'",
    )
    .execute(&pool)
    .await
    .expect("stamp a compatible expansion");

    let result = PostgresStorage::from_pool(pool.clone()).await;
    scratch.cleanup().await;
    match result {
        Err(StoreError::Incompatible {
            refusal: CompatRefusal::ShapeRefused { findings, .. },
        }) => {
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.contains("submission_digest")),
                "the missing digest must explain the refusal: {findings:?}"
            );
        }
        Err(other) => panic!("the old pending-input shape got another refusal: {other}"),
        Ok(_) => panic!("the old pending-input shape must be refused"),
    }
}
