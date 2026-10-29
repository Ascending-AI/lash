//! Contract tests over both committed artifacts: `schema.sql`, the DDL a host
//! vendors, and `schema-shape.txt`, the structure every open verifies against.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::artifacts::{
    drop_scratch_schema, provision_scratch_schema, read_scratch_shape, teardown_artifact,
};
use super::*;
use crate::postgres_test_support;

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

/// Content addresses use the same 32-byte key space on both stores. Follow
/// foreign keys from the content tables so every referrer is part of this law.
#[test]
fn content_address_columns_are_binary_keys() {
    let ddl = crate::PostgresStorage::schema_ddl();
    let mut table = "";
    for line in ddl.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            table = name.split_whitespace().next().expect("table name");
        }
        let references_content = line.split_once("REFERENCES ").is_some_and(|(_, target)| {
            target.starts_with("lash_blobs(hash)") || target.starts_with("lash_prompt_texts(hash)")
        });
        let content_key = line.starts_with("attachment_id ")
            || references_content
            || (matches!(table, "lash_blobs" | "lash_prompt_texts") && line.starts_with("hash "))
            || (table == "lash_retirement_frontier" && line.starts_with("node_ref "));
        if content_key {
            assert_eq!(
                line.split_whitespace().nth(1),
                Some("BYTEA"),
                "{table}: content addresses must be binary keys: {line}"
            );
        }
    }
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
    // no-op the second time. The generated function's statements are not
    // seeds: they run when it is called.
    let (seeds, generated) = ddl
        .split_once("-- BEGIN GENERATED lash_owner_commit")
        .expect("schema.sql declares the generated owner commit");
    let (_, seeds_after) = generated
        .split_once("-- END GENERATED lash_owner_commit")
        .expect("schema.sql ends the generated owner commit");
    let seeds = format!("{seeds}{seeds_after}");
    assert_eq!(
        seeds.matches("INSERT INTO ").count(),
        seeds.matches("ON CONFLICT").count(),
        "every seed insert must be idempotent"
    );
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

#[tokio::test]
async fn retention_keeps_terminal_process_until_cascade_drains() {
    let url = crate::testing::required_database_url();
    let (mut conn, scratch, _) = provision_scratch_schema(&url).await;
    sqlx::query("INSERT INTO lash_processes (process_id, originator_id, identity_kind, created_at_ms, updated_at_ms, change_seq, lifetime, start_json, execution_json, lifecycle_json, cascade_cursor) VALUES ('parent', 'origin', 'standard', 0, 0, 1, 'detached', '{}', '{}', '{\"last_event_sequence\":0,\"lifecycle\":{\"state\":\"terminal\",\"occurred_at_ms\":0,\"outcome\":{\"type\":\"settled\",\"output\":{\"outcome\":{\"status\":\"success\"}}}}}', 'child')").execute(&mut conn).await.expect("terminal with unfinished cascade");
    let statements = crate::process_sql::process_sql();
    for sql in [
        statements.process_postgres.list_prunable_terminal.sql(),
        statements
            .process_postgres
            .list_prunable_terminal_for_update
            .sql(),
    ] {
        let rows = sqlx::query(sql)
            .bind(1_i64)
            .bind(Option::<i64>::None)
            .fetch_all(&mut conn)
            .await
            .expect("candidate list");
        assert!(rows.is_empty(), "an unfinished cascade retains its parent");
    }
    sqlx::query("UPDATE lash_processes SET cascade_cursor = NULL")
        .execute(&mut conn)
        .await
        .expect("drain cascade");
    for sql in [
        statements.process_postgres.list_prunable_terminal.sql(),
        statements
            .process_postgres
            .list_prunable_terminal_for_update
            .sql(),
    ] {
        let rows = sqlx::query(sql)
            .bind(1_i64)
            .bind(Option::<i64>::None)
            .fetch_all(&mut conn)
            .await
            .expect("candidate list");
        assert_eq!(rows.len(), 1);
    }
    drop_scratch_schema(conn, &scratch).await;
}
