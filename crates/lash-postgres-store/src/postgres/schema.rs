use crate::*;

/// The DDL this build provisions, committed verbatim as the crate's
/// `schema.sql` artifact so a host can vendor the exact bytes lash executes.
pub(crate) const SCHEMA_DDL: &str = include_str!("../../schema.sql");

/// Advisory-lock key lash takes for the duration of a schema-provisioning or
/// schema-verifying transaction. See
/// [`crate::PostgresStorage::schema_advisory_lock_key`].
pub(crate) const SCHEMA_ADVISORY_LOCK_KEY: (i32, i32) = (715421, 907001);

struct SchemaMigration {
    from: i32,
    to: i32,
    statements: &'static [&'static str],
}

const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    from: 50,
    to: 51,
    statements: &[
        r#"CREATE TABLE lash_process_parent_end_plans (
            process_id TEXT PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            actions_json TEXT NOT NULL
        )"#,
        r#"CREATE TABLE lash_tool_intent_submissions (
            replay_key TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            execution_scope_id TEXT NOT NULL,
            tool_call_id TEXT NOT NULL,
            intent_index BIGINT NOT NULL,
            kind TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            submission_json TEXT NOT NULL
        )"#,
        r#"CREATE INDEX idx_lash_tool_intent_submissions_scope
            ON lash_tool_intent_submissions(session_id, execution_scope_id, intent_index)"#,
        r#"UPDATE lash_schema_versions
           SET version = 51
         WHERE component = 'lash-postgres-store' AND version = 50"#,
    ],
}];

/// How one open should treat the database's schema.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SchemaOpenOptions {
    pub(crate) provisioning: SchemaProvisioning,
    pub(crate) check: SchemaCheck,
}

/// Brings the database to the state this build requires and returns the
/// store-resident await-event signing secret.
///
/// Both provisioning modes end in the same structural verification, so a
/// database that opens is a database whose shape lash has read — never one whose
/// version stamp merely claimed the right number.
pub(crate) async fn ensure_schema(
    pool: &PgPool,
    options: SchemaOpenOptions,
) -> Result<Vec<u8>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    // Serializes lash's own openers, so two concurrent first opens cannot race
    // each other's DDL and a verifying open cannot read a half-applied batch from
    // a provisioning one. It does *not* coordinate with host migrations: nothing
    // outside lash takes this key unless a host chooses to, which
    // `PostgresStorage::schema_advisory_lock_key` exists to let it do. The lock
    // needs no privileges, so it is taken in both modes.
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    if options.provisioning == SchemaProvisioning::LashManaged {
        // Preflight before the DDL: a stale baseline must be rejected rather than
        // have this build's creation statements layered over it.
        let search_path = read_search_path(&mut tx).await?;
        let installation = resolve_installation(&mut tx, &search_path).await?;
        let preflight_mismatch = if let Some(installation) = installation {
            match read_component_version(&mut tx, &installation, &SchemaShape::expected()).await? {
                ComponentVersion::Readable(Some(found))
                    if options.check == SchemaCheck::Enforce
                        && apply_schema_migration(&mut tx, found).await? =>
                {
                    None
                }
                ComponentVersion::Readable(found) if found != Some(SCHEMA_VERSION) => {
                    Some((installation.namespace().to_string(), found))
                }
                ComponentVersion::Readable(_) | ComponentVersion::Unreadable => None,
            }
        } else {
            let unstamped_schema: Option<String> = sqlx::query_scalar(
                r#"SELECT current_schema()::text
                   WHERE EXISTS (
                       SELECT 1
                       FROM pg_catalog.pg_class AS class
                       JOIN pg_catalog.pg_namespace AS namespace
                         ON namespace.oid = class.relnamespace
                       WHERE namespace.nspname = current_schema()
                         AND class.relname LIKE 'lash\_%' ESCAPE '\'
                         AND class.relname <> 'lash_schema_versions'
                         AND class.relkind IN ('r', 'p', 'v', 'm', 'S')
                   )"#,
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            unstamped_schema.map(|schema| (schema, None))
        };
        if let Some((schema, found_version)) = preflight_mismatch {
            // Same field set as every other outcome, built from what the preflight
            // knows: it runs before the structural read, so the only finding it can
            // have is the version itself.
            let preflight = SchemaReport {
                schema: Some(schema),
                expected_version: SCHEMA_VERSION,
                found_version,
                findings: vec![SchemaFinding::VersionMismatch {
                    expected: SCHEMA_VERSION,
                    found: found_version,
                }],
            };
            record_schema_gate_decision(&preflight, options, "denied_version_preflight");
            return Err(version_mismatch_error(found_version));
        }
        tx.execute(SCHEMA_DDL).await.map_err(store_sqlx_error)?;
    }

    let report = verify_schema_shape(&mut tx).await?;
    // The component version is the reject-and-recreate boundary, and it is
    // unconditional: `SchemaCheck` governs the catalog comparison only. Letting
    // `WarnOnly` downgrade this would turn lash's own recommended escape hatch
    // into a path that silently runs one build against another schema generation,
    // which is exactly the cross-version corruption the boundary exists to stop.
    if report.found_version != Some(SCHEMA_VERSION) {
        record_schema_gate_decision(&report, options, "denied_version");
        return Err(version_mismatch_error(report.found_version));
    }
    let admitted_as = match (report.is_conformant(), options.check) {
        (true, _) => "allowed",
        (false, SchemaCheck::Enforce) => {
            record_schema_gate_decision(&report, options, "denied_shape");
            return Err(StoreError::Backend(report.to_string()));
        }
        (false, SchemaCheck::WarnOnly) => {
            tracing::warn!(
                "opening Postgres storage against a non-conformant schema because \
                 SchemaCheck::WarnOnly is configured: {report}"
            );
            "allowed_warn_only"
        }
    };

    let signing_secret: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    // The secret is a data precondition, not a shape: `SchemaCheck::WarnOnly`
    // relaxes structural enforcement, never the store's ability to construct
    // itself. Without this row there is no key to authenticate durable await-event
    // promises with, so there is nothing to hand back. A host-provisioned database
    // missing it must apply the seed statements from `schema.sql`.
    //
    // The admission is recorded only after this succeeds. Logging it earlier would
    // let a database with an unusable secret produce an admission event and then a
    // rejected open, which is the one shape of decision evidence worse than none.
    let signing_secret = match signing_secret {
        Some(secret) if secret.len() == AWAIT_EVENT_SIGNING_SECRET_BYTES => secret,
        Some(secret) => {
            record_schema_gate_decision(&report, options, "denied_seed_secret_width");
            return Err(StoreError::Backend(format!(
                "Postgres await-event signing secret has {} bytes, expected \
                 {AWAIT_EVENT_SIGNING_SECRET_BYTES}",
                secret.len()
            )));
        }
        None => {
            record_schema_gate_decision(&report, options, "denied_seed_secret_missing");
            return Err(StoreError::Backend(
                "Postgres await-event signing secret row is missing from \
                 lash_await_event_meta; apply the seed statements from this build's schema.sql \
                 artifact"
                    .to_string(),
            ));
        }
    };
    record_schema_gate_decision(&report, options, admitted_as);
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(signing_secret)
}

async fn apply_schema_migration(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    found: i32,
) -> Result<bool, StoreError> {
    let Some(migration) = SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == found && migration.to == SCHEMA_VERSION)
    else {
        return Ok(false);
    };
    for statement in migration.statements {
        sqlx::query(statement)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    tracing::info!(
        component = SCHEMA_COMPONENT,
        from_version = migration.from,
        to_version = migration.to,
        outcome = "migrated",
        "applied Lash-managed PostgreSQL schema migration"
    );
    Ok(true)
}

/// Runs the structural check under the published advisory key, held in *shared*
/// mode, with every catalog read pinned to one snapshot.
///
/// Two orderings matter here and neither is incidental.
///
/// The key is taken at *session* scope, before the transaction begins, because a
/// `REPEATABLE READ` snapshot is established by the transaction's first statement —
/// and that includes the statement that waits for a lock. Acquiring an
/// `xact`-scoped lock as the first statement would therefore snapshot the catalog
/// *before* the lock was granted, so a verification that queued behind a host
/// migration would go on to describe the schema as it was before that migration.
/// Measured on PostgreSQL 16: a transaction whose first statement blocks on the key
/// cannot see a table the lock holder committed while it waited.
///
/// The transaction is then `REPEATABLE READ` so every `pg_catalog` read shares one
/// snapshot. `READ COMMITTED` would re-snapshot per statement, which is what let a
/// concurrently committed catalog row appear midway through a verification.
pub(crate) async fn verify_schema_under_advisory_lock(
    pool: &PgPool,
) -> Result<SchemaReport, StoreError> {
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    // Detached rather than borrowed from the pool, because the lock this takes is
    // *session*-scoped: a future cancelled between the lock and the unlock would
    // otherwise hand a still-locked connection back to the pool and block every
    // later exclusive holder for that connection's lifetime. An owned connection is
    // closed when it drops — on the error and cancellation paths as much as the
    // happy one — and the backend releases the session lock with it.
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    let verified = async {
        sqlx::query("SELECT pg_advisory_lock_shared($1, $2)")
            .bind(lock_namespace)
            .bind(lock_key)
            .execute(&mut connection)
            .await
            .map_err(store_sqlx_error)?;
        verify_within_repeatable_read(&mut connection).await
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    verified
}

/// Reads the schema inside one `REPEATABLE READ` transaction.
async fn verify_within_repeatable_read(
    connection: &mut sqlx::PgConnection,
) -> Result<SchemaReport, StoreError> {
    let mut tx = sqlx::Connection::begin(connection)
        .await
        .map_err(store_sqlx_error)?;
    // Must precede every other statement in the transaction: PostgreSQL rejects the
    // change once a snapshot has been established.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let report = verify_schema_shape(&mut tx).await?;
    // Read-only, but committing rather than rolling back keeps the transaction's
    // disposition unambiguous in a host's own logs.
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(report)
}

/// Emits the schema gate's decision basis.
///
/// A gate that can deny ships the inputs it consulted, not just its verdict
/// (`docs/agents/way-of-working.md`): the stamped and expected versions, both
/// policy knobs, and the finding counts per class, so a refused open can be
/// diagnosed from a trace without reproducing it.
fn record_schema_gate_decision(
    report: &SchemaReport,
    options: SchemaOpenOptions,
    outcome: &'static str,
) {
    let counts = report.finding_counts();
    let fields = tracing::field::display(
        counts
            .iter()
            .map(|(section, count)| format!("{section}={count}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let schema = report.schema.as_deref().unwrap_or("<unresolved>");
    match outcome {
        "allowed" => tracing::debug!(
            component = SCHEMA_COMPONENT,
            schema,
            expected_version = report.expected_version,
            found_version = ?report.found_version,
            provisioning = ?options.provisioning,
            schema_check = ?options.check,
            findings = %fields,
            finding_total = report.findings.len(),
            outcome,
            "lash Postgres schema gate admitted the database"
        ),
        _ => tracing::warn!(
            component = SCHEMA_COMPONENT,
            schema,
            expected_version = report.expected_version,
            found_version = ?report.found_version,
            provisioning = ?options.provisioning,
            schema_check = ?options.check,
            findings = %fields,
            finding_total = report.findings.len(),
            outcome,
            "lash Postgres schema gate decided against admitting the database as-is"
        ),
    }
}

/// Renders the reject-and-recreate boundary error, naming the remedy rather than
/// only the numbers.
pub(crate) fn version_mismatch_error(found: Option<i32>) -> StoreError {
    let (found, expected) = match found {
        Some(version) => (
            format!("has version {version}"),
            format!("expected {SCHEMA_VERSION}"),
        ),
        None => (
            "has no version stamp".to_string(),
            format!("expected version {SCHEMA_VERSION}"),
        ),
    };
    StoreError::Backend(format!(
        "Postgres schema component `{SCHEMA_COMPONENT}` {found}, {expected}. \
         The component schema is a reject-and-recreate boundary with no migration chain. Drain \
         affected sessions and recreate the whole Lash trust domain with this version: provision \
         the database from this build's schema.sql artifact, and reset the tombstones, await-event \
         revocation ledger, effect journal, and Restate state together; see \
         docs/persistence.html#delete-sessions. This gate is unconditional; \
         SchemaCheck::WarnOnly does not relax it."
    ))
}
