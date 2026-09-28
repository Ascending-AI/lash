//! Durable-residue digests for the FIG-2841 no-residue law.
//!
//! The law is: a store operation that returns `Err` for a refusal or a corrupt
//! input leaves durable state byte-identical to before the call.
//!
//! [`RawDurableState`] cannot express that on its own. It *decodes* every row
//! it reads, so it panics on a deliberately corrupted record, and it projects
//! away columns that are not cross-backend comparable — exactly the columns a
//! leaked write would land in. The digest here is therefore separate and
//! deliberately different in kind:
//!
//! * it is decode-free on the SQL backends (raw column text), so a corrupt row
//!   is a comparable value rather than a panic;
//! * it is compared only against *the same backend's* pre-call digest, never
//!   across backends, so no normalization is required and no physical layout
//!   choice can read as drift;
//! * what crosses the backend boundary is the mutated-or-not verdict and the
//!   set of logical tables that moved, which are backend-neutral.
//!
//! The tables come from each backend's own catalog, never from a list here: a
//! table with a `session_id` column is read for this session, every other
//! table is read whole. Adding a table therefore needs no edit to this file.
//! Whole-table reads are sound because every case runs sequentially under the
//! shared database's advisory lock, so no other writer moves a store-wide row
//! between a step's two digests.

use super::*;

/// One backend's durable rows, keyed by logical table, rendered without
/// decoding. Only ever compared with another digest from the same backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ResidueDigest {
    tables: BTreeMap<String, Vec<String>>,
}

impl ResidueDigest {
    /// Logical tables whose rows differ between `self` (pre-call) and `after`.
    pub(super) fn changed_tables(&self, after: &Self) -> Vec<String> {
        let mut changed = Vec::new();
        for table in self.tables.keys().chain(after.tables.keys()) {
            if self.tables.get(table) != after.tables.get(table) && !changed.contains(table) {
                changed.push(table.clone());
            }
        }
        changed.sort_unstable();
        changed
    }
}

/// The reads a plain session filter cannot express, by the logical name they
/// digest under: `(logical, SQLite table, SQLite read, Postgres read)`. The
/// SQLite table name is what the read covers, so the catalog walk skips it.
/// `?1`/`$1` is the session id.
const SCOPED_READS: &[(&str, &str, &str, &str)] = &[
    // Condemnation rows are keyed by attachment, not by session, so they are
    // scoped through this session's manifest rows.
    (
        "attachment_condemnations",
        "attachment_condemnations",
        "SELECT * FROM attachment_condemnations
         WHERE attachment_id IN
             (SELECT attachment_id FROM attachment_manifest WHERE session_id = ?1)",
        "SELECT to_jsonb(t)::text FROM lash_attachment_condemnations t
         WHERE attachment_id IN
             (SELECT attachment_id FROM lash_attachment_manifest WHERE session_id = $1)",
    ),
    (
        "queued_work_items",
        "queued_work_items",
        "SELECT item.* FROM queued_work_items AS item
         JOIN queued_work_batches AS batch ON batch.batch_id = item.batch_id
         WHERE batch.session_id = ?1",
        "SELECT to_jsonb(item)::text FROM lash_queued_work_items AS item
         JOIN lash_queued_work_batches AS batch ON batch.batch_id = item.batch_id
         WHERE batch.session_id = $1",
    ),
    (
        "checkpoint_blob_refs",
        "checkpoint_blob_refs",
        "SELECT * FROM checkpoint_blob_refs
         WHERE checkpoint_ref IN (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1)",
        "SELECT to_jsonb(t)::text FROM lash_checkpoint_blob_refs t
         WHERE checkpoint_ref IN (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)",
    ),
    // Checkpoint manifest and component bytes reachable from this session's
    // head. Blobs are content-addressed and shared by every session, so only
    // the reachable rows are read; a corrupted body is visible here and
    // nowhere else.
    (
        "checkpoint_blobs",
        "blobs",
        "SELECT hash, hex(content) FROM blobs
         WHERE hash IN (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1)
            OR hash IN (
                SELECT blob_ref FROM checkpoint_blob_refs
                WHERE checkpoint_ref IN
                    (SELECT checkpoint_ref FROM session_head WHERE session_id = ?1))",
        "SELECT hash || ':' || encode(content, 'hex') FROM lash_blobs
         WHERE hash IN (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)
            OR hash IN (
                SELECT blob_ref FROM lash_checkpoint_blob_refs
                WHERE checkpoint_ref IN
                    (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1))",
    ),
    (
        "node_anchors",
        "node_anchors",
        "SELECT * FROM node_anchors WHERE source_session_id = ?1",
        "SELECT to_jsonb(t)::text FROM lash_node_anchors t WHERE source_session_id = $1",
    ),
];

/// The SQLite logical name of a Postgres table: the `lash_` prefix dropped,
/// and the two tables Postgres names differently mapped back.
fn postgres_logical_name(table: &str) -> Option<String> {
    let name = table.strip_prefix("lash_")?;
    Some(
        match name {
            "sessions" => "session_head",
            "lashlang_artifacts" => "artifact_refs",
            other => other,
        }
        .to_string(),
    )
}

fn is_scoped(logical: &str) -> bool {
    SCOPED_READS
        .iter()
        .any(|(_, sqlite_table, _, _)| *sqlite_table == logical)
}

/// The catalog read of one SQLite database: every user table, with the
/// session read when it carries a `session_id` column and the whole table
/// otherwise, plus the scoped reads.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn sqlite_residue_reads(connection: &rusqlite::Connection) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT m.name, EXISTS (
                 SELECT 1 FROM pragma_table_info(m.name) WHERE name = 'session_id')
             FROM sqlite_master AS m
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite!_%' ESCAPE '!'
             ORDER BY m.name",
        )
        .expect("prepare the SQLite catalog read");
    let tables = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .expect("read the SQLite catalog")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect the SQLite catalog");
    let mut reads: Vec<(String, String)> = SCOPED_READS
        .iter()
        .map(|(logical, _, sql, _)| ((*logical).to_string(), (*sql).to_string()))
        .collect();
    for (table, has_session) in tables {
        if is_scoped(&table) {
            continue;
        }
        let filter = if has_session {
            " WHERE session_id = ?1"
        } else {
            ""
        };
        let sql = format!("SELECT * FROM \"{table}\"{filter}");
        reads.push((table, sql));
    }
    reads
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn sqlite_residue_digest(path: &Path, session_id: &SessionId) -> ResidueDigest {
    let connection = rusqlite::Connection::open(path).expect("open SQLite residue reader");
    connection
        .busy_timeout(Duration::from_secs(15))
        .expect("configure SQLite residue reader busy timeout");
    sqlite_connection_residue_digest(&connection, session_id)
}

fn sqlite_connection_residue_digest(
    connection: &rusqlite::Connection,
    session_id: &SessionId,
) -> ResidueDigest {
    let mut tables = BTreeMap::new();
    for (table, sql) in sqlite_residue_reads(connection) {
        let mut statement = connection
            .prepare(&sql)
            .unwrap_or_else(|error| panic!("prepare SQLite residue read for `{table}`: {error}"));
        let column_count = statement.column_count();
        let render = |row: &rusqlite::Row| -> rusqlite::Result<String> {
            let mut rendered = String::new();
            for column in 0..column_count {
                let value: rusqlite::types::Value = row.get(column)?;
                let _ = write!(rendered, "{value:?}|");
            }
            Ok(rendered)
        };
        let mut rows: Vec<String> = if statement.parameter_count() == 0 {
            statement.query_map([], render)
        } else {
            statement.query_map([session_id.as_str()], render)
        }
        .unwrap_or_else(|error| panic!("read SQLite residue rows for `{table}`: {error}"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("collect SQLite residue rows for `{table}`: {error}"));
        rows.sort();
        tables.insert(table, rows);
    }
    ResidueDigest { tables }
}

/// The catalog read of the Postgres schema the installation is anchored in.
/// `to_jsonb(row)` renders every column without this harness naming them.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn postgres_residue_reads(connection: &mut PgConnection) -> Vec<(String, String)> {
    let tables: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT c.table_schema::text, c.table_name::text, bool_or(c.column_name = 'session_id')
         FROM information_schema.columns AS c
         JOIN information_schema.tables AS t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
         WHERE t.table_type = 'BASE TABLE'
           AND c.table_schema = (
               SELECT n.nspname FROM pg_catalog.pg_class AS r
               JOIN pg_catalog.pg_namespace AS n ON n.oid = r.relnamespace
               WHERE r.oid = to_regclass('lash_schema_versions'))
         GROUP BY c.table_schema, c.table_name
         ORDER BY c.table_name",
    )
    .fetch_all(&mut *connection)
    .await
    .expect("read the Postgres catalog");
    assert!(
        !tables.is_empty(),
        "no Postgres tables found in the schema that anchors `lash_schema_versions`"
    );
    let mut reads: Vec<(String, String)> = SCOPED_READS
        .iter()
        .map(|(logical, _, _, sql)| ((*logical).to_string(), (*sql).to_string()))
        .collect();
    for (schema, table, has_session) in tables {
        let Some(logical) = postgres_logical_name(&table) else {
            continue;
        };
        if is_scoped(&logical) {
            continue;
        }
        let filter = if has_session {
            " WHERE session_id = $1"
        } else {
            ""
        };
        let sql = format!("SELECT to_jsonb(t)::text FROM \"{schema}\".\"{table}\" t{filter}");
        reads.push((logical, sql));
    }
    reads
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn postgres_residue_digest(
    pool: &PgPool,
    session_id: &SessionId,
) -> ResidueDigest {
    let mut connection = pool
        .acquire()
        .await
        .expect("acquire a Postgres residue reader");
    postgres_connection_residue_digest(&mut connection, session_id).await
}

async fn postgres_connection_residue_digest(
    connection: &mut PgConnection,
    session_id: &SessionId,
) -> ResidueDigest {
    let mut tables = BTreeMap::new();
    for (table, sql) in postgres_residue_reads(connection).await {
        // `$1` is the session id; a whole-table read declares no parameter,
        // and Postgres refuses a bind it does not declare.
        let query = sqlx::query_scalar::<_, String>(&sql);
        let query = if sql.contains("$1") {
            query.bind(session_id.as_str())
        } else {
            query
        };
        let mut rows: Vec<String> = query
            .fetch_all(&mut *connection)
            .await
            .unwrap_or_else(|error| panic!("read Postgres residue rows for `{table}`: {error}"));
        rows.sort();
        tables.insert(table, rows);
    }
    ResidueDigest { tables }
}

/// A table the harness has never heard of is digested the moment the schema
/// declares it: session-scoped when it carries `session_id`, whole otherwise.
#[tokio::test]
async fn residue_digest_covers_a_planted_sqlite_table() {
    let root = tempfile::tempdir().expect("create the planted-table root");
    lash_sqlite_store::SqliteStoreSet::open(root.path())
        .await
        .expect("open a fresh SQLite store set");
    let connection = rusqlite::Connection::open(
        root.path()
            .join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name()),
    )
    .expect("open the durable core");
    connection
        .execute_batch(
            "CREATE TABLE planted_session_rows (session_id TEXT NOT NULL, value TEXT NOT NULL);
             CREATE TABLE planted_store_rows (value TEXT NOT NULL);",
        )
        .expect("plant two tables");
    let session = SessionId::from("planted-session");
    let before = sqlite_connection_residue_digest(&connection, &session);
    connection
        .execute_batch("INSERT INTO planted_session_rows VALUES ('other-session', 'unseen');")
        .expect("write another session's row");
    assert_eq!(
        before.changed_tables(&sqlite_connection_residue_digest(&connection, &session)),
        Vec::<String>::new(),
        "a session-scoped planted table is read for this session only"
    );
    connection
        .execute_batch(
            "INSERT INTO planted_session_rows VALUES ('planted-session', 'leak');
             INSERT INTO planted_store_rows VALUES ('leak');",
        )
        .expect("write the planted rows");
    assert_eq!(
        before.changed_tables(&sqlite_connection_residue_digest(&connection, &session)),
        vec![
            "planted_session_rows".to_string(),
            "planted_store_rows".to_string()
        ],
    );
}

/// The Postgres leg of [`residue_digest_covers_a_planted_sqlite_table`], planted
/// inside a transaction that is rolled back so the shared schema is untouched.
#[tokio::test]
#[ignore = "requires Postgres (LASH_POSTGRES_DATABASE_URL with --include-ignored)"]
async fn residue_digest_covers_a_planted_postgres_table() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => database_url,
        _ => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1"
            );
            eprintln!(
                "SKIPPED planted Postgres residue table; LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url)
        .await
        .expect("connect the Postgres advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .expect("acquire the Postgres advisory lock");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut database_lock)
        .await
        .expect("provision the shared Postgres database from schema.sql");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect the planted-table reader");
    sqlx::raw_sql(
        "BEGIN;
         CREATE TABLE lash_planted_session_rows (session_id TEXT NOT NULL, value TEXT NOT NULL);
         CREATE TABLE lash_planted_store_rows (value TEXT NOT NULL);",
    )
    .execute(&mut connection)
    .await
    .expect("plant two tables");
    let session = SessionId::from("planted-session");
    let before = postgres_connection_residue_digest(&mut connection, &session).await;
    sqlx::raw_sql("INSERT INTO lash_planted_session_rows VALUES ('other-session', 'unseen');")
        .execute(&mut connection)
        .await
        .expect("write another session's row");
    assert_eq!(
        before.changed_tables(&postgres_connection_residue_digest(&mut connection, &session).await),
        Vec::<String>::new(),
        "a session-scoped planted table is read for this session only"
    );
    sqlx::raw_sql(
        "INSERT INTO lash_planted_session_rows VALUES ('planted-session', 'leak');
         INSERT INTO lash_planted_store_rows VALUES ('leak');",
    )
    .execute(&mut connection)
    .await
    .expect("write the planted rows");
    let changed =
        before.changed_tables(&postgres_connection_residue_digest(&mut connection, &session).await);
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut connection)
        .await
        .expect("roll the planted tables back");
    assert_eq!(
        changed,
        vec![
            "planted_session_rows".to_string(),
            "planted_store_rows".to_string()
        ],
    );
}
