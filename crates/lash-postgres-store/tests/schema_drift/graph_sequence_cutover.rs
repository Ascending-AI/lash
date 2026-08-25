use super::*;

/// Component 59 still has an otherwise applicable creation-only declaration,
/// but its published graph shape carries the sequence column retired by 61.
/// The hard-cutover preflight must refuse before creating any component-60
/// artifact or advancing the component stamp.
#[tokio::test]
async fn component_59_graph_sequence_shape_is_refused_before_migration_ddl() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-59 graph cutover law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "DROP INDEX idx_lash_session_meta_state_version;
             ALTER TABLE lash_session_meta DROP COLUMN session_state_version;
             ALTER TABLE lash_graph_nodes ADD COLUMN seq BIGSERIAL;
             CREATE INDEX idx_lash_graph_nodes_seq
                 ON lash_graph_nodes(session_id, seq);
             UPDATE lash_schema_versions
                SET version = 59
              WHERE component = 'lash-postgres-store'",
        )
        .await;

    let error = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .err()
    .expect("the published component-59 graph shape must be refused");
    let message = error.to_string();
    let retired_column = format!("{}.lash_graph_nodes.seq", scratch.name);
    assert!(
        message.contains("explicitly retired by the current hard cutover")
            && message.contains(&retired_column),
        "the refusal must identify the retired graph column: {message}"
    );

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read component version after hard-cutover refusal");
    assert_eq!(version, 59, "the refusal must not advance the stamp");
    let state_version_index_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('idx_lash_session_meta_state_version') IS NOT NULL")
            .fetch_one(&scratch.pool)
            .await
            .expect("probe component-60 index after hard-cutover refusal");
    assert!(
        !state_version_index_exists,
        "the refusal must not run the component-60 creation DDL"
    );
    scratch.cleanup().await;
}
