//! The executable half of `runbooks/host-provisioned-schema/runbook.md`.
//!
//! An operator provisions the database out of band — applying the committed
//! `schema.sql` artifact through host tooling, not through lash's open — and the
//! runtime then opens with [`SchemaProvisioning::HostProvisioned`] under a role
//! that cannot run DDL at all. The three cases below are the runbook's claims
//! turned into assertions: prepare + verify + open succeeds, and both an
//! incomplete (missing seed row) and an incompatible (drifted shape) schema are
//! refused with the object named and nothing repaired behind the host's back.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::str::FromStr;

use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaProvisioning};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, Row};

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::ScratchSchema;

fn host_provisioned_config() -> PostgresStoreConfig {
    PostgresStoreConfig {
        schema_provisioning: SchemaProvisioning::HostProvisioned,
        schema_check: SchemaCheck::Enforce,
        ..PostgresStoreConfig::default()
    }
}

/// A runtime login role the way the runbook prescribes it: CONNECT on the
/// database, USAGE on the lash schema, and row-level privileges only. It cannot
/// create, alter, or drop anything, so a successful open under it is itself the
/// proof that open ran no DDL — and a failed open cannot have repaired anything.
struct RuntimeRole {
    name: String,
    password: String,
}

impl RuntimeRole {
    async fn create(admin: &mut PgConnection, schema: &str) -> Self {
        let role = Self {
            name: format!(
                "lash_rt_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..24]
            ),
            password: uuid::Uuid::new_v4().to_string(),
        };
        admin
            .execute(
                format!(
                    "CREATE ROLE \"{}\" LOGIN PASSWORD '{}'",
                    role.name, role.password
                )
                .as_str(),
            )
            .await
            .expect("create the runtime role");
        for grant in [
            format!("GRANT USAGE ON SCHEMA {schema} TO \"{}\"", role.name),
            format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} \
                 TO \"{}\"",
                role.name
            ),
            format!(
                "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO \"{}\"",
                role.name
            ),
        ] {
            admin
                .execute(grant.as_str())
                .await
                .expect("grant runtime privileges");
        }
        role
    }

    /// The application-owned pool: the host's database URL with only the
    /// identity swapped, connections pinned to the lash schema.
    async fn pool(&self, database_url: &str, schema: &str) -> PgPool {
        let options = PgConnectOptions::from_str(database_url)
            .expect("parse the host database URL")
            .username(&self.name)
            .password(&self.password);
        let search_path = schema.to_string();
        PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _meta| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path TO {search_path}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .expect("build the runtime pool")
    }

    async fn drop(self, admin: &mut PgConnection) {
        admin
            .execute(format!("DROP ROLE \"{}\"", self.name).as_str())
            .await
            .expect("drop the runtime role");
    }
}

/// Prepare → verify → open under a role with no schema-changing privileges.
///
/// This is the runbook's happy path end to end: host applies `schema.sql`, CI
/// gates on `verify_schema_for`, and the runtime opens HostProvisioned +
/// Enforce through a role for which DDL is not merely disabled but impossible.
#[tokio::test]
async fn a_host_provisioned_schema_opens_under_a_role_without_ddl_privileges() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping host-provisioned rollout: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;

    // The CI gate: the same structural check the open runs, against a bare
    // pool, before any binary is deployed.
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify a freshly provisioned schema");
    assert!(
        report.is_conformant(),
        "a schema provisioned from this build's schema.sql must verify: {report}"
    );

    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect role admin");
    let role = RuntimeRole::create(&mut admin, &scratch.name).await;
    let runtime_pool = role.pool(&database_url, &scratch.name).await;

    // The premise the whole runbook stands on: this role cannot repair
    // anything even if a bug asked it to.
    let ddl_error = sqlx::query("CREATE TABLE runtime_must_not_create_this (id int)")
        .execute(&runtime_pool)
        .await
        .expect_err("the runtime role must lack CREATE");
    assert!(
        ddl_error.to_string().contains("permission denied")
            || ddl_error.to_string().contains("insufficient_privilege"),
        "expected a privilege refusal, got: {ddl_error}"
    );

    let storage = PostgresStorage::from_pool_with(runtime_pool.clone(), host_provisioned_config())
        .await
        .expect("a conformant host-provisioned schema must open without DDL");

    // Open ran the verification itself; the pool the store holds can also read
    // and write rows — the only privileges the runtime actually needs.
    let open_report = storage.verify_schema().await.expect("verify via the store");
    assert!(open_report.is_conformant());
    let stamped: i64 = sqlx::query("SELECT count(*) FROM lash_schema_versions")
        .fetch_one(&runtime_pool)
        .await
        .expect("read the version stamp as the runtime role")
        .get(0);
    assert_eq!(stamped, 1);

    runtime_pool.close().await;
    // Drop order mirrors production teardown: the schema goes before the role,
    // so the role's grants disappear with it rather than blocking DROP ROLE.
    scratch.cleanup().await;
    role.drop(&mut admin).await;
    admin.close().await.expect("close role admin");
}

/// An incomplete schema — every table present, the required seed row missing —
/// is refused by name, and the refused open leaves the database exactly as the
/// host's migration left it: no row conjured, no repair attempted.
#[tokio::test]
async fn a_schema_missing_its_seed_row_is_refused_without_repair() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping host-provisioned rollout: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch.apply("DELETE FROM lash_await_event_meta").await;

    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("a schema without the await-event seed must not open")
        .to_string();
    assert!(
        error.contains("lash_await_event_meta") && error.contains("schema.sql"),
        "the refusal must name the missing seed and the artifact that owns it: {error}"
    );

    // No auto-repair: the seed row is still absent after the refused open, and
    // `verify_schema_for` still reports the version stamp the DDL wrote.
    let remaining: i64 = sqlx::query("SELECT count(*) FROM lash_await_event_meta")
        .fetch_one(&scratch.pool)
        .await
        .expect("re-read the seed table")
        .get(0);
    assert_eq!(
        remaining, 0,
        "a refused open must not seed what the host did not"
    );
    // The same gap is visible to the CI gate before deploy: the seed rows are
    // part of the verification, not only of the open.
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("the refused schema is still readable");
    assert!(
        !report.is_conformant()
            && report
                .findings
                .iter()
                .any(|finding| finding.to_string().contains("lash_await_event_meta")),
        "verify_schema_for must flag the missing seed row by name: {report}"
    );

    scratch.cleanup().await;
}

/// An incompatible schema — one lash table dropped after provisioning — is
/// refused under Enforce with the drifted object named, and stays dropped:
/// HostProvisioned never re-runs the DDL the host owns.
#[tokio::test]
async fn a_drifted_schema_is_refused_without_repair() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping host-provisioned rollout: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    // Dependents' foreign keys go with it — drift, not an empty diff.
    scratch.apply("DROP TABLE lash_processes CASCADE").await;

    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("a drifted schema must not open")
        .to_string();
    assert!(
        error.contains("lash_processes"),
        "the refusal must name the missing table: {error}"
    );

    let still_missing: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (
             SELECT 1 FROM pg_catalog.pg_class AS class
             JOIN pg_catalog.pg_namespace AS namespace
               ON namespace.oid = class.relnamespace
             WHERE namespace.nspname = current_schema()
               AND class.relname = 'lash_processes'
         )",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("re-check the dropped table");
    assert!(
        still_missing,
        "a refused open must not recreate the table the host dropped"
    );

    scratch.cleanup().await;
}
