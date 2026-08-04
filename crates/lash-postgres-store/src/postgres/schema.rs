use crate::*;

/// The DDL this build provisions, committed verbatim as the crate's
/// `schema.sql` artifact so a host can vendor the exact bytes lash executes.
pub(crate) const SCHEMA_DDL: &str = include_str!("../../schema.sql");

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
    // Serializes concurrent openers so a reader cannot introspect a half-applied
    // DDL batch. The lock needs no privileges, so it is taken in both modes.
    tx.execute("SELECT pg_advisory_xact_lock(715421, 907001)")
        .await
        .map_err(store_sqlx_error)?;
    if options.provisioning == SchemaProvisioning::LashManaged {
        // Preflight before the DDL: a stale baseline must be rejected rather than
        // have this build's creation statements layered over it.
        if let Some(version) = read_component_version(&mut tx).await?
            && version != SCHEMA_VERSION
        {
            return Err(StoreError::Backend(format!(
                "Postgres schema component `{SCHEMA_COMPONENT}` has version {version}, expected {SCHEMA_VERSION}"
            )));
        }
        tx.execute(SCHEMA_DDL).await.map_err(store_sqlx_error)?;
    }

    let report = verify_schema_shape(&mut tx).await?;
    if !report.is_conformant() {
        match options.check {
            SchemaCheck::Enforce => return Err(StoreError::Backend(report.to_string())),
            SchemaCheck::WarnOnly => tracing::warn!(
                "opening Postgres storage against a non-conformant schema because \
                 SchemaCheck::WarnOnly is configured: {report}"
            ),
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
    // itself. A host-provisioned database missing this row must apply the seed
    // statements from `schema.sql`.
    let signing_secret = signing_secret.ok_or_else(|| {
        StoreError::Backend(
            "Postgres await-event signing secret row is missing from lash_await_event_meta; \
             apply the seed statements from this build's schema.sql artifact"
                .to_string(),
        )
    })?;
    if signing_secret.len() != 32 {
        return Err(StoreError::Backend(format!(
            "Postgres await-event signing secret has {} bytes, expected 32",
            signing_secret.len()
        )));
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(signing_secret)
}

/// Reads the component version stamp, tolerating a database that has no
/// `lash_schema_versions` table at all.
async fn read_component_version(
    connection: &mut sqlx::PgConnection,
) -> Result<Option<i32>, StoreError> {
    let stamped: bool =
        sqlx::query_scalar("SELECT pg_catalog.to_regclass('lash_schema_versions') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await
            .map_err(store_sqlx_error)?;
    if !stamped {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
        .bind(SCHEMA_COMPONENT)
        .fetch_optional(connection)
        .await
        .map_err(store_sqlx_error)
}
