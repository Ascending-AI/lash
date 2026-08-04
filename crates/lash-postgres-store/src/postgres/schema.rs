use crate::*;

/// The DDL this build provisions, committed verbatim as the crate's
/// `schema.sql` artifact so a host can vendor the exact bytes lash executes.
pub(crate) const SCHEMA_DDL: &str = include_str!("../../schema.sql");

/// Advisory-lock key lash takes for the duration of a schema-provisioning or
/// schema-verifying transaction. See
/// [`crate::PostgresStorage::schema_advisory_lock_key`].
pub(crate) const SCHEMA_ADVISORY_LOCK_KEY: (i32, i32) = (715421, 907001);

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
        if let Some(installation) = resolve_installation(&mut tx).await?
            && let ComponentVersion::Readable(Some(version)) =
                read_component_version(&mut tx, &installation, &SchemaShape::expected()).await?
            && version != SCHEMA_VERSION
        {
            return Err(version_mismatch_error(Some(version)));
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
    if report.is_conformant() {
        record_schema_gate_decision(&report, options, "allowed");
    } else {
        match options.check {
            SchemaCheck::Enforce => {
                record_schema_gate_decision(&report, options, "denied_shape");
                return Err(StoreError::Backend(report.to_string()));
            }
            SchemaCheck::WarnOnly => {
                record_schema_gate_decision(&report, options, "allowed_warn_only");
                tracing::warn!(
                    "opening Postgres storage against a non-conformant schema because \
                     SchemaCheck::WarnOnly is configured: {report}"
                );
            }
        }
    }

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
    let signing_secret = signing_secret.ok_or_else(|| {
        StoreError::Backend(
            "Postgres await-event signing secret row is missing from lash_await_event_meta; \
             apply the seed statements from this build's schema.sql artifact"
                .to_string(),
        )
    })?;
    if signing_secret.len() != AWAIT_EVENT_SIGNING_SECRET_BYTES {
        return Err(StoreError::Backend(format!(
            "Postgres await-event signing secret has {} bytes, expected \
             {AWAIT_EVENT_SIGNING_SECRET_BYTES}",
            signing_secret.len()
        )));
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(signing_secret)
}

/// Runs the structural check under the published advisory key, held in *shared*
/// mode for the duration of one transaction.
///
/// The key is the coordination point lash publishes for hosts, so verification has
/// to take it or the protocol is only half real: a migration that takes the key
/// would exclude opens but not the migration-CI verification the same docs
/// recommend. Shared mode is the right strength — concurrent verifications do not
/// conflict with each other, an exclusive holder (a lash open, or a host migration
/// following the protocol) excludes them all, and one transaction gives every
/// catalog read a single snapshot.
pub(crate) async fn verify_schema_under_advisory_lock(
    pool: &PgPool,
) -> Result<SchemaReport, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
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
fn version_mismatch_error(found: Option<i32>) -> StoreError {
    let found = match found {
        Some(version) => format!("has version {version}"),
        None => "has no version stamp".to_string(),
    };
    StoreError::Backend(format!(
        "Postgres schema component `{SCHEMA_COMPONENT}` {found}, expected {SCHEMA_VERSION}. \
         The component schema is a reject-and-recreate boundary with no migration chain: \
         provision a fresh database from this build's schema.sql artifact. This gate is \
         unconditional; SchemaCheck::WarnOnly does not relax it."
    ))
}
