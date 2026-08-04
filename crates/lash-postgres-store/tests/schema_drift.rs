//! Proofs that the structural schema check catches the drift classes it exists
//! for, and tolerates the schema shapes a host legitimately produces.
//!
//! Every case runs through the public open path against a throwaway PostgreSQL
//! schema, so what is asserted is what a host actually experiences: a mis-ported
//! vendored schema fails at `PostgresStorage::from_pool_with` with the drifted
//! object named, and an equivalent schema built by `ALTER` — different column
//! order, host-chosen constraint names, identity instead of `BIGSERIAL` — opens
//! clean.

use lash_postgres_store::{
    ForeignKeyAction, PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaFinding,
    SchemaProvisioning,
};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection};

#[allow(dead_code)]
mod support;

use support::database_url;

/// A throwaway PostgreSQL schema, provisioned from the committed DDL artifact.
struct ScratchSchema {
    name: String,
    pool: PgPool,
    database_url: String,
}

impl ScratchSchema {
    /// Creates the schema and provisions it exactly as a host would: by applying
    /// [`PostgresStorage::schema_ddl`], not by letting lash open into it.
    async fn provision(database_url: &str) -> Self {
        let name = format!("lash_drift_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(database_url)
            .await
            .expect("connect scratch admin");
        admin
            .execute(format!("CREATE SCHEMA {name}").as_str())
            .await
            .expect("create scratch schema");
        admin
            .execute(format!("SET search_path TO {name}").as_str())
            .await
            .expect("point admin search_path at the scratch schema");
        sqlx::raw_sql(PostgresStorage::schema_ddl())
            .execute(&mut admin)
            .await
            .expect("apply the committed DDL artifact");
        admin.close().await.expect("close scratch admin");
        let scratch = name.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _meta| {
                let scratch = scratch.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path TO {scratch}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(database_url)
            .await
            .expect("build scratch pool");
        Self {
            name,
            pool,
            database_url: database_url.to_string(),
        }
    }

    /// Runs host DDL or DML against the scratch schema.
    async fn apply(&self, statements: &str) {
        sqlx::raw_sql(statements)
            .execute(&self.pool)
            .await
            .unwrap_or_else(|error| panic!("apply scratch mutation: {error}"));
    }

    /// Opens the store the way a host with its own migrations does: no DDL, hard
    /// failure on drift.
    async fn open_host_provisioned(
        &self,
        check: SchemaCheck,
    ) -> Result<PostgresStorage, lash_core::StoreError> {
        PostgresStorage::from_pool_with(
            self.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: SchemaProvisioning::HostProvisioned,
                schema_check: check,
                ..PostgresStoreConfig::default()
            },
        )
        .await
    }

    /// Drops the schema. Called explicitly so a failing assertion leaves the
    /// schema behind for inspection.
    async fn cleanup(self) {
        let name = self.name;
        self.pool.close().await;
        let mut admin = PgConnection::connect(&self.database_url)
            .await
            .expect("connect scratch cleanup");
        admin
            .execute(format!("DROP SCHEMA {name} CASCADE").as_str())
            .await
            .expect("drop scratch schema");
    }
}

/// Applies one mutation to a freshly provisioned schema and asserts the check
/// rejects the open, names the drifted object, and reports the expected finding —
/// while `SchemaCheck::WarnOnly` opens the very same database.
async fn assert_mutation_is_rejected(
    mutation: &str,
    expected_message_fragments: &[&str],
    finding_matches: impl Fn(&SchemaFinding) -> bool,
) {
    let Some(database_url) = database_url() else {
        eprintln!("skipping schema drift mutation: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch.apply(mutation).await;

    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .unwrap_or_else(|| panic!("a drifted schema must not open: {mutation}"));
    let rendered = error.to_string();
    for fragment in expected_message_fragments {
        assert!(
            rendered.contains(fragment),
            "the open error must name the drifted object; missing {fragment:?} after \
             `{mutation}` in:\n{rendered}"
        );
    }

    // The same drift is reachable as structured findings, which is what a host
    // gates its migration CI on.
    let warned = scratch
        .open_host_provisioned(SchemaCheck::WarnOnly)
        .await
        .unwrap_or_else(|error| {
            panic!("SchemaCheck::WarnOnly must open the drifted schema: {error}")
        });
    let report = warned.verify_schema().await.expect("verify drifted schema");
    assert!(
        !report.is_conformant(),
        "verify_schema must report the drift it warned about: {report}"
    );
    assert!(
        report.findings.iter().any(finding_matches),
        "verify_schema must report the expected finding after `{mutation}`, got {:?}",
        report.findings
    );
    scratch.cleanup().await;
}

/// The motivating case. `idx_lash_process_events_key` is a partial unique
/// *index*, so it has no `pg_constraint` row: a constraints-only check would
/// never see it go missing, and exactly-once process-event dedup would degrade
/// silently with no query ever failing.
#[tokio::test]
async fn a_dropped_partial_unique_dedup_index_is_rejected() {
    assert_mutation_is_rejected(
        "DROP INDEX idx_lash_process_events_key",
        &[
            "UNIQUE GUARD DRIFT",
            "lash_process_events: missing unique (process_id, idempotency_key) where \
             idempotency_key is not null",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingUniqueGuard { table, expected }
                    if table == "lash_process_events"
                        && expected.predicate.as_deref() == Some("idempotency_key is not null")
            )
        },
    )
    .await;
}

/// A retyped column: `text` widened to `character varying(64)` truncates values
/// lash writes, and nothing about the version stamp would notice.
#[tokio::test]
async fn a_retyped_column_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_processes ALTER COLUMN status TYPE VARCHAR(64)",
        &[
            "COLUMN DRIFT",
            "lash_processes.status: expected text not-null, found character varying(64) not-null",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, expected, found }
                    if table == "lash_processes"
                        && expected.name == "status"
                        && found.sql_type == "character varying(64)"
            )
        },
    )
    .await;
}

/// A column whose NOT NULL was dropped. lash relies on the constraint rather than
/// re-checking every read.
#[tokio::test]
async fn a_widened_nullability_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_sessions ALTER COLUMN head_json DROP NOT NULL",
        &[
            "COLUMN DRIFT",
            "lash_sessions.head_json: expected text not-null, found text nullable",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, expected, .. }
                    if table == "lash_sessions" && expected.name == "head_json"
            )
        },
    )
    .await;
}

/// A `BIGSERIAL` ported as a bare `BIGINT`. Inserts omit the column and rely on
/// the sequence default, so without the auto-generated-value flag in scope this
/// drift would pass the check and fail at the first append.
#[tokio::test]
async fn a_lost_sequence_default_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_usage_deltas ALTER COLUMN seq DROP DEFAULT",
        &[
            "COLUMN DRIFT",
            "lash_usage_deltas.seq: expected bigint not-null auto-generated, found bigint not-null",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, expected, found }
                    if table == "lash_usage_deltas"
                        && expected.name == "seq"
                        && expected.auto_generated
                        && !found.auto_generated
            )
        },
    )
    .await;
}

/// A foreign key ported without its cascade. Process pruning deletes the parent
/// row and expects the children to follow; without the cascade the delete either
/// errors or, for a key ported as `NO ACTION` with deferred children, leaves
/// orphan rows.
#[tokio::test]
async fn a_foreign_key_missing_its_cascade_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_process_events
             DROP CONSTRAINT lash_process_events_process_id_fkey;
         ALTER TABLE lash_process_events
             ADD CONSTRAINT lash_process_events_process_id_fkey
             FOREIGN KEY (process_id) REFERENCES lash_processes(process_id)",
        &[
            "FOREIGN KEY DRIFT",
            "lash_process_events: expected foreign key (process_id) references lash_processes \
             (process_id) on delete cascade, found (process_id) references lash_processes \
             (process_id) on delete no action",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ForeignKeyMismatch { table, found, .. }
                    if table == "lash_process_events"
                        && found.on_delete == ForeignKeyAction::NoAction
            )
        },
    )
    .await;
}

/// A foreign key dropped outright.
#[tokio::test]
async fn a_dropped_foreign_key_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_process_leases DROP CONSTRAINT lash_process_leases_process_id_fkey",
        &[
            "FOREIGN KEY DRIFT",
            "lash_process_leases: missing foreign key (process_id) references lash_processes",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingForeignKey { table, .. } if table == "lash_process_leases"
            )
        },
    )
    .await;
}

/// A missing seed row. No structural comparison can see this, and every
/// process-registry write depends on the clock row existing.
#[tokio::test]
async fn a_missing_seed_row_is_rejected() {
    assert_mutation_is_rejected(
        "DELETE FROM lash_process_change_clock",
        &[
            "SEED ROWS",
            "lash_process_change_clock: seed row is missing (transactional process-change clock)",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingSeedRow { table, .. }
                    if table == "lash_process_change_clock"
            )
        },
    )
    .await;
}

/// A dropped table.
#[tokio::test]
async fn a_missing_table_is_rejected() {
    assert_mutation_is_rejected(
        "DROP TABLE lash_process_observers",
        &["MISSING TABLES", "lash_process_observers: table is missing"],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingTable { table } if table == "lash_process_observers"
            )
        },
    )
    .await;
}

/// A column lash does not know about. lash owns these tables by contract, and a
/// host column that is `NOT NULL` without a default breaks every insert.
#[tokio::test]
async fn an_unexpected_column_on_a_lash_table_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_blobs ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
        &["COLUMN DRIFT", "lash_blobs.tenant_id: unexpected column"],
        |finding| {
            matches!(
                finding,
                SchemaFinding::UnexpectedColumn { table, found }
                    if table == "lash_blobs" && found.name == "tenant_id"
            )
        },
    )
    .await;
}

/// The await-event signing secret is a data precondition rather than a shape, so
/// `SchemaCheck::WarnOnly` cannot relax it: without the row there is no secret to
/// authenticate promises with.
#[tokio::test]
async fn a_missing_signing_secret_is_fatal_in_every_mode() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping signing-secret precondition: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch.apply("DELETE FROM lash_await_event_meta").await;
    for check in [SchemaCheck::Enforce, SchemaCheck::WarnOnly] {
        let error = scratch
            .open_host_provisioned(check)
            .await
            .err()
            .unwrap_or_else(|| panic!("{check:?} must not open without a signing secret"));
        assert!(
            error.to_string().contains("lash_await_event_meta"),
            "the error must name the table carrying the secret: {error}"
        );
    }
    scratch.cleanup().await;
}

/// A version stamp naming another generation short-circuits the structural diff:
/// the database is a different schema generation, so a per-column diff of it
/// would be noise rather than a diagnosis.
#[tokio::test]
async fn a_mismatched_version_stamp_is_rejected_without_a_column_diff() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping version stamp check: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 1 WHERE component = 'lash-postgres-store'",
        )
        .await;
    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("a mismatched version stamp must not open");
    let rendered = error.to_string();
    assert!(
        rendered.contains("COMPONENT VERSION")
            && rendered.contains("is stamped version 1")
            && rendered.contains(&format!("expected {}", PostgresStorage::schema_version())),
        "the error must name the version mismatch: {rendered}"
    );
    assert!(
        !rendered.contains("COLUMN DRIFT"),
        "a version mismatch must not emit a column diff: {rendered}"
    );
    scratch.cleanup().await;
}

/// The tolerance the target host depends on. A schema composed by `ALTER` — the
/// shape a migration tool produces when it edits an existing baseline — differs
/// from a fresh `CREATE` in column order, in `attnum` gaps left by dropped
/// columns, and in constraint names. None of that is semantic, and none of it may
/// fail the check. Host additions outside lash's tables are equally tolerated.
#[tokio::test]
async fn an_alter_built_equivalent_schema_opens_clean() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping ALTER-built equivalence: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            // Different physical column order, plus an attnum gap from the drop.
            // Dropping the column takes its index with it; the host rebuilds it
            // under its own name.
            "ALTER TABLE lash_sessions DROP COLUMN leaf_node_id;
             ALTER TABLE lash_sessions ADD COLUMN leaf_node_id TEXT;
             CREATE INDEX host_named_leaf_index ON lash_sessions(leaf_node_id);

             -- Host-chosen constraint names over the same columns.
             ALTER TABLE lash_trigger_subscriptions
                 DROP CONSTRAINT lash_trigger_subscriptions_owner_scope_subscription_key_key;
             ALTER TABLE lash_trigger_subscriptions
                 ADD CONSTRAINT host_named_subscription_key
                 UNIQUE (owner_scope, subscription_key);
             ALTER TABLE lash_process_observers
                 DROP CONSTRAINT lash_process_observers_process_id_fkey;
             ALTER TABLE lash_process_observers
                 ADD CONSTRAINT host_named_observer_process
                 FOREIGN KEY (process_id) REFERENCES lash_processes(process_id)
                 ON DELETE CASCADE;

             -- The dedup guard rebuilt as a constraint-free index with another
             -- name and the predicate written differently.
             DROP INDEX idx_lash_process_events_key;
             CREATE UNIQUE INDEX host_named_event_dedup
                 ON lash_process_events(process_id, idempotency_key)
                 WHERE (idempotency_key IS NOT NULL);

             -- BIGSERIAL modernized to an identity column: identical insert
             -- semantics for lash, which never supplies the value.
             ALTER TABLE lash_usage_deltas ALTER COLUMN seq DROP DEFAULT;
             ALTER TABLE lash_usage_deltas
                 ALTER COLUMN seq ADD GENERATED BY DEFAULT AS IDENTITY;

             -- Host objects lash has no standing to veto.
             CREATE TABLE host_owned_table (id TEXT PRIMARY KEY);
             CREATE INDEX host_extra_index ON lash_processes(updated_at_ms);",
        )
        .await;

    let storage = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .unwrap_or_else(|error| {
            panic!("an ALTER-built equivalent schema must open clean: {error}")
        });
    let report = storage
        .verify_schema()
        .await
        .expect("verify the ALTER-built schema");
    assert!(
        report.is_conformant(),
        "an ALTER-built equivalent schema must verify clean: {report}"
    );
    assert_eq!(
        report.schema.as_deref(),
        Some(scratch.name.as_str()),
        "the report must name the schema the tables actually resolved in"
    );
    scratch.cleanup().await;
}

/// Rewrites a connection URL's credentials, so a test can connect as a role other
/// than the one the suite is configured with.
fn with_credentials(database_url: &str, role: &str, password: &str) -> String {
    let (scheme, rest) = database_url
        .split_once("://")
        .expect("a Postgres URL carries a scheme");
    let host_and_path = rest.rsplit_once('@').map_or(rest, |(_, tail)| tail);
    format!("{scheme}://{role}:{password}@{host_and_path}")
}

/// `SchemaProvisioning::HostProvisioned` must run no DDL at all — the point of the
/// mode is that a host under restricted grants can open lash. Proven against a
/// purpose-made role that holds nothing but `USAGE` on the schema and `SELECT` on
/// its tables: any mode that executed even one `CREATE TABLE IF NOT EXISTS`, or
/// that wrote a version stamp or a seed row, fails here.
#[tokio::test]
async fn host_provisioned_mode_needs_no_ddl_privilege() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping host-provisioned no-DDL proof: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let role = format!("lash_reader_{}", uuid::Uuid::new_v4().simple());
    scratch
        .apply(&format!(
            "CREATE ROLE {role} LOGIN PASSWORD 'reader';
             GRANT USAGE ON SCHEMA {schema} TO {role};
             GRANT SELECT ON ALL TABLES IN SCHEMA {schema} TO {role};",
            schema = scratch.name
        ))
        .await;

    let reader_url = with_credentials(&database_url, &role, "reader");
    let schema = scratch.name.clone();
    let reader_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _meta| {
            let schema = schema.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path TO {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&reader_url)
        .await
        .expect("connect as the read-only role");
    let storage = PostgresStorage::from_pool_with(
        reader_pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .unwrap_or_else(|error| {
        panic!("host-provisioned open must need no privilege beyond SELECT: {error}")
    });
    assert!(
        storage
            .verify_schema()
            .await
            .expect("verify host-provisioned schema")
            .is_conformant()
    );

    // The default mode still provisions, which is what every existing caller
    // relies on — and it is exactly what this role cannot do.
    let denied =
        PostgresStorage::from_pool_with(reader_pool.clone(), PostgresStoreConfig::default())
            .await
            .err()
            .expect("lash-managed provisioning must need DDL privilege the role lacks");
    assert!(
        denied.to_string().contains("permission denied"),
        "the default mode must fail on the missing DDL privilege: {denied}"
    );
    reader_pool.close().await;

    scratch
        .apply(&format!("DROP OWNED BY {role}; DROP ROLE {role};",))
        .await;
    scratch.cleanup().await;
}

/// An unprovisioned database in host-provisioned mode is a configuration error,
/// and the message must say so rather than emit a diff of every missing table.
#[tokio::test]
async fn host_provisioned_mode_rejects_an_unprovisioned_database() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping unprovisioned rejection: database URL is not set");
        return;
    };
    let name = format!("lash_empty_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect empty-schema admin");
    admin
        .execute(format!("CREATE SCHEMA {name}").as_str())
        .await
        .expect("create empty schema");
    let scratch = name.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _meta| {
            let scratch = scratch.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path TO {scratch}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("build empty-schema pool");
    let error = PostgresStorage::from_pool_with(
        pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .err()
    .expect("an unprovisioned database must not open in host-provisioned mode");
    assert!(
        error.to_string().contains("has no version stamp"),
        "the error must say the database is unprovisioned: {error}"
    );
    pool.close().await;
    admin
        .execute(format!("DROP SCHEMA {name} CASCADE").as_str())
        .await
        .expect("drop empty schema");
}
