use crate::*;

/// The DDL this build requires, committed verbatim as the crate's
/// `schema.sql` artifact so a host can vendor the exact bytes lash executes.
///
/// Workers never run it: the only callers permitted to apply it are the `lash
/// migrate` runner and host tooling, never an open (FIG-3797, FIG-3816).
pub(crate) const SCHEMA_DDL: &str = include_str!("../../schema.sql");

/// The DDL that drops every object `SCHEMA_DDL` provisions, committed verbatim
/// as the crate's `teardown.sql` artifact so a host can vendor the exact bytes.
/// The artifact consistency test regenerates it from the object list the
/// schema DDL declares, so the two files can never drift apart.
pub(crate) const TEARDOWN_DDL: &str = include_str!("../../teardown.sql");

/// Advisory-lock key lash takes for the duration of a schema-verifying open or
/// a `lash migrate` run. See
/// [`crate::PostgresStorage::schema_advisory_lock_key`].
pub(crate) const SCHEMA_ADVISORY_LOCK_KEY: (i32, i32) = (715421, 907001);

/// The generation component `$1` is provisioned at, or no row when the
/// database has never been stamped for it.
///
/// It lives here, with the artifact that writes it, rather than in a table
/// module: `schema_versions` is the one lash table whose *name* is also a
/// *column* of another table (`lash_release_stamp.schema_versions`), and the
/// renderer rewrites a table name wherever the token appears, so registering
/// it would rewrite that column too. Provisioning owns the stamp; the schema
/// artifacts are the ownership gate's declared home for it.
#[cfg(feature = "testing")]
pub(crate) const SELECT_COMPONENT_VERSION: &str =
    "SELECT version FROM lash_schema_versions WHERE component = $1";

/// Whether a stamped component version is inside the supported range
/// [MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION] (FIG-3797).
pub(crate) fn supported_version(version: Option<i32>) -> bool {
    version.is_some_and(|v| (MIN_SUPPORTED_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&v))
}

/// Verifies the database is in the state this build admits and returns the
/// catalog's identity.
///
/// Open is read-only about the schema itself: workers never run DDL on
/// PostgreSQL (FIG-3797), so this gate verifies rather than provisions. The
/// only write is the release stamp, recorded by the transaction that admitted
/// the database. A database that opens is a database whose shape lash has read
/// — never one whose version stamp merely claimed the right number.
pub(crate) async fn ensure_schema(pool: &PgPool, check: SchemaCheck) -> Result<String, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    // Serializes lash's own openers with each other and with a `lash migrate`
    // run holding the same key exclusively, so a verifying open cannot read a
    // half-applied migration batch. The lock needs no privileges, so a runtime
    // role that can neither create nor alter anything can still hold it.
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;

    let report = verify_schema_shape(&mut tx).await?;
    // The two-sided supported range is unconditional (FIG-3797): a stamp below
    // the minimum is an older or skipped compatibility release, one above the
    // latest is a newer build's catalog. `SchemaCheck` governs the structural
    // comparison only; letting `WarnOnly` downgrade this would silently run one
    // build against another schema generation.
    if !supported_version(report.found_version) {
        record_schema_gate_decision(&report, check, "denied_version");
        let writing_release = crate::release_stamp::read_release_in_tx(&mut tx).await;
        return Err(version_mismatch_error(
            report.schema.as_deref(),
            report.found_version,
            writing_release.as_deref(),
        ));
    }
    let admitted_as = match (report.is_conformant(), check) {
        (true, _) => "allowed",
        (false, SchemaCheck::Enforce) => {
            record_schema_gate_decision(&report, check, "denied_shape");
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

    // The identity is a data precondition, not a shape: `SchemaCheck::WarnOnly`
    // relaxes structural enforcement, never the store's ability to construct
    // itself. The admission is recorded only after this succeeds, so a refused
    // open never logs an admission first.
    let Some(catalog_id) = read_catalog_id(&mut *tx).await.map_err(store_sqlx_error)? else {
        record_schema_gate_decision(&report, check, "denied_seed_catalog_identity_missing");
        return Err(missing_catalog_identity_error());
    };
    record_schema_gate_decision(&report, check, admitted_as);
    // Only an admitted open stamps. A refused open has not written this
    // database and must not claim it did, and the write rides the admitting
    // transaction so a rollback anywhere after this point takes the stamp with
    // it. Stamping is DML on a lash-owned table, not DDL: a runtime role with
    // only row privileges still records it, and a role that cannot is skipped
    // rather than failed.
    crate::release_stamp::write(&mut tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(catalog_id)
}

/// The catalog's identity row, the random id the seed statements wrote at
/// install, or `None` when the row is absent.
pub(crate) async fn read_catalog_id<'e, E>(executor: E) -> Result<Option<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar("SELECT catalog_id FROM lash_catalog_identity WHERE singleton = TRUE")
        .fetch_optional(executor)
        .await
}

/// The refusal for a catalog whose identity row is missing: a data
/// precondition no `SchemaCheck` relaxes, because open has no identity to hand
/// its session catalogs without it.
pub(crate) fn missing_catalog_identity_error() -> StoreError {
    StoreError::Backend(
        "Postgres catalog identity row is missing from lash_catalog_identity; apply the seed \
         statements from this build's schema.sql artifact"
            .to_string(),
    )
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

/// A gate that can deny ships the inputs it consulted, not just its verdict
/// (`docs/agents/way-of-working.md`): the stamped and expected versions, the
/// policy knob, and the finding counts per class, so a refused open can be
/// diagnosed from a trace without reproducing it.
fn record_schema_gate_decision(report: &SchemaReport, check: SchemaCheck, outcome: &'static str) {
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
            supported_min = MIN_SUPPORTED_SCHEMA_VERSION,
            schema_check = ?check,
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
            supported_min = MIN_SUPPORTED_SCHEMA_VERSION,
            schema_check = ?check,
            findings = %fields,
            finding_total = report.findings.len(),
            outcome,
            "lash Postgres schema gate decided against admitting the database as-is"
        ),
    }
}

/// The recreate procedure, stated where the operator reads it rather than behind
/// a link. Every SQL-store refusal that ends in reject-and-recreate shares this
/// text so the four durable surfaces are always named together (FIG-3173).
fn recreate_trust_domain_remedy() -> String {
    "Drain the affected sessions and recreate the whole Lash trust domain with this build: drop \
     the schema lash owns (`DROP SCHEMA ... CASCADE`) or recreate the database, provision it with \
     `lash migrate` or this build's schema.sql artifact (`PostgresStorage::schema_ddl()`, \
     committed as crates/lash-postgres-store/schema.sql), and reset the Restate state with it — \
     Restate left behind still refers to sessions the recreated database does not have. An older \
     build's catalog can hold tables this build's teardown no longer names, so this build's \
     `teardown_ddl()` does not clear it. \
     docs/adr/0081-destructive-schema-changes-are-currently-reject-and-recreate.md records why \
     this boundary refuses instead of migrating."
        .to_string()
}

/// Renders the supported-range refusal, naming the found version and the
/// admitted range rather than only the expected integer (FIG-3797).
///
/// The range is the two-sided fact every arm shares: a stamp below
/// `MIN_SUPPORTED_SCHEMA_VERSION` is an older build or a skipped compatibility
/// release, a stamp above `SCHEMA_VERSION` is a newer build's catalog, and no
/// stamp at all means the generation cannot be established — or the database
/// was never provisioned, in which case the remedy is `lash migrate`, not
/// recreation.
///
/// Every arm keeps the phrase `has no applicable migration` where a migration
/// is the thing that does not exist: it is what the version-bump runbook
/// companion classifies this refusal by. The one exception is a database with
/// no installation at all, where the honest statement is that nothing was ever
/// provisioned.
///
/// `writing_release` names the lash release that wrote the database when the
/// release stamp could still be read. It rides as a trailing sentence: every
/// substring the version-bump runbook companion and the store tests pin — the
/// component clause, the `has no applicable migration` phrase, the remedy, the
/// `SchemaCheck::WarnOnly` sentence — is produced byte-identically, and a
/// database with no readable stamp produces the message unchanged rather than a
/// hedge about an unknown release.
pub(crate) fn version_mismatch_error(
    installed_schema: Option<&str>,
    found: Option<i32>,
    writing_release: Option<&str>,
) -> StoreError {
    let range = format!("{MIN_SUPPORTED_SCHEMA_VERSION}..={SCHEMA_VERSION}");
    let (stamp, explanation) = match found {
        Some(version) if version < MIN_SUPPORTED_SCHEMA_VERSION => (
            format!("has version {version}"),
            format!(
                "That database was provisioned by an older or skipped release: component \
                 {version} predates this build's minimum supported component \
                 {MIN_SUPPORTED_SCHEMA_VERSION} and has no applicable migration. This build \
                 declares no forward migration into component {SCHEMA_VERSION}. The component \
                 schema is normally a reject-and-recreate boundary. {}",
                recreate_trust_domain_remedy()
            ),
        ),
        // A caller only renders a mismatch, so the remaining stamped case is a
        // database written by a newer build than this binary.
        Some(version) => (
            format!("has version {version}"),
            format!(
                "That database was provisioned by a newer build: component {version} is ahead of \
                 this build's supported range {range} and has no applicable migration, because \
                 Lash never migrates a schema backwards. Deploy a build whose supported range \
                 includes component {version} instead of downgrading it."
            ),
        ),
        // Lash relations exist but the stamp row does not: the generation
        // cannot be established and the remedy is recreation, not migration.
        None if installed_schema.is_some() => (
            "has no version stamp".to_string(),
            format!(
                "That database carries Lash relations but no readable `lash_schema_versions` row \
                 for this component, so its generation cannot be established at all and it has no \
                 applicable migration. {}",
                recreate_trust_domain_remedy()
            ),
        ),
        // Nothing on the search path is lash's: the host never provisioned.
        // Migration, not recreation, is the remedy — there is no trust domain
        // to drain.
        None => (
            "has no version stamp".to_string(),
            "That database is unprovisioned: no lash schema installation resolves through the \
             connection's search_path. Run `lash migrate` (or apply this build's schema.sql \
             artifact through the host's own tooling) before opening it."
                .to_string(),
        ),
    };
    let release_clause = match writing_release {
        Some(release) => format!(" This database was last written by lash release {release}."),
        None => String::new(),
    };
    StoreError::SchemaVersionOutOfRange {
        component: SCHEMA_COMPONENT.to_string(),
        found,
        supported_min: MIN_SUPPORTED_SCHEMA_VERSION,
        supported_latest: SCHEMA_VERSION,
        message: format!(
            "Postgres schema component `{SCHEMA_COMPONENT}` {stamp}, expected {SCHEMA_VERSION} \
             (supported range {range}). {explanation} This gate is unconditional; \
             SchemaCheck::WarnOnly does not relax it.{release_clause}"
        ),
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
