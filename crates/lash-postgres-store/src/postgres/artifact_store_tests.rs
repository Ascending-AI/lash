//! Artifact reads on PostgreSQL.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use lash_core_store::artifact_referrer::{ArtifactReferrer, HostArtifactPin, ReferrerClaim};
use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;

use crate::testing::IsolatedDatabase;

/// A read decodes every referrer edge naming the artifact, in stored-pair
/// order, before it answers the bytes: an edge this build cannot read
/// refuses the read even though the bytes are stored, and the refusal is
/// the first such edge's (FIG-7701 R1, review item 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_refuses_on_the_first_unreadable_edge_before_its_bytes() {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the artifact-read edge law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = storage.process_env_store();
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("pin claim");
    let document = lash_kernel_value::names::DocumentId::from_bytes([7; 32]).to_string();
    store
        .publish_module_artifact(&claim, &document, b"stored document")
        .await
        .expect("publish the document");
    assert_eq!(
        store
            .get_module_artifact(&document)
            .await
            .expect("a read under readable edges"),
        Some(b"stored document".to_vec()),
    );

    // Two edges no referrer decode accepts. In stored-pair order `[1` comes
    // before `x`; their decode errors differ (`[1` names a non-string
    // element, `x` is no JSON value).
    for id in ["x", "[1"] {
        sqlx::query(
            "INSERT INTO lash_artifact_referrer_edges
                 (namespace, artifact_ref, referrer_kind, referrer_id)
             VALUES ('kernel_document', $1, 'frame_environment', $2)",
        )
        .bind(&document)
        .bind(id)
        .execute(storage.pool())
        .await
        .expect("plant an unreadable edge");
    }
    let refusal = store
        .get_module_artifact(&document)
        .await
        .expect_err("an unreadable edge refuses the read");
    let message = refusal.to_string();
    assert!(
        message.contains("malformed frame_environment referrer id")
            && message.contains("invalid type: integer"),
        "the first edge in stored-pair order refuses: {message}"
    );
}
