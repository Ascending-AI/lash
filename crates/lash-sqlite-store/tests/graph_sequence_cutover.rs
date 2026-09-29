use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::{StorePreflight, StoreSchemaVerdict};
use lash_sqlite_store::{SqliteStore, SqliteStorePreflight};

#[tokio::test]
async fn sqlite_retained_prior_durable_core_is_refused_at_open() {
    let dir = tempfile::tempdir().expect("SQLite predecessor-refusal tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite predecessor fixture");
    #[derive(serde::Serialize)]
    struct PriorEnvelope<'a> {
        descriptor: serde_json::Value,
        compression: &'a str,
        #[serde(with = "serde_bytes")]
        content: &'a [u8],
    }
    let prior_envelope = rmp_serde::to_vec_named(&PriorEnvelope {
        descriptor: serde_json::json!({
            "kind": "CheckpointComponent",
            "hints": ["Compressible", "LargePayload"],
        }),
        compression: "None",
        content: b"pre-cutover payload",
    })
    .expect("encode the pre-cutover envelope shape");
    let blob_ref = lash_core_execution::BlobRef::for_content(b"pre-cutover payload");
    connection
        .execute(
            "INSERT INTO blobs (hash, content) VALUES (?1, ?2)",
            rusqlite::params![blob_ref.as_str(), prior_envelope],
        )
        .expect("retain an envelope with the deleted kind field");
    connection
        .execute("UPDATE lash_compat SET version = 70, min_reader = 70", [])
        .expect("stamp a predecessor whose reader floor excludes this build");
    drop(connection);

    let status = SqliteStorePreflight::for_durable_core(&path)
        .schema_status()
        .await
        .expect("inspect old catalog without decoding blobs");
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::ReaderFloorAbove {
                component: "sqlite-core".to_owned(),
                found: 70,
                min_reader: 70,
                reads: VersionRange::exactly(1),
            },
        }
    );
    assert!(SqliteStore::open(&path).await.is_err());

    let connection = rusqlite::Connection::open(&path).expect("inspect refused catalog");
    let stored: Vec<u8> = connection
        .query_row(
            "SELECT content FROM blobs WHERE hash = ?1",
            [blob_ref.as_str()],
            |row| row.get(0),
        )
        .expect("old envelope remains untouched");
    assert_eq!(stored, prior_envelope);
    let version: i64 = connection
        .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
        .expect("read refused compatibility stamp");
    assert_eq!(version, 70);
}

#[tokio::test]
async fn sqlite_graph_sequence_unique_constraint_is_rejected_without_migration() {
    let dir = tempfile::tempdir().expect("SQLite graph-sequence cutover tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite graph fixture");
    connection
        .execute("ALTER TABLE graph_nodes ADD COLUMN seq INTEGER", [])
        .expect("restore the graph sequence column");
    connection
        .execute(
            "CREATE UNIQUE INDEX idx_graph_nodes_session_seq ON graph_nodes(session_id, seq)",
            [],
        )
        .expect("add a constraint this build cannot safely write beside");
    connection
        .execute("UPDATE lash_compat SET version = 2, min_reader = 1", [])
        .expect("stamp an expanded catalog under this build's reader floor");
    drop(connection);

    let status = SqliteStorePreflight::for_durable_core(&path)
        .schema_status()
        .await
        .expect("inspect expanded graph catalog");
    match &status.databases[0].verdict {
        StoreSchemaVerdict::Refused {
            refusal:
                CompatRefusal::ShapeRefused {
                    component,
                    findings,
                },
        } => {
            assert_eq!(component, "sqlite-core");
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.contains("idx_graph_nodes_session_seq"))
            );
        }
        verdict => panic!("unsafe graph constraint must be refused: {verdict:?}"),
    }
    assert!(SqliteStore::open(&path).await.is_err());

    let connection = rusqlite::Connection::open(&path).expect("inspect refused SQLite catalog");
    let version: i64 = connection
        .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
        .expect("read refused compatibility stamp");
    assert_eq!(
        version, 2,
        "the rejected open must not relabel the graph shape"
    );
}
