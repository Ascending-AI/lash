//! Artifact generation and scratch catalog support for tooling and drift checks.

#![expect(
    clippy::expect_used,
    reason = "artifact tooling and test setup stop on a malformed compiled DDL or failed private scratch database"
)]

use super::*;
use sqlx::Connection;

pub(crate) fn teardown_artifact() -> String {
    let mut drops = Vec::new();
    for line in crate::PostgresStorage::schema_ddl().lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            let table = rest
                .split_whitespace()
                .next()
                .expect("a CREATE TABLE line must name its table");
            drops.push(format!("DROP TABLE IF EXISTS {table} CASCADE;"));
        } else if let Some(rest) = line.strip_prefix("CREATE SEQUENCE IF NOT EXISTS ") {
            let sequence = rest
                .trim_end_matches(';')
                .split_whitespace()
                .next()
                .expect("a CREATE SEQUENCE line must name its sequence");
            drops.push(format!("DROP SEQUENCE IF EXISTS {sequence} CASCADE;"));
        } else if let Some(rest) = line.strip_prefix("CREATE OR REPLACE VIEW ") {
            let view = rest
                .split_whitespace()
                .next()
                .expect("a CREATE VIEW line must name its view");
            drops.push(format!("DROP VIEW IF EXISTS {view} CASCADE;"));
        } else if let Some(rest) = line.strip_prefix("CREATE OR REPLACE FUNCTION ") {
            let function = rest
                .split('(')
                .next()
                .expect("a CREATE FUNCTION line must name its function");
            drops.push(format!("DROP FUNCTION IF EXISTS {function} CASCADE;"));
        } else if line.starts_with("CREATE ") {
            // Indexes ride on their tables and need no drop. Any other kind —
            // a type, a function — must teach the generator how to drop it
            // here rather than ship a teardown that misses it.
            assert!(
                line.starts_with("CREATE INDEX IF NOT EXISTS")
                    || line.starts_with("CREATE UNIQUE INDEX IF NOT EXISTS"),
                "schema.sql creates an object teardown does not model: {line}"
            );
        }
    }
    assert!(
        !drops.is_empty(),
        "the schema object list must not be empty"
    );
    format!(
        "-- lash-postgres-store teardown, component version {SCHEMA_VERSION}.\n\
         --\n\
         -- Generated artifact. These bytes are exactly the DDL a host applies to drop\n\
         -- everything this component owns at the reject-and-recreate boundary;\n\
         -- `PostgresStorage::teardown_ddl()` returns this file verbatim. Every\n\
         -- statement is idempotent (`IF EXISTS`), and `CASCADE` releases the intra-lash\n\
         -- foreign keys so table order carries no meaning. Indexes, constraints, and\n\
         -- seed rows die with their tables; each sequence, view and function\n\
         -- schema.sql declares is dropped by name, and it declares no types.\n\
         --\n\
         -- Like schema.sql, nothing here is schema-qualified: the file tears down\n\
         -- whichever schema the session's `search_path` resolves. Regenerate it with\n\
         -- `python3 scripts/regenerate-postgres-artifact.py teardown.sql`, never by hand.\n\
         --\n\
         {}\n",
        drops.join("\n\n")
    )
}

/// Applies `schema.sql` into a throwaway PostgreSQL schema and returns the live
/// shape the DDL actually produces, alongside the schema name it resolved in.
///
/// The scratch schema is deliberately not `public`: introspection that hard-coded
/// `public` would find nothing here, which is the property the test asserts on
/// every run.
pub(super) async fn provision_scratch_schema(
    database_url: &str,
) -> (sqlx::PgConnection, String, SchemaShape) {
    let mut connection = sqlx::PgConnection::connect(database_url)
        .await
        .expect("connect scratch schema");
    let scratch = format!("lash_shape_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {scratch}"))
        .execute(&mut connection)
        .await
        .expect("create scratch schema");
    sqlx::query(&format!("SET search_path TO {scratch}"))
        .execute(&mut connection)
        .await
        .expect("point search_path at the scratch schema");
    sqlx::raw_sql(crate::schema::SCHEMA_DDL)
        .execute(&mut connection)
        .await
        .expect("apply the committed schema.sql artifact");
    let shape = read_scratch_shape(&mut connection, &scratch).await;
    (connection, scratch, shape)
}

/// The table list comes from the catalog, not from the committed expectation, so
/// the artifact generator cannot bootstrap itself off a stale artifact — a table
/// added to `schema.sql` but absent from `schema-shape.txt` shows up as drift.
pub(super) async fn read_scratch_shape(
    connection: &mut sqlx::PgConnection,
    scratch: &str,
) -> SchemaShape {
    let table_names: Vec<String> = sqlx::query_scalar(
        r"SELECT relation.relname::text
          FROM pg_catalog.pg_class AS relation
          JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace
          WHERE namespace.nspname = $1
            AND relation.relkind IN ('r', 'p')
            AND relation.relname LIKE 'lash\_%'
          ORDER BY relation.relname",
    )
    .bind(scratch)
    .fetch_all(&mut *connection)
    .await
    .expect("discover the tables schema.sql created");
    assert!(
        table_names.len() > 20,
        "the DDL artifact must create lash's whole table set, found {table_names:?}"
    );
    let search_path = read_search_path(connection)
        .await
        .expect("read scratch search path");
    let installation = resolve_installation(connection, &search_path)
        .await
        .expect("resolve scratch installation")
        .expect("the scratch schema is provisioned");
    let resolved = resolve_tables(connection, &installation, &table_names)
        .await
        .expect("resolve scratch tables");
    read_live_shape(connection, &installation, &resolved)
        .await
        .expect("read scratch shape")
}

pub(super) async fn drop_scratch_schema(mut connection: sqlx::PgConnection, scratch: &str) {
    sqlx::query(&format!("DROP SCHEMA {scratch} CASCADE"))
        .execute(&mut connection)
        .await
        .expect("drop scratch schema");
}

pub(crate) async fn schema_shape_artifact(database_url: &str) -> String {
    let (connection, scratch, live) = provision_scratch_schema(database_url).await;
    let rendered = live.render(SCHEMA_VERSION);
    drop_scratch_schema(connection, &scratch).await;
    rendered
}
