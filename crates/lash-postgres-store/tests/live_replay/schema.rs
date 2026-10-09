//! The live replay store's published schema artifact and its two schema
//! modes (FIG-5220): `install` creates the tables from the artifact,
//! `verify_only` runs no DDL, and both refuse tables that differ from it
//! without repairing them.

#![allow(clippy::disallowed_methods)]

use std::str::FromStr as _;

use lash::postgres::{
    PostgresHostConfig, PostgresLiveReplayError, PostgresLiveReplaySchemaFinding,
    PostgresLiveReplayStore, ReplaySchemaMode,
};
use lash_postgres_store::testing::{IsolatedDatabase, required_database_url};
use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions as _, Connection as _, PgConnection, PgPool};

use super::{config, endpoints, fresh_schema, publish, with_data};

fn verify_only(schema: &str) -> PostgresHostConfig {
    with_data(schema, |data| {
        data.schema_mode = ReplaySchemaMode::VerifyOnly
    })
}

/// What a host's migration tooling does: create the schema and apply the
/// artifact's bytes into it, twice, since a rerun must be a no-op.
async fn host_provision(url: &str, schema: &str) {
    let mut connection = PgConnection::connect(url).await.expect("connect as host");
    sqlx::raw_sql(&format!(
        "CREATE SCHEMA \"{schema}\"; SET search_path TO \"{schema}\";"
    ))
    .execute(&mut connection)
    .await
    .expect("create the schema");
    for _ in 0..2 {
        sqlx::raw_sql(PostgresLiveReplayStore::schema_ddl())
            .execute(&mut connection)
            .await
            .expect("apply the published artifact");
    }
}

async fn execute(url: &str, statement: &str) {
    let mut connection = PgConnection::connect(url).await.expect("connect");
    sqlx::raw_sql(statement)
        .execute(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
}

async fn refusal(url: &str, config: PostgresHostConfig) -> Vec<PostgresLiveReplaySchemaFinding> {
    match PostgresLiveReplayStore::connect(&endpoints(url), &config).await {
        Err(PostgresLiveReplayError::SchemaDrift(report)) => report.findings().to_vec(),
        Err(other) => panic!("expected a schema drift refusal, got {other}"),
        Ok(_) => panic!("expected a schema drift refusal, the store connected"),
    }
}

/// A host vendors the artifact into a schema it may not own outright,
/// possibly more than once: every statement is creation-only and
/// idempotent, and nothing names a schema.
#[test]
fn the_published_ddl_is_creation_only_and_unqualified() {
    let body = PostgresLiveReplayStore::schema_ddl()
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "DROP ",
        "ALTER ",
        "TRUNCATE ",
        "GRANT ",
        "INSERT ",
        "SCHEMA",
        "public.",
        "lash_live_replay",
    ] {
        assert!(
            !body.contains(forbidden),
            "the artifact must not contain `{forbidden}`"
        );
    }
    let creates = body.matches("CREATE ").count();
    assert!(creates >= 3, "the artifact creates the store's tables");
    assert_eq!(
        body.matches(" IF NOT EXISTS ").count(),
        creates,
        "every object is created idempotently"
    );
}

/// The committed shape is the catalog the artifact produces: an `install`
/// connect provisions exactly it, and a second connect over the same
/// schema finds it unchanged.
#[tokio::test]
async fn the_install_mode_provisions_exactly_the_committed_shape() {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    for _ in 0..2 {
        drop(
            PostgresLiveReplayStore::connect(&endpoints(database.url()), &config(&schema))
                .await
                .expect("install connects"),
        );
    }
    let pool = PgPool::connect(database.url()).await.expect("pool");
    let report = PostgresLiveReplayStore::verify_schema(&pool, &schema)
        .await
        .expect("verify");
    assert!(report.is_conformant(), "{report}");
    assert_eq!(
        report.found_shape(),
        PostgresLiveReplayStore::schema_shape()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "regenerates crates/lash/postgres-live-replay-schema-shape.txt"]
async fn regenerate_live_replay_schema_shape() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    host_provision(database.url(), &schema).await;
    let pool = PgPool::connect(database.url()).await.expect("pool");
    let report = PostgresLiveReplayStore::verify_schema(&pool, &schema)
        .await
        .expect("read the provisioned catalog");
    pool.close().await;
    std::fs::write(
        std::path::Path::new(&root).join("crates/lash/postgres-live-replay-schema-shape.txt"),
        report.found_shape(),
    )
    .expect("rewrite the shape artifact");
}

/// A host provisions the schema with the artifact; the store then connects
/// in `verify_only` mode under a role that cannot run DDL at all, and
/// serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_provisioned_schema_serves_verify_only_under_a_role_without_ddl_privileges() {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    host_provision(database.url(), &schema).await;
    let role = format!("live_replay_runtime_{}", uuid::Uuid::new_v4().simple());
    // A server that authenticates by password (the release gate's) refuses
    // a login role without one.
    let password = uuid::Uuid::new_v4().simple().to_string();
    execute(
        database.url(),
        &format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'; \
             GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\"; \
             GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON ALL TABLES IN SCHEMA \"{schema}\" \
               TO \"{role}\";"
        ),
    )
    .await;
    let runtime_url = PgConnectOptions::from_str(database.url())
        .expect("parse the database URL")
        .username(&role)
        .password(&password)
        .to_url_lossy()
        .to_string();
    let mut runtime = PgConnection::connect(&runtime_url)
        .await
        .expect("connect as the runtime role");
    sqlx::query(&format!("CREATE TABLE \"{schema}\".probe (x int)"))
        .execute(&mut runtime)
        .await
        .expect_err("the runtime role cannot run DDL");
    drop(runtime);

    let store: std::sync::Arc<dyn lash_core::LiveReplayStore> = std::sync::Arc::new(
        PostgresLiveReplayStore::connect(&endpoints(&runtime_url), &verify_only(&schema))
            .await
            .expect("a host-provisioned schema connects verify-only"),
    );
    let session = lash_sansio::SessionId::from("provisioned");
    let before = store.current_cursor(&session, lash_core::SessionRevision::new(1));
    let first = publish(&store, &session, "k#0", "first").await;
    match store.replay_after_cursor(&before).await.expect("replay") {
        lash_core::LiveReplayOutcome::Replayed(events) => assert_eq!(
            events.iter().map(|event| &event.cursor).collect::<Vec<_>>(),
            [&first.cursor],
            "the published event replays"
        ),
        lash_core::LiveReplayOutcome::Gap(reason) => panic!("unexpected gap {reason:?}"),
    }
    drop(store);
    execute(
        database.url(),
        &format!("DROP OWNED BY \"{role}\"; DROP ROLE \"{role}\";"),
    )
    .await;
}

/// `verify_only` refuses an unprovisioned schema by its missing tables and
/// a drifted one by its missing guard, and creates or repairs nothing.
#[tokio::test]
async fn verify_only_refuses_a_missing_or_drifted_schema_without_repair() {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    let findings = refusal(database.url(), verify_only(&schema)).await;
    assert_eq!(
        findings,
        [
            "live_replay_head",
            "live_replay_incarnation",
            "live_replay_log"
        ]
        .map(|table| PostgresLiveReplaySchemaFinding::MissingTable {
            table: table.to_string()
        })
        .to_vec()
    );
    let pool = PgPool::connect(database.url()).await.expect("pool");
    let created: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
            .bind(&schema)
            .fetch_one(&pool)
            .await
            .expect("read the catalog");
    assert!(!created, "verify_only created the schema");

    host_provision(database.url(), &schema).await;
    execute(
        database.url(),
        &format!("DROP INDEX \"{schema}\".live_replay_log_span"),
    )
    .await;
    let findings = refusal(database.url(), verify_only(&schema)).await;
    assert!(
        matches!(
            findings.as_slice(),
            [PostgresLiveReplaySchemaFinding::Missing { table, object }]
                if table == "live_replay_log"
                    && object.starts_with("unique (activity_first, activity_key, session_id)")
        ),
        "{findings:?}"
    );
    let report = PostgresLiveReplayStore::verify_schema(&pool, &schema)
        .await
        .expect("verify");
    assert_eq!(report.findings(), findings, "the guard stays dropped");
    pool.close().await;
}

/// `install` creates only what is absent: a table an older build left in
/// another shape refuses the connect instead of running beside it.
#[tokio::test]
async fn install_refuses_a_stale_table_without_repair() {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    host_provision(database.url(), &schema).await;
    execute(
        database.url(),
        &format!("ALTER TABLE \"{schema}\".live_replay_head DROP COLUMN touched_at"),
    )
    .await;
    let findings = refusal(database.url(), config(&schema)).await;
    assert_eq!(
        findings,
        [PostgresLiveReplaySchemaFinding::Missing {
            table: "live_replay_head".to_string(),
            object: "column touched_at timestamp with time zone not-null".to_string(),
        }]
    );
}
