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
    ColumnValueSource, ForeignKeyAction, PostgresStorage, PostgresStoreConfig, SchemaCheck,
    SchemaFinding, SchemaProvisioning,
};
use lash_sansio::sync::MutexExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection};

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::{
    REWIND_PAST_54_ARTIFACTS, REWIND_PAST_55_ARTIFACTS, ScratchSchema, assert_mutation_is_rejected,
    pool_with_search_path, postgres_server_version_num,
};

/// Append-request replay depends on one durable receipt per session and turn.
/// Dropping the receipt table's primary key would silently admit conflicting
/// identities for the same append operation.
#[tokio::test]
async fn a_dropped_append_receipt_unique_guard_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_runtime_turn_commits
             DROP CONSTRAINT lash_runtime_turn_commits_pkey",
        &[
            "UNIQUE GUARD DRIFT",
            "lash_runtime_turn_commits: missing primary key (session_id, turn_id)",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingUniqueGuard { table, expected }
                    if table == "lash_runtime_turn_commits"
                        && expected.primary_key
                        && expected.columns == ["session_id", "turn_id"]
            )
        },
    )
    .await;
}

/// The five-part usage identity guard is what makes usage publication
/// exactly-once across receipt replay: without it, a re-staged delta at a
/// reused ordinal inserts as a duplicate row and accounting silently double
/// counts, with no query ever failing.
#[tokio::test]
async fn a_dropped_usage_identity_unique_guard_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_usage_deltas
             DROP CONSTRAINT lash_usage_deltas_session_id_operation_storage_key_entry_or_key",
        &[
            "UNIQUE GUARD DRIFT",
            "lash_usage_deltas: missing unique (session_id, operation_storage_key, \
             entry_ordinal, payload_encoding_version, payload_hash)",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::MissingUniqueGuard { table, expected }
                    if table == "lash_usage_deltas"
                        && !expected.primary_key
                        && expected.columns
                            == [
                                "session_id",
                                "operation_storage_key",
                                "entry_ordinal",
                                "payload_encoding_version",
                                "payload_hash",
                            ]
            )
        },
    )
    .await;
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
/// the sequence default, so without the column's value source in scope this drift
/// would pass the check and fail at the first append.
#[tokio::test]
async fn a_lost_sequence_default_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_usage_deltas ALTER COLUMN seq DROP DEFAULT",
        &[
            "COLUMN DRIFT",
            "lash_usage_deltas.seq: expected bigint not-null default, found bigint not-null",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, expected, found }
                    if table == "lash_usage_deltas"
                        && expected.name == "seq"
                        && expected.value_source == ColumnValueSource::Default
                        && found.value_source == ColumnValueSource::Supplied
            )
        },
    )
    .await;
}

/// `BIGSERIAL` "modernized" to `GENERATED ALWAYS AS IDENTITY` — the variant
/// PostgreSQL's own docs steer a host toward, and the one that looks stricter.
/// It supplies a value, so a single has-auto-generated-value bit accepts it, but
/// it *rejects* the explicit value `enqueue_queued_work` supplies: lash takes
/// `nextval` itself and names `enqueue_seq` in the insert because it needs the
/// value to derive `batch_id`. Every enqueue — so every process wake and every
/// background session command — would fail after a clean open.
#[tokio::test]
async fn an_identity_always_column_lash_writes_explicitly_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_queued_work_batches ALTER COLUMN enqueue_seq DROP DEFAULT;
         ALTER TABLE lash_queued_work_batches
             ALTER COLUMN enqueue_seq ADD GENERATED ALWAYS AS IDENTITY",
        &[
            "COLUMN DRIFT",
            "lash_queued_work_batches.enqueue_seq: expected bigint not-null default, found \
             bigint not-null identity-always",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, found, .. }
                    if table == "lash_queued_work_batches"
                        && found.value_source == ColumnValueSource::IdentityAlways
            )
        },
    )
    .await;
}

/// A defaulted column rebuilt as a stored generated column. Same root cause as
/// the identity-always case from the other direction: it supplies a value and
/// rejects every explicit one, and lash writes `head_revision` explicitly on every
/// session-head commit.
#[tokio::test]
async fn a_stored_generated_column_lash_writes_explicitly_is_rejected() {
    assert_mutation_is_rejected(
        "ALTER TABLE lash_sessions DROP COLUMN head_revision;
         ALTER TABLE lash_sessions
             ADD COLUMN head_revision BIGINT NOT NULL
             GENERATED ALWAYS AS (length(head_json)::bigint) STORED",
        &[
            "COLUMN DRIFT",
            "lash_sessions.head_revision: expected bigint not-null default, found bigint \
             not-null generated",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::ColumnMismatch { table, found, .. }
                    if table == "lash_sessions"
                        && found.value_source == ColumnValueSource::Generated
            )
        },
    )
    .await;
}

/// `UNIQUE NULLS NOT DISTINCT` over the same columns guards a different row set.
/// `source_key` is nullable and lash writes `NULL` for every batch without a
/// source key, so under this rebuild the second such batch in a session is
/// rejected as a duplicate. Same columns, same kind, same predicate — only the
/// null-distinctness differs, which is why it has to be in the fingerprint.
#[tokio::test]
async fn a_nulls_not_distinct_rebuild_of_a_nullable_guard_is_rejected() {
    if database_url().is_none() {
        eprintln!("skipping NULLS NOT DISTINCT drift: database URL is not set");
        return;
    }
    if postgres_server_version_num().await < 150_000 {
        eprintln!("skipping NULLS NOT DISTINCT drift: needs PostgreSQL 15 or newer");
        return;
    }
    assert_mutation_is_rejected(
        "ALTER TABLE lash_queued_work_batches
             DROP CONSTRAINT lash_queued_work_batches_session_id_source_key_key;
         CREATE UNIQUE INDEX host_source_key_guard
             ON lash_queued_work_batches (session_id, source_key) NULLS NOT DISTINCT",
        &[
            "UNIQUE GUARD DRIFT",
            "lash_queued_work_batches: expected unique (session_id, source_key), found unique \
             (session_id, source_key) nulls not distinct",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::UniqueGuardMismatch { table, found, .. }
                    if table == "lash_queued_work_batches" && found.nulls_not_distinct
            )
        },
    )
    .await;
}

/// A guard whose key columns are in another order enforces exactly the same rule:
/// `UNIQUE (a, b)` and `UNIQUE (b, a)` reject the same rows. A host that rebuilt
/// the dedup index the other way round has not drifted, so it must open clean.
/// Column order changes which index prefixes can be scanned, which is an
/// access-path property — the same class this check leaves out when it ignores
/// non-unique indexes entirely.
#[tokio::test]
async fn a_reordered_unique_guard_opens_clean() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping reordered-guard tolerance: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "DROP INDEX idx_lash_process_events_key;
             CREATE UNIQUE INDEX host_reordered_dedup
                 ON lash_process_events(idempotency_key, process_id)
                 WHERE idempotency_key IS NOT NULL",
        )
        .await;
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the reordered-guard database");
    assert!(
        report.is_conformant(),
        "a reordered key over the same column set enforces the same rule: {report}"
    );
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("a reordered guard must not block an open");
    scratch.cleanup().await;
}

/// The tolerance above must not swallow a guard that covers a *different* row set.
/// Same kind, same column set, different partial predicate — which is exactly the
/// pair that set-based matching brings together, so it is exactly where a
/// too-permissive comparison would go silent.
#[tokio::test]
async fn a_same_column_set_guard_with_another_predicate_is_rejected() {
    assert_mutation_is_rejected(
        "DROP INDEX idx_lash_process_events_key;
         CREATE UNIQUE INDEX host_narrowed_dedup
             ON lash_process_events(process_id, idempotency_key)
             WHERE idempotency_key <> ''",
        &[
            "UNIQUE GUARD DRIFT",
            "lash_process_events: expected unique (process_id, idempotency_key) where \
             idempotency_key is not null, found unique (process_id, idempotency_key) where \
             idempotency_key <> ''::text",
        ],
        |finding| {
            matches!(
                finding,
                SchemaFinding::UniqueGuardMismatch { table, found, .. }
                    if table == "lash_process_events"
                        && found.predicate.as_deref() != Some("idempotency_key is not null")
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
/// the database is a different schema generation, so a per-column diff of it would
/// be noise rather than a diagnosis. The report carries the version finding alone.
#[tokio::test]
async fn a_mismatched_version_stamp_is_reported_without_a_column_diff() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping version stamp check: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 1 WHERE component = 'lash-postgres-store';
             ALTER TABLE lash_processes ALTER COLUMN status TYPE VARCHAR(64)",
        )
        .await;
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the stale-version database");
    assert_eq!(
        report.findings,
        vec![SchemaFinding::VersionMismatch {
            expected: PostgresStorage::schema_version(),
            found: Some(1),
        }],
        "a version mismatch must suppress the structural diff entirely"
    );
    let rendered = report.to_string();
    assert!(
        rendered.contains("COMPONENT VERSION") && rendered.contains("is stamped version 1"),
        "the report must name the version mismatch: {rendered}"
    );
    assert!(
        !rendered.contains("COLUMN DRIFT"),
        "a version mismatch must not emit a column diff even when one exists: {rendered}"
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

             -- BIGSERIAL modernized to GENERATED BY DEFAULT AS IDENTITY. That
             -- variant keeps both capabilities the original had: it supplies a
             -- value when an insert omits the column and accepts one when an
             -- insert names it. GENERATED ALWAYS would drop the second, which
             -- `an_identity_always_column_lash_writes_explicitly_is_rejected`
             -- pins as a mismatch — lash does name some of these columns
             -- explicitly (`enqueue_seq`), so the tolerance is about capability,
             -- not about lash never supplying a value.
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

/// The verifier must describe one installation, not assemble a conformant-looking
/// one out of two partial ones. With `search_path = front, back` and a table
/// missing from `front`, resolving each table independently would take the rest
/// from `front` and the missing one from `back`: every object individually has the
/// expected shape, the foreign key still renders unqualified, and the check would
/// pass — while runtime's unqualified process insert lands in `front` and its event
/// insert lands in `back`, whose foreign key targets `back`. Anchoring on the
/// namespace that holds `lash_schema_versions` makes the absent table simply
/// missing.
#[tokio::test]
async fn a_partial_installation_fronting_a_complete_one_is_rejected() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping mixed-schema rejection: database URL is not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let front = format!("lash_front_{suffix}");
    let back = format!("lash_back_{suffix}");
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect mixed-schema admin");
    for schema in [&front, &back] {
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin
            .execute(format!("SET search_path TO {schema}").as_str())
            .await
            .expect("point admin search_path");
        sqlx::raw_sql(PostgresStorage::schema_ddl())
            .execute(&mut admin)
            .await
            .expect("provision schema");
    }
    // `front` is the interrupted migration: everything but the event table.
    admin
        .execute(format!("DROP TABLE {front}.lash_process_events CASCADE").as_str())
        .await
        .expect("remove the event table from the fronting installation");

    let pool = pool_with_search_path(&database_url, &format!("{front}, {back}")).await;
    let report = PostgresStorage::verify_schema_for(&pool)
        .await
        .expect("verify the mixed installation");
    assert!(
        !report.is_conformant(),
        "two partial installations on one search_path must never verify clean: {report}"
    );
    assert_eq!(
        report.schema.as_deref(),
        Some(front.as_str()),
        "verification must anchor on the namespace holding lash_schema_versions"
    );
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::MissingTable { table } if table == "lash_process_events"
        )),
        "the table absent from the anchored namespace must be missing, not borrowed from the \
         next schema on the search_path: {:?}",
        report.findings
    );
    let error = PostgresStorage::from_pool_with(
        pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .err()
    .expect("a mixed installation must not open");
    assert!(error.to_string().contains("lash_process_events"));
    pool.close().await;
    for schema in [&front, &back] {
        admin
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .expect("drop schema");
    }
}

/// The mirror image: the anchored installation is complete, but a lash-named table
/// earlier on the `search_path` shadows it, so lash's own unqualified statements
/// would write somewhere the check never looked.
#[tokio::test]
async fn a_lash_table_shadowing_the_anchored_installation_is_reported() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping shadowed-table rejection: database URL is not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let front = format!("lash_shadow_{suffix}");
    let back = format!("lash_anchor_{suffix}");
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect shadow admin");
    admin
        .execute(format!("CREATE SCHEMA {back}; CREATE SCHEMA {front}").as_str())
        .await
        .expect("create schemas");
    admin
        .execute(format!("SET search_path TO {back}").as_str())
        .await
        .expect("point admin search_path");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut admin)
        .await
        .expect("provision the anchored installation");
    // A lone leftover table in a schema that comes first. The anchor stays `back`
    // because only it holds `lash_schema_versions`.
    admin
        .execute(
            format!("CREATE TABLE {front}.lash_processes (LIKE {back}.lash_processes)").as_str(),
        )
        .await
        .expect("create the shadowing table");

    let pool = pool_with_search_path(&database_url, &format!("{front}, {back}")).await;
    let report = PostgresStorage::verify_schema_for(&pool)
        .await
        .expect("verify the shadowed installation");
    assert_eq!(report.schema.as_deref(), Some(back.as_str()));
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::ShadowedTable { table, found_schema, .. }
                if table == "lash_processes" && found_schema == &front
        )),
        "a lash table resolving outside the anchored namespace must be reported: {:?}",
        report.findings
    );
    let error = PostgresStorage::from_pool_with(
        pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .err()
    .expect("a shadowed installation must not open");
    assert!(error.to_string().contains("SHADOWED TABLES"), "{error}");
    pool.close().await;
    for schema in [&front, &back] {
        admin
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .expect("drop schema");
    }
}

/// The component version is the reject-and-recreate boundary and no `SchemaCheck`
/// relaxes it. If `WarnOnly` could downgrade it, a host that adopted the valve for
/// a structural false positive would later open silently against a pre-cutover
/// database — process events with no completion-authority payload, manifest rows
/// naming a blob layout that cannot be read — which is the corruption the boundary
/// exists to prevent.
#[tokio::test]
async fn a_stale_version_stamp_is_fatal_in_every_mode() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping version-gate universality: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 1 WHERE component = 'lash-postgres-store'",
        )
        .await;
    for provisioning in [
        SchemaProvisioning::HostProvisioned,
        SchemaProvisioning::LashManaged,
    ] {
        for check in [SchemaCheck::Enforce, SchemaCheck::WarnOnly] {
            let error = PostgresStorage::from_pool_with(
                scratch.pool.clone(),
                PostgresStoreConfig {
                    schema_provisioning: provisioning,
                    schema_check: check,
                    ..PostgresStoreConfig::default()
                },
            )
            .await
            .err()
            .unwrap_or_else(|| {
                panic!("{provisioning:?} + {check:?} must reject a stale version stamp")
            });
            let rendered = error.to_string();
            assert!(
                rendered.contains("has version 1")
                    && rendered.contains("reject-and-recreate")
                    && rendered.contains("does not relax it"),
                "{provisioning:?} + {check:?} must name the boundary and say the valve does not \
                 relax it: {rendered}"
            );
        }
    }
    scratch.cleanup().await;
}

/// A pre-cutover queued-work table stamped with the old component version must
/// be refused before creation-only DDL or `WarnOnly` can admit it. This models
/// an existing installation crossing the queued-work shape cutover, rather
/// than merely proving that a fresh schema matches this build.
#[tokio::test]
async fn pre_queued_work_cutover_install_is_refused_even_under_warn_only() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping queued-work version crossing: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "ALTER TABLE lash_queued_work_batches
                 DROP COLUMN work_kind,
                 DROP COLUMN authority_json,
                 DROP COLUMN merge_key;
             ALTER TABLE lash_queued_work_batches
                 ADD COLUMN slot_policy TEXT NOT NULL DEFAULT 'join',
                 ADD COLUMN merge_key_json TEXT NOT NULL DEFAULT '\"never\"';
             UPDATE lash_schema_versions
                SET version = 43
              WHERE component = 'lash-postgres-store'",
        )
        .await;

    for provisioning in [
        SchemaProvisioning::HostProvisioned,
        SchemaProvisioning::LashManaged,
    ] {
        let error = PostgresStorage::from_pool_with(
            scratch.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: provisioning,
                schema_check: SchemaCheck::WarnOnly,
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .err()
        .unwrap_or_else(|| {
            panic!("{provisioning:?} + WarnOnly must refuse the pre-cutover install")
        });
        let rendered = error.to_string();
        assert!(
            rendered.contains("has version 43")
                && rendered.contains("expected 55")
                && rendered.contains("does not relax it"),
            "the version boundary must dominate the incompatible queued-work shape: {rendered}"
        );
    }
    scratch.cleanup().await;
}

/// Main's published component-50 shape upgrades through the explicit 50 -> 55
/// migration before the creation-only target DDL is evaluated.
#[tokio::test]
async fn main_component_50_store_upgrades_cleanly_to_55() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-50 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             DROP TABLE lash_attachment_condemnations;
             DROP TABLE lash_tool_intent_submissions;
             DROP TABLE lash_process_parent_end_plans;
             UPDATE lash_schema_versions
                SET version = 50
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-50 shape migrates to 55");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 55);
    for table in [
        "lash_attachment_condemnations",
        "lash_process_parent_end_plans",
        "lash_tool_intent_submissions",
        "lash_runtime_effect_group",
    ] {
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
                .bind(table)
                .fetch_one(&scratch.pool)
                .await
                .expect("read migrated table");
        assert!(present, "migration omitted {table}");
    }
    scratch.cleanup().await;
}

/// The published component-51 shape upgrades through the explicit 51 -> 55
/// migration that introduces the attachment GC fence's condemnation table.
#[tokio::test]
async fn main_component_51_store_upgrades_cleanly_to_55() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-51 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             DROP TABLE lash_attachment_condemnations;
             UPDATE lash_schema_versions
                SET version = 51
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-51 shape migrates to 55");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 55);
    for table in ["lash_attachment_condemnations", "lash_runtime_effect_group"] {
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
                .bind(table)
                .fetch_one(&scratch.pool)
                .await
                .expect("read migrated table");
        assert!(present, "migration omitted {table}");
    }
    scratch.cleanup().await;
}

/// The published component-52 shape upgrades through the explicit 52 -> 55
/// migration, which adds the two idle-arbitration ordering indexes on top of the
/// whole effect-group journal.
#[tokio::test]
async fn main_component_52_store_upgrades_cleanly_to_55() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-52 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             UPDATE lash_schema_versions
                SET version = 52
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-52 shape migrates to 55");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 55);
    for index in [
        "idx_lash_queued_work_session_command_order",
        "idx_lash_pending_turn_input_order",
    ] {
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
                .bind(index)
                .fetch_one(&scratch.pool)
                .await
                .expect("read migrated index");
        assert!(present, "migration omitted {index}");
    }
    scratch.cleanup().await;
}

/// The published component-53 shape upgrades through the explicit 53 -> 55
/// migration, which adds the effect-group journal and nothing else.
///
/// This is the row production actually takes on this bump: 53 is the immediate
/// predecessor, so every live Lash-managed store crosses here. It is also the
/// only migration whose source shape is described by columns and a guard rather
/// than by whole tables — the pre-open report has to carry the two nullable
/// `lash_runtime_effect_replay` columns and the partial unique guard over them,
/// and nothing else, or the source is refused as drifted.
#[tokio::test]
async fn main_component_53_store_upgrades_cleanly_to_55() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-53 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             UPDATE lash_schema_versions
                SET version = 53
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-53 shape migrates to 55");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 55);
    for relation in [
        "lash_runtime_effect_group",
        "idx_lash_runtime_effect_group_session",
        "idx_lash_runtime_effect_group_scope",
        "uq_lash_runtime_effect_replay_group_seq",
        "idx_lash_runtime_effect_replay_group_unsettled",
    ] {
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
                .bind(relation)
                .fetch_one(&scratch.pool)
                .await
                .expect("read migrated relation");
        assert!(present, "migration omitted {relation}");
    }
    // The columns the guard is built over are what the journal actually writes,
    // and a `CREATE INDEX` alone would not restore them.
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'lash_runtime_effect_replay'
            AND column_name = ANY($1)",
    )
    .bind(vec!["group_key", "settlement_seq"])
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated effect-replay columns");
    assert_eq!(columns, 2, "migration omitted the effect-group columns");
    scratch.cleanup().await;
}

/// The published component-54 shape upgrades through the explicit 54 -> 55
/// migration, which adds the drain's unsettled-children index and nothing else.
///
/// This is the row production takes on this bump: 54 is the immediate
/// predecessor, so every live Lash-managed store crosses here. Its source shape
/// is the narrowest any migration declares — no missing table, column, or guard,
/// because the shape checker does not compare non-unique indexes at all — so the
/// only finding a conformant 54 store may present is the version stamp itself.
#[tokio::test]
async fn main_component_54_store_upgrades_cleanly_to_55() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-54 migration law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_55_ARTIFACTS}
             UPDATE lash_schema_versions
                SET version = 54
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("the exact published component-54 shape migrates to 55");

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read migrated component version");
    assert_eq!(version, 55);
    // The index is the generation. Asserting the stamp alone would pass on a
    // migration whose only effect was the `UPDATE`, which is the one failure
    // this bump can have.
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
            .bind("idx_lash_runtime_effect_replay_group_unsettled")
            .fetch_one(&scratch.pool)
            .await
            .expect("read migrated relation");
    assert!(present, "migration omitted the drain index");
    scratch.cleanup().await;
}

/// A component-53 stamp that already carries one of the post-53 artifacts is
/// divergence, not a migration source.
///
/// Every 53 -> 55 statement but the `CREATE TABLE` is idempotent, so a retry over
/// a partially-applied generation would silently no-op on the parts it had
/// already landed rather than fail. The `pg_class` probe over
/// `introduced_relations` is the only guard that turns that half-applied shape
/// into a typed refusal naming the artifact, so it is proven with the guard
/// index present and the group table absent.
#[tokio::test]
async fn component_53_stamp_with_one_new_artifact_is_refused_as_divergence() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-53 divergence law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "DROP TABLE lash_runtime_effect_group;
             UPDATE lash_schema_versions
                SET version = 53
              WHERE component = 'lash-postgres-store'",
        )
        .await;

    for check in [SchemaCheck::Enforce, SchemaCheck::WarnOnly] {
        let error = PostgresStorage::from_pool_with(
            scratch.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: SchemaProvisioning::LashManaged,
                schema_check: check,
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .err()
        .unwrap_or_else(|| {
            panic!("a component-53 stamp over a newer artifact must be refused under {check:?}")
        });
        let rendered = error.to_string();
        for fragment in [
            "has version 53",
            "expected 55",
            "schema artifacts newer than the recorded version",
            "uq_lash_runtime_effect_replay_group_seq",
            "inspect and recreate",
        ] {
            assert!(
                rendered.contains(fragment),
                "typed {check:?} divergence refusal must contain {fragment:?}: {rendered}"
            );
        }
        assert!(
            !rendered.contains("lash_runtime_effect_group"),
            "the absent table is not evidence of divergence: {rendered}"
        );
    }

    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read component version after divergence refusal");
    assert_eq!(
        version, 53,
        "the refused open must not advance the version ledger"
    );
    let recreated: bool =
        sqlx::query_scalar("SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL")
            .bind("lash_runtime_effect_group")
            .fetch_one(&scratch.pool)
            .await
            .expect("probe the absent table after refusal");
    assert!(
        !recreated,
        "the refused open must not run any migration DDL"
    );
    scratch.cleanup().await;
}

/// Migration DDL belongs to the installation anchored by the version ledger,
/// even when an earlier writable schema appears on `search_path`.
#[tokio::test]
async fn component_50_migration_stays_in_the_anchored_namespace() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-50 migration namespace law: database URL is not set");
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let front = format!("lash_migration_front_{suffix}");
    let back = format!("lash_migration_anchor_{suffix}");
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect migration namespace admin");
    admin
        .execute(format!("CREATE SCHEMA {front}; CREATE SCHEMA {back}").as_str())
        .await
        .expect("create migration namespaces");
    admin
        .execute(format!("SET search_path TO {back}").as_str())
        .await
        .expect("point admin at anchored namespace");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut admin)
        .await
        .expect("provision current schema in anchored namespace");
    admin
        .execute(
            format!(
                "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             DROP TABLE lash_attachment_condemnations;
             DROP TABLE lash_tool_intent_submissions;
             DROP TABLE lash_process_parent_end_plans;
             UPDATE lash_schema_versions
                SET version = 50
              WHERE component = 'lash-postgres-store'"
            )
            .as_str(),
        )
        .await
        .expect("rewind anchored installation to component 50");
    admin
        .close()
        .await
        .expect("close migration namespace admin");

    let pool = pool_with_search_path(&database_url, &format!("{front}, {back}")).await;
    PostgresStorage::from_pool_with(
        pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("migrate the anchored component-50 installation");

    let front_relations: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = $1
            AND class.relname LIKE 'lash\\_%' ESCAPE '\\'",
    )
    .bind(&front)
    .fetch_one(&pool)
    .await
    .expect("count front-schema Lash relations");
    assert_eq!(
        front_relations, 0,
        "migration and creation DDL must not split into the earlier schema"
    );
    let anchored_artifacts: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = $1
            AND class.relname = ANY($2)",
    )
    .bind(&back)
    .bind(vec![
        "idx_lash_tool_intent_submissions_scope",
        "lash_process_parent_end_plans",
        "lash_runtime_effect_group",
        "lash_tool_intent_submissions",
    ])
    .fetch_one(&pool)
    .await
    .expect("count anchored migration artifacts");
    assert_eq!(anchored_artifacts, 4);
    let current_schema: String = sqlx::query_scalar("SELECT current_schema()::text")
        .fetch_one(&pool)
        .await
        .expect("read restored caller search path");
    assert_eq!(
        current_schema, front,
        "open must restore the caller's search path"
    );

    pool.close().await;
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect migration namespace cleanup");
    for schema in [&front, &back] {
        admin
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .expect("drop migration namespace");
    }
}

/// A component-50 stamp on a current-generation catalog is not a migration source.
/// It is evidence that the version ledger and live schema diverged, so Lash
/// must refuse before running any migration DDL rather than guessing which
/// parts of an apparently newer catalog are trustworthy.
#[tokio::test]
async fn component_50_stamp_with_newer_artifacts_is_refused_without_mutation() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-50 divergence law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let capture = installed_capture();
    scratch
        .apply(
            "UPDATE lash_schema_versions
                SET version = 50
              WHERE component = 'lash-postgres-store'",
        )
        .await;

    let before: Vec<(String, i64)> = sqlx::query_as(
        "SELECT class.relname, class.oid::bigint
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = current_schema()
            AND class.relname = ANY($1)
          ORDER BY class.relname",
    )
    .bind(vec![
        "idx_lash_pending_turn_input_order",
        "idx_lash_queued_work_session_command_order",
        "idx_lash_tool_intent_submissions_scope",
        "lash_attachment_condemnations",
        "lash_process_parent_end_plans",
        "lash_tool_intent_submissions",
    ])
    .fetch_all(&scratch.pool)
    .await
    .expect("probe divergent migration artifacts before open");
    assert_eq!(
        before.len(),
        6,
        "the fixture must retain every newer-generation artifact"
    );

    for check in [SchemaCheck::Enforce, SchemaCheck::WarnOnly] {
        let error = PostgresStorage::from_pool_with(
            scratch.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: SchemaProvisioning::LashManaged,
                schema_check: check,
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .err()
        .unwrap_or_else(|| {
            panic!(
                "a component-50 stamp over newer-generation artifacts must be refused under \
                 {check:?}"
            )
        });
        let rendered = error.to_string();
        for fragment in [
            "has version 50",
            "expected 55",
            "schema artifacts newer than the recorded version",
            "lash_process_parent_end_plans",
            "inspect and recreate",
        ] {
            assert!(
                rendered.contains(fragment),
                "typed {check:?} divergence refusal must contain {fragment:?}: {rendered}"
            );
        }
    }
    let denial_events = capture.events_for(&scratch.name);
    for check in ["Enforce", "WarnOnly"] {
        assert!(
            denial_events.iter().any(|event| {
                event.contains("outcome=denied_migration_divergence")
                    && event.contains(&format!("schema_check={check}"))
                    && event.contains("migration_detail_kind=migration_artifacts")
                    && event.contains("lash_process_parent_end_plans")
            }),
            "the {check} divergence denial must emit its artifact basis: {denial_events:#?}"
        );
    }

    let after: Vec<(String, i64)> = sqlx::query_as(
        "SELECT class.relname, class.oid::bigint
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = current_schema()
            AND class.relname = ANY($1)
          ORDER BY class.relname",
    )
    .bind(vec![
        "idx_lash_pending_turn_input_order",
        "idx_lash_queued_work_session_command_order",
        "idx_lash_tool_intent_submissions_scope",
        "lash_attachment_condemnations",
        "lash_process_parent_end_plans",
        "lash_tool_intent_submissions",
    ])
    .fetch_all(&scratch.pool)
    .await
    .expect("probe divergent migration artifacts after refusal");
    assert_eq!(
        after, before,
        "the refused open must not mutate migration artifacts"
    );
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read component version after divergence refusal");
    assert_eq!(
        version, 50,
        "the refused open must not advance the version ledger"
    );

    scratch.cleanup().await;
}

/// A migration is permission to transform one exact published source shape,
/// not every catalog carrying its version integer.
#[tokio::test]
async fn drifted_component_50_source_is_refused_before_migration_ddl() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-50 source-shape law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             DROP TABLE lash_attachment_condemnations;
             DROP TABLE lash_tool_intent_submissions;
             DROP TABLE lash_process_parent_end_plans;
             DROP TABLE lash_processes CASCADE;
             UPDATE lash_schema_versions
                SET version = 50
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    let error = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .err()
    .expect("a drifted component-50 source must be refused");
    let rendered = error.to_string();
    for fragment in [
        "has version 50",
        "expected 55",
        "does not match the published component-50 migration source shape",
        "lash_processes: table is missing",
        "inspect and recreate",
    ] {
        assert!(
            rendered.contains(fragment),
            "typed source-shape refusal must contain {fragment:?}: {rendered}"
        );
    }
    let introduced_relations: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = current_schema()
            AND class.relname = ANY($1)",
    )
    .bind(vec![
        "idx_lash_tool_intent_submissions_scope",
        "lash_process_parent_end_plans",
        "lash_tool_intent_submissions",
    ])
    .fetch_one(&scratch.pool)
    .await
    .expect("probe migration artifacts after source-shape refusal");
    assert_eq!(
        introduced_relations, 0,
        "source-shape refusal must run no migration DDL"
    );
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&scratch.pool)
    .await
    .expect("read version after source-shape refusal");
    assert_eq!(version, 50);

    scratch.cleanup().await;
}

/// WarnOnly is a shape valve, not permission to run workers against the
/// component-50 schema before its migration has been applied.
#[tokio::test]
async fn warn_only_refuses_component_50_before_process_workers_can_open() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping component-50 WarnOnly law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(&format!(
            "{REWIND_PAST_54_ARTIFACTS}
             DROP INDEX idx_lash_queued_work_session_command_order;
             DROP INDEX idx_lash_pending_turn_input_order;
             DROP TABLE lash_attachment_condemnations;
             DROP TABLE lash_tool_intent_submissions;
             DROP TABLE lash_process_parent_end_plans;
             UPDATE lash_schema_versions
                SET version = 50
              WHERE component = 'lash-postgres-store'"
        ))
        .await;

    for provisioning in [
        SchemaProvisioning::HostProvisioned,
        SchemaProvisioning::LashManaged,
    ] {
        let error = PostgresStorage::from_pool_with(
            scratch.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: provisioning,
                schema_check: SchemaCheck::WarnOnly,
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("{provisioning:?} WarnOnly must refuse component 50"));
        let rendered = error.to_string();
        assert!(
            rendered.contains("has version 50")
                && rendered.contains("expected 55")
                && rendered.contains("does not relax it"),
            "typed version refusal was lost for {provisioning:?}: {rendered}"
        );
    }
    scratch.cleanup().await;
}

/// The seed check must query the row lash actually reads. `CHECK (singleton)` is
/// deliberately unverified, so a host port that omits it can hold a
/// `singleton = FALSE` row — which satisfies "the table has rows" but leaves
/// `next_process_change_seq`'s `UPDATE ... WHERE singleton = TRUE RETURNING`
/// matching nothing on the first process mutation.
#[tokio::test]
async fn a_seed_row_under_the_wrong_key_is_rejected() {
    assert_mutation_is_rejected(
        // Drop the CHECK by discovery rather than by name: its name is
        // auto-generated, and a host port that omits it would not reproduce it
        // anyway. CHECK constraints are deliberately outside the verified scope,
        // which is precisely why the seed check cannot lean on this one.
        "DO $$
         DECLARE constraint_name text;
         BEGIN
             FOR constraint_name IN
                 SELECT conname FROM pg_catalog.pg_constraint
                 WHERE conrelid = 'lash_process_change_clock'::regclass AND contype = 'c'
             LOOP
                 EXECUTE format(
                     'ALTER TABLE lash_process_change_clock DROP CONSTRAINT %I',
                     constraint_name
                 );
             END LOOP;
         END $$;
         DELETE FROM lash_process_change_clock;
         INSERT INTO lash_process_change_clock (singleton, current_seq) VALUES (FALSE, 0)",
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

/// A signing secret seeded at the wrong width used to be an open-time backend
/// error only, so a host gating its migration CI on the report got a green run and
/// a red production open. It is a report finding now, while open keeps rejecting
/// it — and `verify_schema_for` reaches it without needing the open that fails.
#[tokio::test]
async fn a_wrong_width_signing_secret_is_a_finding_and_still_fatal_at_open() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping signing-secret width check: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply("UPDATE lash_await_event_meta SET signing_secret = '\\x0102'::bytea")
        .await;
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the short-secret database");
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::InvalidSeedRow { table, detail }
                if table == "lash_await_event_meta" && detail.contains("2 bytes")
        )),
        "the report must carry the secret width: {:?}",
        report.findings
    );
    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("a short signing secret must not open");
    assert!(error.to_string().contains("expected 32"), "{error}");
    scratch.cleanup().await;
}

/// `verify_schema_for` has to work on the databases it exists for: ones too broken
/// to open at all. Removing the signing-secret table makes every open fail, in
/// every mode, and the structured report is the only diagnosis available.
#[tokio::test]
async fn verify_schema_for_describes_a_database_that_cannot_be_opened() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping unopenable-database verification: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch.apply("DROP TABLE lash_await_event_meta").await;
    for check in [SchemaCheck::Enforce, SchemaCheck::WarnOnly] {
        assert!(
            scratch.open_host_provisioned(check).await.is_err(),
            "{check:?} must not open a database with no signing-secret table"
        );
    }
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify_schema_for must not need a successful open");
    assert!(report.findings.iter().any(|finding| matches!(
        finding,
        SchemaFinding::MissingTable { table } if table == "lash_await_event_meta"
    )));
    assert!(
        report.to_string().contains("MISSING TABLES"),
        "the report must render the diff a host's CI reads: {report}"
    );
    scratch.cleanup().await;
}

/// A seed table rebuilt with the right column names and the wrong types used to
/// abort verification: the probe checked presence, then bound a boolean against a
/// `text` column and raised `operator does not exist: text = boolean`. A method
/// whose whole purpose is describing a broken schema must never do that — the
/// column drift is already fully diagnosable from the catalog.
#[tokio::test]
async fn a_seed_table_with_mistyped_columns_reports_instead_of_aborting() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping mistyped-seed-column reporting: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "DROP TABLE lash_await_event_meta;
             CREATE TABLE lash_await_event_meta (
                 singleton TEXT PRIMARY KEY,
                 signing_secret BYTEA NOT NULL
             );
             INSERT INTO lash_await_event_meta (singleton, signing_secret)
             VALUES ('t', decode('0102', 'hex'));",
        )
        .await;
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verification must report mistyped seed columns, not abort on them");
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::ColumnMismatch { table, expected, found }
                if table == "lash_await_event_meta"
                    && expected.name == "singleton"
                    && found.sql_type == "text"
        )),
        "the mistyped column must be a structured finding: {:?}",
        report.findings
    );
    assert!(
        scratch
            .open_host_provisioned(SchemaCheck::Enforce)
            .await
            .is_err(),
        "a mistyped seed column must still refuse the open"
    );
    scratch.cleanup().await;
}

/// Same class on the version stamp itself. `read_component_version` decodes
/// `version` as `i32`, so a `text` column would raise a decode error — and because
/// the stamp is then unreadable rather than merely absent, the structural diff must
/// *not* be short-circuited: the column drift is the diagnosis a host needs.
#[tokio::test]
async fn a_mistyped_version_stamp_reports_the_column_drift() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping mistyped-version-stamp reporting: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply("ALTER TABLE lash_schema_versions ALTER COLUMN version TYPE TEXT")
        .await;
    let report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verification must report a mistyped version column, not abort on it");
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::ColumnMismatch { table, expected, found }
                if table == "lash_schema_versions"
                    && expected.name == "version"
                    && found.sql_type == "text"
        )),
        "an unreadable stamp must not suppress the column diff that explains it: {:?}",
        report.findings
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| matches!(finding, SchemaFinding::VersionMismatch { found: None, .. })),
        "an undecodable stamp is also not a usable version: {:?}",
        report.findings
    );
    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("an unreadable version stamp must refuse the open");
    assert!(
        error.to_string().contains("has no version stamp"),
        "{error}"
    );
    scratch.cleanup().await;
}

/// The remedy a report prints has to be one a host can actually follow. A version
/// mismatch is unconditional, so recommending `SchemaCheck::WarnOnly` there would
/// send a host down a path that cannot open the database.
#[tokio::test]
async fn report_remedies_match_the_finding_class() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping remedy-truthfulness check: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 1 WHERE component = 'lash-postgres-store'",
        )
        .await;
    let version_report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the stale-version database")
        .to_string();
    assert!(
        version_report.contains("reject-and-recreate")
            && version_report.contains("no `SchemaCheck` relaxes it"),
        "a version report must give the reject-and-recreate remedy: {version_report}"
    );
    assert!(
        !version_report.contains("SchemaCheck::WarnOnly"),
        "a version report must not recommend a valve that cannot open it: {version_report}"
    );

    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 55 WHERE component = 'lash-postgres-store';
             DROP INDEX idx_lash_process_events_key",
        )
        .await;
    let shape_report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the drifted database")
        .to_string();
    assert!(
        shape_report.contains("SchemaCheck::WarnOnly"),
        "a shape report may still offer the valve: {shape_report}"
    );
    assert!(
        !shape_report.contains("reject-and-recreate"),
        "a shape report must not claim the database needs recreating: {shape_report}"
    );

    // The mixed case: an unreadable stamp carries a VersionMismatch *and* the column
    // findings that explain it. A version finding dominates, so the valve must still
    // not be offered — recreating from the artifact resolves both classes anyway.
    scratch
        .apply("ALTER TABLE lash_schema_versions ALTER COLUMN version TYPE TEXT")
        .await;
    let mixed_report = PostgresStorage::verify_schema_for(&scratch.pool)
        .await
        .expect("verify the unreadable-stamp database")
        .to_string();
    assert!(
        mixed_report.contains("COMPONENT VERSION") && mixed_report.contains("COLUMN DRIFT"),
        "the mixed case must carry both finding classes: {mixed_report}"
    );
    assert!(
        !mixed_report.contains("SchemaCheck::WarnOnly"),
        "a report carrying a version finding must never recommend the valve: {mixed_report}"
    );
    assert!(
        mixed_report.contains("reject-and-recreate"),
        "the mixed case must give the reject-and-recreate remedy: {mixed_report}"
    );
    scratch.cleanup().await;
}

/// Captures this crate's `tracing` events so a decision seam's evidence can be
/// asserted the way a test asserts any other output.
///
/// Installed as the *global* subscriber rather than a thread-local one on purpose:
/// `tracing` caches callsite interest process-wide, so a sibling test that reaches
/// the same `warn!` with no subscriber installed can mark the callsite as
/// permanently uninteresting and silence it for everyone. A global subscriber that
/// is always interested in this crate's targets cannot be poisoned that way.
#[derive(Clone, Default)]
struct EventCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl EventCapture {
    /// Every captured event mentioning `schema`, which is unique per scratch
    /// schema and so isolates one test's decisions from every other test's.
    fn events_for(&self, schema: &str) -> Vec<String> {
        self.0
            .lock_recover()
            .iter()
            .filter(|event| event.contains(&format!("schema={schema} ")))
            .cloned()
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !event.metadata().target().starts_with("lash_postgres_store") {
            return;
        }
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push_str(&format!("{}={value} ", field.name()));
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.0.push_str(&format!("{}={value} ", field.name()));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.push_str(&format!("{}={value} ", field.name()));
            }
        }
        let mut visitor = Visitor(format!("level={} ", event.metadata().level()));
        event.record(&mut visitor);
        self.0.lock_recover().push(visitor.0);
    }
}

/// The one global capture for this test binary.
fn installed_capture() -> &'static EventCapture {
    static CAPTURE: std::sync::OnceLock<EventCapture> = std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt;
        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("install the decision-evidence capture");
        capture
    })
}

/// A gate that can deny must ship its decision basis, not just its verdict
/// (`docs/agents/way-of-working.md`). "The code denies correctly but you cannot see
/// why from a trace" is the failure this pins: each of the four outcomes has to
/// carry the versions it compared, both policy knobs, and the finding counts.
#[tokio::test]
async fn the_schema_gate_emits_its_decision_basis() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping schema-gate decision evidence: database URL is not set");
        return;
    };
    let capture = installed_capture();
    let scratch = ScratchSchema::provision(&database_url).await;

    // (a) admitted.
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("open a conformant schema");
    assert_evidence(
        capture,
        &scratch.name,
        "allowed",
        &["found_version=Some(55)", "finding_total=0"],
    );

    // (b) denied on shape.
    scratch
        .apply("DROP INDEX idx_lash_process_events_key")
        .await;
    let error = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("a dropped dedup guard must deny the open");
    assert!(
        error.to_string().contains("UNIQUE GUARD DRIFT"),
        "the denial must be the shape gate, not something else: {error}"
    );
    assert_evidence(
        capture,
        &scratch.name,
        "denied_shape",
        &[
            "UNIQUE GUARD DRIFT=1",
            "finding_total=1",
            "schema_check=Enforce",
        ],
    );

    // (c) admitted under the valve, carrying the same basis.
    scratch
        .open_host_provisioned(SchemaCheck::WarnOnly)
        .await
        .expect("WarnOnly opens the drifted schema");
    assert_evidence(
        capture,
        &scratch.name,
        "allowed_warn_only",
        &["UNIQUE GUARD DRIFT=1", "schema_check=WarnOnly"],
    );

    // (d) denied on the version boundary, which no valve relaxes.
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 1 WHERE component = 'lash-postgres-store'",
        )
        .await;
    assert!(
        scratch
            .open_host_provisioned(SchemaCheck::WarnOnly)
            .await
            .is_err()
    );
    assert_evidence(
        capture,
        &scratch.name,
        "denied_version",
        &[
            "found_version=Some(1)",
            "COMPONENT VERSION=1",
            "schema_check=WarnOnly",
        ],
    );

    // (e) the LashManaged preflight is a separate early return: it denies before the
    // structural read runs at all, and has to carry the same basis.
    assert!(
        PostgresStorage::from_pool_with(
            scratch.pool.clone(),
            PostgresStoreConfig {
                schema_provisioning: SchemaProvisioning::LashManaged,
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .is_err()
    );
    assert_evidence_with_provisioning(
        capture,
        &scratch.name,
        "denied_version_preflight",
        "LashManaged",
        &["found_version=Some(1)", "COMPONENT VERSION=1"],
    );

    scratch.cleanup().await;
}

/// The signing secret is read after the structural verdict, so its two failures are
/// their own outcomes — and they are reachable exactly where they matter: under
/// `WarnOnly`, which relaxes the seed *findings* but cannot conjure a key to
/// authenticate promises with. Recording an admission before that read would let such
/// a database log an admit and then refuse the open, and decision evidence that
/// contradicts what happened is worse than none.
#[tokio::test]
async fn a_rejected_signing_secret_emits_its_own_decision() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping signing-secret decision evidence: database URL is not set");
        return;
    };
    let capture = installed_capture();

    for (mutation, outcome) in [
        (
            "UPDATE lash_await_event_meta SET signing_secret = decode('0102', 'hex')",
            "denied_seed_secret_width",
        ),
        (
            "DELETE FROM lash_await_event_meta",
            "denied_seed_secret_missing",
        ),
    ] {
        let scratch = ScratchSchema::provision(&database_url).await;
        scratch.apply(mutation).await;

        // Under Enforce the report already names the bad row, so the shape gate is
        // what denies — with its own evidence.
        assert!(
            scratch
                .open_host_provisioned(SchemaCheck::Enforce)
                .await
                .is_err()
        );
        assert_evidence(capture, &scratch.name, "denied_shape", &["SEED ROWS=1"]);

        // Under WarnOnly the finding is relaxed and the secret read is what refuses.
        assert!(
            scratch
                .open_host_provisioned(SchemaCheck::WarnOnly)
                .await
                .is_err()
        );
        assert_evidence(
            capture,
            &scratch.name,
            outcome,
            &["schema_check=WarnOnly", "SEED ROWS=1"],
        );
        assert!(
            capture
                .events_for(&scratch.name)
                .iter()
                .all(|event| !event.contains("outcome=allowed")),
            "no open succeeded, so nothing may have logged an admission: {:?}",
            capture.events_for(&scratch.name)
        );
        scratch.cleanup().await;
    }
}

/// Asserts one captured decision carries the named outcome plus the inputs the gate
/// consulted to reach it.
fn assert_evidence(capture: &EventCapture, schema: &str, outcome: &str, extra: &[&str]) {
    assert_evidence_with_provisioning(capture, schema, outcome, "HostProvisioned", extra);
}

/// As [`assert_evidence`], for an outcome reached under another provisioning mode.
fn assert_evidence_with_provisioning(
    capture: &EventCapture,
    schema: &str,
    outcome: &str,
    provisioning: &str,
    extra: &[&str],
) {
    let events = capture.events_for(schema);
    let event = events
        .iter()
        .find(|event| event.contains(&format!("outcome={outcome} ")))
        .unwrap_or_else(|| {
            panic!(
                "no schema-gate event with outcome={outcome} for {schema}; captured:\n{events:#?}"
            )
        });
    let provisioning = format!("provisioning={provisioning}");
    for field in ["component=lash-postgres-store", "expected_version=55"]
        .iter()
        .chain(std::iter::once(&provisioning.as_str()))
        .chain(extra)
    {
        assert!(
            event.contains(field),
            "the {outcome} decision must carry {field}; got:\n{event}"
        );
    }
}

/// The published migration protocol tells CI to hold the advisory key around its
/// migrate-then-verify sequence. `verify_schema_for` acquires the key itself, so a
/// caller already holding it exclusively would queue behind its own connection
/// forever; `verify_schema_on` is the verifier for that case. This pins both halves:
/// the standalone form blocks, and the connection-taking form completes.
#[tokio::test]
async fn a_caller_holding_the_advisory_key_can_still_verify() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping locked-caller verification: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let (lock_namespace, lock_key) = PostgresStorage::schema_advisory_lock_key();

    // A migration CI job: take the key exclusively, then verify on that connection.
    let mut migrator = scratch.pool.acquire().await.expect("acquire migrator");
    sqlx::query("SELECT pg_advisory_lock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *migrator)
        .await
        .expect("take the published key exclusively");

    let report = PostgresStorage::verify_schema_on(&mut migrator)
        .await
        .expect("verify on the already-locked connection");
    assert!(
        report.is_conformant(),
        "verification on the locked connection must describe the schema: {report}"
    );

    // The pool form must not be usable here: it waits for the shared lock behind the
    // exclusive hold this same test still owns.
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        PostgresStorage::verify_schema_for(&scratch.pool),
    )
    .await;
    assert!(
        blocked.is_err(),
        "verify_schema_for must queue behind an exclusive holder, which is exactly why \
         verify_schema_on exists"
    );

    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *migrator)
        .await
        .expect("release the published key");
    drop(migrator);

    // Released, so the standalone form works again.
    assert!(
        PostgresStorage::verify_schema_for(&scratch.pool)
            .await
            .expect("verify once the key is free")
            .is_conformant()
    );
    scratch.cleanup().await;
}

/// `verify_schema_for` claims every catalog read shares one snapshot. That is only
/// true if the transaction is `REPEATABLE READ` *and* the lock was granted before the
/// snapshot was taken — an `xact`-scoped lock acquired as the first statement
/// snapshots before it is granted, so a verification queued behind a migration would
/// describe the pre-migration schema. This pins the observable consequence: a
/// verification that waits for the key sees what the holder committed.
#[tokio::test]
async fn verification_waiting_for_the_key_sees_the_holders_committed_work() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping snapshot-ordering check: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let (lock_namespace, lock_key) = PostgresStorage::schema_advisory_lock_key();

    // Hold the key, then drop the dedup guard while a verification is already queued.
    let mut holder = scratch.pool.acquire().await.expect("acquire holder");
    sqlx::query("SELECT pg_advisory_lock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *holder)
        .await
        .expect("take the key");

    let verify_pool = scratch.pool.clone();
    let verification =
        tokio::spawn(async move { PostgresStorage::verify_schema_for(&verify_pool).await });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    sqlx::query("DROP INDEX idx_lash_process_events_key")
        .execute(&mut *holder)
        .await
        .expect("drop the guard while the verification waits");
    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *holder)
        .await
        .expect("release the key");
    drop(holder);

    let report = verification
        .await
        .expect("verification task")
        .expect("verification result");
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            SchemaFinding::MissingUniqueGuard { table, .. } if table == "lash_process_events"
        )),
        "a verification that waited for the key must see the schema as the holder left \
         it, not as it was before the wait: {report}"
    );
    scratch.cleanup().await;
}
