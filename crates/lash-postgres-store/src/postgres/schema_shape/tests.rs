//! Contract tests over both committed artifacts: `schema.sql`, the DDL a host
//! vendors, and `schema-shape.txt`, the structure every open verifies against.

use super::*;
use crate::postgres_test_support;
use sqlx::Connection;

/// The DDL artifact is only vendorable if what the crate compiles is what the
/// repository publishes. Reading the file back at test time is what keeps a
/// future refactor from reintroducing a Rust-literal DDL that silently diverges
/// from the bytes a host copied.
#[test]
fn the_published_ddl_file_is_the_ddl_this_build_executes() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema.sql");
    let published = std::fs::read_to_string(&path).expect("read the committed DDL artifact");
    assert_eq!(
        crate::PostgresStorage::schema_ddl(),
        published,
        "{} must be the exact DDL open executes",
        path.display()
    );
}

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

/// A structural check cannot see a missing row, so the artifact has to carry the
/// seeds itself — otherwise a host that copies it faithfully still ends up with a
/// database lash refuses to open.
#[test]
fn the_published_ddl_seeds_every_required_row() {
    let ddl = crate::PostgresStorage::schema_ddl();
    for (table, _) in SEED_ROWS {
        assert!(
            ddl.contains(&format!("INSERT INTO {table} ")),
            "the DDL artifact must seed {table}"
        );
    }
    assert!(
        ddl.contains(&format!("VALUES ('{SCHEMA_COMPONENT}', {SCHEMA_VERSION})")),
        "the DDL artifact must stamp the component version this build implements"
    );
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

/// Reads the live shape of everything the DDL artifact created in `scratch`.
///
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
fn expected_artifact_parses_and_is_stamped_for_this_build() {
    let (version, shape) =
        SchemaShape::parse(SHAPE_ARTIFACT).expect("the committed shape artifact must parse");
    assert_eq!(
        version, SCHEMA_VERSION,
        "the shape artifact must be regenerated whenever SCHEMA_VERSION moves"
    );
    assert!(
        shape.tables.contains_key("lash_process_events"),
        "the artifact must describe lash's own tables"
    );
}

#[test]
fn expected_artifact_renders_byte_identically() {
    let expected = SchemaShape::expected();
    assert_eq!(
        expected.render(SCHEMA_VERSION),
        SHAPE_ARTIFACT,
        "rendering the parsed artifact must reproduce the committed bytes, so an unchanged \
         schema regenerates to a no-op"
    );
}

/// The database catalog cannot expose fields inside a serialized JSON value, so
/// this is the database-free half of the drift gate: changing the Rust type
/// without regenerating the published artifact fails here in the author's diff.
#[test]
fn committed_payload_shapes_match_registered_rust_types() {
    let expected = SchemaShape::expected();
    for ((table, column), derived) in payload_shape::registered_payload_shapes() {
        let committed = expected
            .tables
            .get(&table)
            .and_then(|table| table.payload_shapes.get(&column))
            .unwrap_or_else(|| {
                panic!("{table}.{column} must publish its registered Rust payload shape")
            });
        assert!(
            committed == &derived,
            "{table}.{column} changed shape inside its JSON blob; bump SCHEMA_VERSION and \
             regenerate schema-shape.txt rather than admitting old rows: {}",
            payload_shape_difference(committed, &derived)
        );
    }
}

#[test]
fn denormalized_session_relation_drift_names_the_changed_column() {
    let expected = SchemaShape::expected();
    let mut found = expected.clone();
    found
        .tables
        .get_mut("lash_session_meta")
        .expect("session metadata table is published")
        .columns
        .get_mut("relation_kind")
        .expect("the denormalized relation discriminator is published")
        .sql_type = "integer".to_string();

    let findings = expected.diff(&found);
    let report = SchemaReport {
        schema: Some("lash".to_string()),
        expected_version: SCHEMA_VERSION,
        found_version: Some(SCHEMA_VERSION),
        findings,
    };
    let rendered = report.to_string();
    assert!(rendered.contains("COLUMN DRIFT"));
    assert!(rendered.contains("lash_session_meta.relation_kind"));
    assert!(rendered.contains("expected text not-null, found integer not-null"));
}

#[test]
fn fig_1219_six_to_two_field_shrink_is_payload_drift() {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Before {
        session_id: String,
        session_name: String,
        created_at: String,
        model: String,
        cwd: Option<String>,
        relation: lash_core::SessionRelation,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct After {
        session_id: String,
        relation: lash_core::SessionRelation,
    }

    let mut before = PayloadShape::of::<Before>();
    before.rust_type = "SessionMeta".to_string();
    let mut after = PayloadShape::of::<After>();
    after.rust_type = "SessionMeta".to_string();
    let mut expected = TableShape::default();
    expected
        .payload_shapes
        .insert("meta_json".to_string(), before);
    let mut found = TableShape::default();
    found.payload_shapes.insert("meta_json".to_string(), after);
    let mut findings = Vec::new();
    diff_payload_shapes("lash_session_meta", &expected, &found, &mut findings);

    let detail = match findings.as_slice() {
        [SchemaFinding::PayloadShapeMismatch { detail, .. }] => detail,
        other => panic!("FIG-1219 shrink must be one payload finding, got {other:?}"),
    };
    for removed in ["session_name", "created_at", "model", "cwd"] {
        assert!(
            detail.contains(&format!("/properties/{removed}")),
            "FIG-1219's removed `{removed}` field must be named: {detail}"
        );
    }
}

#[test]
fn expected_artifact_carries_the_exactly_once_dedup_guard_and_cascades() {
    let expected = SchemaShape::expected();
    let events = expected
        .tables
        .get("lash_process_events")
        .expect("lash_process_events is expected");
    assert!(
        events.unique_guards.contains(&UniqueGuard {
            primary_key: false,
            columns: vec!["process_id".to_string(), "idempotency_key".to_string()],
            predicate: Some("idempotency_key is not null".to_string()),
            nulls_not_distinct: false,
        }),
        "the partial unique index guarding exactly-once dedup must be in scope: {:?}",
        events.unique_guards
    );
    let cascades = expected
        .tables
        .values()
        .flat_map(|table| table.foreign_keys.iter())
        .filter(|key| key.on_delete == ForeignKeyAction::Cascade)
        .count();
    assert!(
        cascades >= 7,
        "every ON DELETE CASCADE foreign key must be in scope, found {cascades}"
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

#[test]
fn foreign_key_lines_round_trip_through_the_artifact_format() {
    let key = ForeignKeyShape {
        columns: vec!["process_id".to_string()],
        parent_table: "lash_processes".to_string(),
        parent_columns: vec!["process_id".to_string()],
        on_delete: ForeignKeyAction::Cascade,
    };
    assert_eq!(parse_foreign_key_line(&key.to_string()), Some(key));
}

#[test]
fn drift_renders_a_sectioned_named_diff_not_a_hash() {
    let report = SchemaReport {
        schema: Some("lash".to_string()),
        expected_version: SCHEMA_VERSION,
        found_version: Some(SCHEMA_VERSION),
        findings: vec![
            SchemaFinding::MissingTable {
                table: "lash_process_observers".to_string(),
            },
            SchemaFinding::ColumnMismatch {
                table: "lash_processes".to_string(),
                expected: ColumnShape {
                    name: "status".to_string(),
                    sql_type: "text".to_string(),
                    nullable: false,
                    value_source: ColumnValueSource::Supplied,
                },
                found: ColumnShape {
                    name: "status".to_string(),
                    sql_type: "character varying(64)".to_string(),
                    nullable: true,
                    value_source: ColumnValueSource::Supplied,
                },
            },
            SchemaFinding::MissingUniqueGuard {
                table: "lash_process_events".to_string(),
                expected: UniqueGuard {
                    primary_key: false,
                    columns: vec!["process_id".to_string(), "idempotency_key".to_string()],
                    predicate: Some("idempotency_key is not null".to_string()),
                    nulls_not_distinct: false,
                },
            },
            SchemaFinding::MissingSeedRow {
                table: "lash_process_change_clock".to_string(),
                detail: "transactional process-change clock".to_string(),
            },
        ],
    };
    let rendered = report.to_string();
    for expected_fragment in [
        "MISSING TABLES\n  lash_process_observers: table is missing",
        "COLUMN DRIFT\n  lash_processes.status: expected text not-null, found character \
         varying(64) nullable",
        "UNIQUE GUARD DRIFT\n  lash_process_events: missing unique (process_id, idempotency_key) \
         where idempotency_key is not null",
        "SEED ROWS\n  lash_process_change_clock: seed row is missing",
        "SchemaCheck::WarnOnly",
    ] {
        assert!(
            rendered.contains(expected_fragment),
            "the diff must name the drifted object; missing {expected_fragment:?} in:\n{rendered}"
        );
    }
}

/// The single drift gate over the DDL artifact: the committed expectation must be
/// exactly what `schema.sql` produces in a live database. Because it reads the
/// catalog rather than the DDL text, it also proves the expectation is
/// reproducible on whichever PostgreSQL major CI points it at — the matrix
/// asserts 14, 16, and 18 all render the same artifact.
#[tokio::test]
async fn committed_shape_artifact_matches_the_ddl_artifact() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping schema shape artifact drift check: database URL is not set");
        return;
    };
    let (connection, scratch, live) = provision_scratch_schema(&database_url).await;
    let rendered = live.render(SCHEMA_VERSION);
    drop_scratch_schema(connection, &scratch).await;

    if rendered != SHAPE_ARTIFACT {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema-shape.txt");
        if std::env::var("LASH_UPDATE_SCHEMA_SHAPE").as_deref() == Ok("1") {
            std::fs::write(&path, &rendered).expect("rewrite the shape artifact");
            panic!(
                "regenerated {} -- rerun the suite to confirm",
                path.display()
            );
        }
        panic!(
            "the committed schema shape artifact does not match what schema.sql produces on this \
             PostgreSQL. Regenerate it with LASH_UPDATE_SCHEMA_SHAPE=1 and review the diff.\n\
             --- committed ---\n{SHAPE_ARTIFACT}\n--- live ---\n{rendered}"
        );
    }
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

/// Applying the artifact twice must be a no-op, which is what lets
/// `SchemaProvisioning::LashManaged` run it on every open.
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
    let secret: Vec<u8> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_one(&mut connection)
    .await
    .expect("read the seeded signing secret");
    assert_eq!(
        secret.len(),
        32,
        "the artifact's seed statement must produce a 32-byte await-event signing secret"
    );
    drop_scratch_schema(connection, &scratch).await;
}
