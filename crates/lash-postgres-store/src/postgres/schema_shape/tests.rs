//! Contract tests over both committed artifacts: `schema.sql`, the DDL a host
//! vendors, and `schema-shape.txt`, the structure every open verifies against.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::postgres_test_support;
use sqlx::Connection;

/// A host applies this file into a schema it may not own outright, possibly more
/// than once. Every statement must therefore be creation-only and idempotent, and
/// nothing may be schema-qualified.
#[test]
fn the_published_ddl_is_creation_only_and_unqualified() {
    let ddl = crate::PostgresStorage::schema_ddl();
    let statements: Vec<&str> = ddl
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect();
    let body = statements.join("\n");
    for forbidden in ["DROP ", "ALTER ", "TRUNCATE ", "GRANT ", "public."] {
        assert!(
            !body.contains(forbidden),
            "the DDL artifact must not contain `{forbidden}`: a host applies it into its own \
             schema, possibly without the privilege to do that"
        );
    }
    let creations = body.matches("CREATE TABLE IF NOT EXISTS").count();
    assert!(
        creations > 20,
        "every table must be created idempotently, found {creations}"
    );
    assert_eq!(
        body.matches("CREATE TABLE ").count(),
        creations,
        "no table may be created non-idempotently"
    );
    assert_eq!(
        body.matches("CREATE INDEX ").count(),
        body.matches("CREATE INDEX IF NOT EXISTS").count(),
        "no index may be created non-idempotently"
    );
    assert_eq!(
        body.matches("CREATE UNIQUE INDEX ").count(),
        body.matches("CREATE UNIQUE INDEX IF NOT EXISTS").count(),
        "no unique index may be created non-idempotently"
    );
}

/// The teardown artifact is the same contract the other way: it must be the
/// exact bytes this build executes, and its statement list must be generated
/// from the object list `schema.sql` declares — never maintained by hand,
/// where a future table or a future non-table object could silently escape it.
#[test]
fn the_published_teardown_is_generated_from_the_schema_object_list() {
    let expected = teardown_artifact();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("teardown.sql");
    let published = std::fs::read_to_string(&path).expect("read the committed teardown artifact");
    assert_eq!(published, expected);
    assert_eq!(crate::PostgresStorage::teardown_ddl(), expected);
}

fn teardown_artifact() -> String {
    let mut drops = Vec::new();
    for line in crate::PostgresStorage::schema_ddl().lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            let table = rest
                .split_whitespace()
                .next()
                .expect("a CREATE TABLE line must name its table");
            drops.push(format!("DROP TABLE IF EXISTS {table} CASCADE;"));
        } else if line.starts_with("CREATE ") {
            // Indexes ride on their tables and need no drop. Any other kind —
            // a sequence, a type, a function — must teach the generator how to
            // drop it here rather than ship a teardown that misses it.
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
         -- seed rows die with their tables; schema.sql declares no standalone\n\
         -- sequences, types, or functions, so there is nothing else to drop.\n\
         --\n\
         -- Like schema.sql, nothing here is schema-qualified: the file tears down\n\
         -- whichever schema the session's `search_path` resolves. Regenerate it with\n\
         -- the schema_shape suite's LASH_REGENERATE=1 path, never by hand.\n\
         --\n\
         {}\n",
        drops.join("\n\n")
    )
}

#[test]
#[ignore = "regenerates crates/lash-postgres-store/teardown.sql"]
fn regenerate_teardown_artifact() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    std::fs::write(regeneration_path("teardown.sql"), teardown_artifact())
        .expect("rewrite the teardown artifact");
}

fn regeneration_path(name: &str) -> std::path::PathBuf {
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
    std::path::PathBuf::from(root)
        .join("crates/lash-postgres-store")
        .join(name)
}

/// A structural check cannot see a missing row, so the artifact has to carry the
/// seeds itself — otherwise a host that copies it faithfully still ends up with a
/// database lash refuses to open.
#[test]
fn the_published_ddl_seeds_every_required_row() {
    let ddl = crate::PostgresStorage::schema_ddl();
    let header_version: i32 = ddl
        .lines()
        .next()
        .expect("the DDL artifact must have a header line")
        .strip_prefix("-- lash-postgres-store schema, component version ")
        .expect("the DDL header must declare its component version")
        .strip_suffix('.')
        .expect("the DDL header ends after the component version")
        .parse()
        .expect("the component version must be an integer");
    let seed = ddl
        .lines()
        .find(|line| line.starts_with(&format!("VALUES ('{SCHEMA_COMPONENT}', ")))
        .expect("the DDL artifact must seed the compatibility stamp");
    assert_eq!(header_version, SCHEMA_VERSION);
    assert_eq!(
        seed,
        format!("VALUES ('{SCHEMA_COMPONENT}', {SCHEMA_VERSION}, {SCHEMA_VERSION})")
    );
    for (table, _, _) in SEED_ROWS {
        assert!(
            ddl.contains(&format!("INSERT INTO {table} ")),
            "the DDL artifact must seed {table}"
        );
    }
    // Every seed is re-applied on each lash-managed open, so each must be a
    // no-op the second time.
    assert_eq!(
        ddl.matches("INSERT INTO ").count(),
        ddl.matches("ON CONFLICT").count(),
        "every seed insert must be idempotent"
    );
}

/// Applies `schema.sql` into a throwaway PostgreSQL schema and returns the live
/// shape the DDL actually produces, alongside the schema name it resolved in.
///
/// The scratch schema is deliberately not `public`: introspection that hard-coded
/// `public` would find nothing here, which is the property the test asserts on
/// every run.
async fn provision_scratch_schema(database_url: &str) -> (sqlx::PgConnection, String, SchemaShape) {
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
async fn read_scratch_shape(connection: &mut sqlx::PgConnection, scratch: &str) -> SchemaShape {
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
    read_live_shape(connection, &resolved)
        .await
        .expect("read scratch shape")
}

async fn drop_scratch_schema(mut connection: sqlx::PgConnection, scratch: &str) {
    sqlx::query(&format!("DROP SCHEMA {scratch} CASCADE"))
        .execute(&mut connection)
        .await
        .expect("drop scratch schema");
}

#[test]
fn predicate_normalization_is_insensitive_to_parens_case_and_spacing() {
    assert_eq!(
        normalize_predicate("(idempotency_key IS NOT NULL)"),
        "idempotency_key is not null"
    );
    assert_eq!(
        normalize_predicate("  ((idempotency_key   IS  not null))  "),
        "idempotency_key is not null"
    );
    // A pair of parens that does not enclose the whole expression is preserved,
    // so `(a) AND (b)` never collapses into something else.
    assert_eq!(normalize_predicate("(a) AND (b)"), "(a) and (b)");
}

#[test]
fn column_lines_round_trip_through_the_artifact_format() {
    let column = ColumnShape {
        name: "seq".to_string(),
        sql_type: "bigint".to_string(),
        nullable: false,
        value_source: ColumnValueSource::Default,
    };
    assert_eq!(
        parse_column_line("seq bigint not-null default"),
        Some(column)
    );
    // Multi-word types survive, so a host's `character varying(64)` renders in a
    // mismatch rather than failing to parse.
    assert_eq!(
        parse_column_line("status character varying(64) nullable"),
        Some(ColumnShape {
            name: "status".to_string(),
            sql_type: "character varying(64)".to_string(),
            nullable: true,
            value_source: ColumnValueSource::Supplied,
        })
    );
    assert_eq!(parse_column_line("status text"), None);
}

/// The single drift gate over the DDL artifact: the committed expectation must be
/// exactly what `schema.sql` produces in a live database. Because it reads the
/// catalog rather than the DDL text, it also proves the expectation is
/// reproducible on PostgreSQL 18, the one supported major.
#[tokio::test]
async fn committed_shape_artifact_matches_the_ddl_artifact() {
    let Some(rendered) = schema_shape_artifact().await else {
        return;
    };
    assert_eq!(rendered, SHAPE_ARTIFACT);
}

async fn schema_shape_artifact() -> Option<String> {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping schema shape artifact drift check: database URL is not set");
        return None;
    };
    let (connection, scratch, live) = provision_scratch_schema(&database_url).await;
    let rendered = live.render(SCHEMA_VERSION);
    drop_scratch_schema(connection, &scratch).await;
    Some(rendered)
}

#[tokio::test]
#[ignore = "regenerates crates/lash-postgres-store/schema-shape.txt"]
async fn regenerate_schema_shape_artifact() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let rendered = schema_shape_artifact()
        .await
        .expect("regeneration requires PostgreSQL");
    std::fs::write(regeneration_path("schema-shape.txt"), rendered)
        .expect("rewrite the shape artifact");
}

/// The DDL artifact provisions into whatever schema `search_path` resolves, and
/// the check follows it. A regression that hard-coded `public` fails here.
#[tokio::test]
async fn a_freshly_provisioned_scratch_schema_is_conformant() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping scratch-schema conformance: database URL is not set");
        return;
    };
    let (mut connection, scratch, _) = provision_scratch_schema(&database_url).await;
    let report = verify_schema_shape(&mut connection)
        .await
        .expect("verify the scratch schema");
    assert!(
        report.is_conformant(),
        "a schema provisioned from schema.sql must verify clean: {report}"
    );
    assert_eq!(
        report.schema.as_deref(),
        Some(scratch.as_str()),
        "the check must report the schema it actually resolved, not `public`"
    );
    assert_eq!(report.found_version, Some(SCHEMA_VERSION));
    drop_scratch_schema(connection, &scratch).await;
}

/// Applying the artifact twice must be a no-op, which is what lets `lash
/// migrate` re-run it idempotently and lets a host re-apply it safely.
#[tokio::test]
async fn the_ddl_artifact_is_idempotent() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping DDL idempotence check: database URL is not set");
        return;
    };
    let (mut connection, scratch, first) = provision_scratch_schema(&database_url).await;
    sqlx::raw_sql(crate::schema::SCHEMA_DDL)
        .execute(&mut connection)
        .await
        .expect("reapply the schema artifact");
    let second = read_scratch_shape(&mut connection, &scratch).await;
    assert_eq!(first, second, "reapplying schema.sql must change nothing");
    drop_scratch_schema(connection, &scratch).await;
}
