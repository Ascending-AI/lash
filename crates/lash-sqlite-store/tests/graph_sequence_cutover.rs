use lash_core_execution::{StorePreflight, StoreSchemaVerdict};
use lash_sqlite_store::{SESSION_SCHEMA_VERSION, SqliteStorePreflight, Store};

const RETAINED_PRIOR_DURABLE_CORE_GENERATION: i32 = 70;

#[tokio::test]
async fn sqlite_retained_prior_durable_core_is_refused_at_open() {
    // Generation 70 retains the pre-envelope-cutover witness; the current
    // message-body cutover must continue refusing it before blob decoding.
    assert_eq!(SESSION_SCHEMA_VERSION, 81);
    let dir = tempfile::tempdir().expect("SQLite predecessor-refusal tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
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
        .pragma_update(None, "user_version", RETAINED_PRIOR_DURABLE_CORE_GENERATION)
        .expect("stamp retained SQLite durable-core predecessor");
    drop(connection);

    let status = SqliteStorePreflight::for_durable_core(&path)
        .schema_status()
        .await
        .expect("inspect old catalog without decoding blobs");
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Mismatch {
            found: i64::from(RETAINED_PRIOR_DURABLE_CORE_GENERATION),
        }
    );
    let error = Store::open(&path)
        .await
        .err()
        .expect("the retained SQLite durable-core predecessor must be refused at open")
        .to_string();
    assert!(
        error.contains(&format!("supports schema version {SESSION_SCHEMA_VERSION}"))
            && error.contains(&format!(
                "database reports version {RETAINED_PRIOR_DURABLE_CORE_GENERATION}"
            )),
        "the predecessor refusal must identify expected and found versions: {error}"
    );
    let connection = rusqlite::Connection::open(&path).expect("inspect refused catalog");
    let stored: Vec<u8> = connection
        .query_row(
            "SELECT content FROM blobs WHERE hash = ?1",
            [blob_ref.as_str()],
            |row| row.get(0),
        )
        .expect("old envelope remains untouched");
    assert_eq!(stored, prior_envelope);
    let version: i32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read refused version");
    assert_eq!(version, RETAINED_PRIOR_DURABLE_CORE_GENERATION);
}

#[tokio::test]
async fn sqlite_41_graph_sequence_shape_is_rejected_without_migration() {
    let dir = tempfile::tempdir().expect("SQLite graph-sequence cutover tempdir");
    let path = dir.path().join("durable-core.db");
    drop(
        Store::open(&path)
            .await
            .expect("create current SQLite catalog"),
    );

    let connection = rusqlite::Connection::open(&path).expect("open SQLite 41 fixture");
    connection
        .execute("ALTER TABLE graph_nodes ADD COLUMN seq INTEGER", [])
        .expect("restore the version-40 graph sequence column");
    connection
        .execute(
            "CREATE INDEX idx_graph_nodes_session_seq ON graph_nodes(session_id, seq)",
            [],
        )
        .expect("restore the version-40 graph sequence index");
    connection
        .pragma_update(None, "user_version", 41)
        .expect("stamp SQLite durable-core 41");
    drop(connection);

    let error = Store::open(&path)
        .await
        .err()
        .expect("version-41 graph shape must be rejected")
        .to_string();
    // The fixture is built by opening at the current generation first, so the
    // durable core carries this build's release stamp; the refusal names it
    // after the pinned sentences rather than in place of any of them.
    assert_eq!(
        error,
        format!(
            "Error(\"Unsupported lash durable core schema: this binary supports schema version 81, but the database reports version 41. There is no migration chain — drain affected sessions and recreate the whole Lash trust domain with this version. Reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/adr/0049-session-ids-are-used-once.md. This store was last written by lash release {}.\")",
            env!("CARGO_PKG_VERSION")
        )
    );
    let connection = rusqlite::Connection::open(&path).expect("inspect refused SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read refused SQLite version"),
        41,
        "the rejected open must not relabel the old graph shape"
    );
}
