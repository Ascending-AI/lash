//! `lash migrate`: the operational step that provisions and advances the
//! PostgreSQL component schema (FIG-3816).
//!
//! Worker opens verify and never run DDL, so something else must own "make the
//! catalog this build expects exist". This module is that something: it takes
//! the same advisory lock a verifying open takes — exclusively — applies the
//! pending expand-phase steps the build declares, and records each applied step
//! in the `lash_migrations` ledger so a rerun is a no-op and an interrupted run
//! resumes from the rows that committed.
//!
//! The phase vocabulary is the operations arc's: `expand` is the only phase
//! this pre-1.0 build executes; `backfill` and `contract` are refused outright
//! until FIG-3817 defines them. An expand run on an empty database provisions
//! the whole schema (the bootstrap step is simply this build's `schema.sql`,
//! which is idempotent by construction); on a stamped database it walks the
//! migration catalog from the found stamp forward.
//!
//! Every step is one transaction: statements, the version-stamp move, the
//! ledger row, and the release stamp commit or roll back together, so there is
//! no half-applied state to inspect — a crash means the step simply runs again.
//! The ledger row is keyed `(phase, migration)`, and planning skips ids already
//! recorded, which is what makes reruns and resumes no-ops.

use crate::schema_shape::{
    ComponentVersion, Installation, SchemaShape, read_component_version, read_search_path,
    resolve_installation,
};
use crate::*;

/// The DDL that creates `lash_migrations`, byte-for-byte the block
/// `schema.sql` carries: the bootstrap and the 133→134 expand step provision
/// the identical table, and a test asserts the bytes agree.
const MIGRATIONS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_migrations (
    phase TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_phase
        CHECK (phase IN ('expand', 'backfill', 'contract')),
    migration TEXT NOT NULL,
    release TEXT NOT NULL,
    state TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_state
        CHECK (state IN ('running', 'applied')),
    from_version INTEGER,
    to_version INTEGER NOT NULL,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    PRIMARY KEY (phase, migration)
);";

/// The DDL that creates `lash_fleet_format`, byte-for-byte the block
/// `schema.sql` carries: the bootstrap and the 135→136 expand step provision
/// the identical table, and a test asserts the bytes agree.
const FLEET_FORMAT_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_fleet_format (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    format_version INTEGER NOT NULL,
    CONSTRAINT ck_fleet_format_singleton CHECK (singleton)
);";

/// One expand-phase step this build's migrate runner can apply.
///
/// `from_version`/`to_version` chain steps together so planning can walk the
/// catalog from any stamped predecessor to `SCHEMA_VERSION`. `statements` are
/// all idempotent (`IF NOT EXISTS` or constraint guards) so a step can replay
/// after a crash without tripping on its own committed output.
struct ExpandMigration {
    /// The stable ledger id; recorded, never renamed.
    id: &'static str,
    /// The stamped component version this step starts from.
    from_version: i32,
    /// The stamped component version after it applies.
    to_version: i32,
    /// Its statements, run in the step's single transaction.
    statements: &'static str,
}

/// The expand catalog this build carries. Pre-1.0 the first step let
/// component 133 gain the ledger itself and become 134 (FIG-3816); the second
/// adds `lash_sessions.pending_follow_on_json`, the frame-handoff follow-on a
/// session head carries, and becomes 135 (FIG-3542); the third adds
/// `lash_fleet_format`, the deployment's fleet-format row, and becomes 136
/// (FIG-3796); the fourth restamps 136 to 137 on a vocabulary-only change
/// (FIG-3814); the fifth adds `lash_session_meta.drive_root_start`, the
/// admitted root's start marker, and becomes 138 (FIG-3815). Newer schema
/// generations append to this list; steps are never
/// removed or edited — the ledger names them permanently.
static EXPAND_MIGRATIONS: &[ExpandMigration] = &[
    ExpandMigration {
        id: "0134-migrations-ledger",
        from_version: 133,
        to_version: 134,
        statements: MIGRATIONS_TABLE_DDL,
    },
    ExpandMigration {
        id: "0135-pending-follow-on",
        from_version: 134,
        to_version: 135,
        statements: "ALTER TABLE lash_sessions ADD COLUMN IF NOT EXISTS pending_follow_on_json TEXT",
    },
    ExpandMigration {
        id: "0136-fleet-format",
        from_version: 135,
        to_version: 136,
        statements: FLEET_FORMAT_TABLE_DDL,
    },
    ExpandMigration {
        id: "0137-runtime-error-vocabulary",
        from_version: 136,
        to_version: 137,
        statements: "-- component 137 (FIG-3814): vocabulary-only change; nothing to apply.",
    },
    ExpandMigration {
        id: "0138-drive-root-start",
        from_version: 137,
        to_version: 138,
        statements: "ALTER TABLE lash_session_meta ADD COLUMN IF NOT EXISTS drive_root_start TEXT",
    },
];

/// The phase a migrate run is asked to execute.
///
/// Only [`MigrationPhase::Expand`] does anything: backfill and contract exist
/// as named phases so operators can spell the full plan today, and both refuse
/// until the operations arc lands (FIG-3817).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    /// Create and alter the objects the new generation reads; safe to run
    /// while old-build workers still serve the catalog.
    Expand,
    /// Populate columns and rows the expand created; runs after the new
    /// workers are rolled out.
    Backfill,
    /// Drop what only the old generation read; runs after backfill completes.
    Contract,
}

impl MigrationPhase {
    /// The phase's ledger and CLI spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Expand => "expand",
            Self::Backfill => "backfill",
            Self::Contract => "contract",
        }
    }

    /// The phase names `lash migrate --phase` accepts.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "expand" => Some(Self::Expand),
            "backfill" => Some(Self::Backfill),
            "contract" => Some(Self::Contract),
            _ => None,
        }
    }

    /// Phases this build refuses to run, expand being the only one defined
    /// before the operations arc (FIG-3817).
    fn unsupported(self) -> Result<(), StoreError> {
        match self {
            Self::Expand => Ok(()),
            Self::Backfill | Self::Contract => Err(StoreError::Backend(format!(
                "`lash migrate --phase {}` is not supported before the operations arc \
                 (FIG-3817): this build executes expand-phase migrations only",
                self.name()
            ))),
        }
    }
}

/// One recorded or planned migrate step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationStep {
    /// `expand`, `backfill`, or `contract`.
    pub phase: String,
    /// The step's ledger id — `bootstrap-{version}` for a fresh provision.
    pub migration: String,
    /// The release that applied the step; empty while the step is only planned.
    pub release: String,
    /// `running` or `applied` — a committed expand row is always `applied`.
    pub state: String,
    /// The stamped version it started from; `None` only for a bootstrap.
    pub from_version: Option<i32>,
    /// The stamped version after it applies.
    pub to_version: i32,
    /// Server-clock instant the step's transaction began; `None` while the step
    /// is only planned.
    pub started_at_ms: Option<i64>,
    /// Server-clock instant the step finished; `None` while it is planned or
    /// still running.
    pub finished_at_ms: Option<i64>,
}

/// What a plan or run found on the database and did about it.
#[derive(Clone, Debug)]
pub struct MigrationReport {
    /// The schema the lash installation resolved to; `None` on an
    /// unprovisioned database.
    pub namespace: Option<String>,
    /// The stamped component version before the run; `None` when the database
    /// had no installation or no readable stamp.
    pub found_version: Option<i32>,
    /// Ledger rows committed before this run, oldest first.
    pub applied: Vec<MigrationStep>,
    /// Steps this run committed, in order. Empty on a rerun and on a plan.
    pub executed: Vec<MigrationStep>,
    /// Steps a run would still commit, in order. On a plan this is the whole
    /// answer; after a run it is empty.
    pub planned: Vec<MigrationStep>,
}

/// What the catalog read says the database is.
struct MigrationState {
    /// The resolved installation, or `None` on an unprovisioned database.
    installation: Option<Installation>,
    /// The component stamp read.
    stamp: Option<ComponentVersion>,
    /// Ledger rows already committed.
    applied: Vec<MigrationStep>,
    /// The writing release, when the release stamp could still be read.
    writing_release: Option<String>,
}

/// One step a plan produced.
enum PlannedStep<'a> {
    /// Provision the whole schema on an unprovisioned database.
    Bootstrap,
    /// Apply a catalog migration.
    Migration(&'a ExpandMigration),
}

/// Reads the stamp, the ledger, and the release stamp inside one transaction
/// snapshot, so a plan describes one instant of the database.
async fn read_state(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<MigrationState, StoreError> {
    let search_path = read_search_path(tx).await?;
    let Some(installation) = resolve_installation(tx, &search_path).await? else {
        return Ok(MigrationState {
            installation: None,
            stamp: None,
            applied: Vec::new(),
            writing_release: None,
        });
    };
    let expected = SchemaShape::expected();
    let stamp = read_component_version(tx, &installation, &expected).await?;
    // Probed by OID against the anchored namespace, like every other object
    // read here: a `to_regclass` name lookup would resolve outside the
    // transaction's snapshot.
    let ledger_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_catalog.pg_class
             WHERE relnamespace = $1 AND relname = 'lash_migrations'
               AND relkind IN ('r', 'p'))",
    )
    .bind(installation.namespace_oid())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let applied = if ledger_present {
        sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                Option<i32>,
                i32,
                i64,
                Option<i64>,
            ),
        >(&format!(
            "SELECT phase, migration, release, state, from_version, to_version,
                        started_at_ms, finished_at_ms
                 FROM {}.lash_migrations ORDER BY started_at_ms, migration",
            installation.quoted_namespace()
        ))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .map(
            |(
                phase,
                migration,
                release,
                state,
                from_version,
                to_version,
                started_at_ms,
                finished_at_ms,
            )| MigrationStep {
                phase,
                migration,
                release,
                state,
                from_version,
                to_version,
                started_at_ms: Some(started_at_ms),
                finished_at_ms,
            },
        )
        .collect()
    } else {
        Vec::new()
    };
    let writing_release = crate::release_stamp::read_release_in_tx(tx).await;
    Ok(MigrationState {
        installation: Some(installation),
        stamp: Some(stamp),
        applied,
        writing_release,
    })
}

/// Turns a read of the database into the ordered steps a run still owes it.
///
/// The walk is the whole planning algorithm: from the found stamp, chain
/// catalog steps whose ids are not yet in the ledger until `SCHEMA_VERSION` is
/// reached. A stamp with no chain is a recreate boundary, refused by the same
/// typed error an open would raise — "no applicable migration" is the honest
/// statement there too.
fn plan(state: &MigrationState) -> Result<Vec<PlannedStep<'_>>, StoreError> {
    let Some(installation) = &state.installation else {
        // Nothing provisioned: the bootstrap applies this build's schema.sql,
        // whose seed statements stamp the current component.
        return Ok(vec![PlannedStep::Bootstrap]);
    };
    let installed = Some(installation.namespace());
    let release = state.writing_release.as_deref();
    let Some(stamp) = &state.stamp else {
        return Ok(Vec::new());
    };
    let found = match stamp {
        ComponentVersion::Unreadable | ComponentVersion::Readable(None) => None,
        ComponentVersion::Readable(Some(version)) => Some(*version),
    };
    let Some(mut at) = found else {
        return Err(version_mismatch_error(installed, None, release));
    };
    if at > SCHEMA_VERSION {
        // A newer build's catalog: Lash never migrates backwards, and the
        // typed refusal says so.
        return Err(version_mismatch_error(installed, found, release));
    }
    let mut pending = Vec::new();
    while at < SCHEMA_VERSION {
        let Some(migration) = EXPAND_MIGRATIONS
            .iter()
            .find(|migration| migration.from_version == at)
        else {
            return Err(version_mismatch_error(installed, found, release));
        };
        // A committed ledger row means this step finished; skip it and keep
        // walking, which is what makes a resumed run pick up where it stopped.
        if !state
            .applied
            .iter()
            .any(|step| step.migration == migration.id)
        {
            pending.push(PlannedStep::Migration(migration));
        }
        at = migration.to_version;
    }
    Ok(pending)
}

/// The server clock's current instant in epoch milliseconds — the ledger's
/// timestamps are the database's, not the migrating host's, the same rule the
/// release stamp follows.
async fn server_clock_ms(tx: &mut sqlx::Transaction<'_, Postgres>) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)")
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

/// Records one applied step in the ledger and returns its committed row's
/// instants. `ON CONFLICT DO NOTHING` keeps the statement honest when a row
/// survived from a run whose planning missed it; the read-back then returns
/// the original row rather than rewriting history.
async fn record_step(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    ledger: &str,
    phase: &str,
    migration: &str,
    from_version: Option<i32>,
    to_version: i32,
    started_at_ms: i64,
) -> Result<(String, String, i64, Option<i64>), StoreError> {
    let recorded: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(&format!(
        "INSERT INTO {ledger} (phase, migration, release, state,
                               from_version, to_version, started_at_ms, finished_at_ms)
         VALUES ($1, $2, $3, 'applied', $4, $5, $6,
                 CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT))
         ON CONFLICT (phase, migration) DO NOTHING
         RETURNING release, state, started_at_ms, finished_at_ms"
    ))
    .bind(phase)
    .bind(migration)
    .bind(crate::release_stamp::BUILD_RELEASE)
    .bind(from_version)
    .bind(to_version)
    .bind(started_at_ms)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    match recorded {
        Some(row) => Ok(row),
        None => sqlx::query_as(&format!(
            "SELECT release, state, started_at_ms, finished_at_ms FROM {ledger}
             WHERE phase = $1 AND migration = $2"
        ))
        .bind(phase)
        .bind(migration)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error),
    }
}

/// Applies one planned step in its own transaction: the step's statements, the
/// component-stamp move to `to_version`, the ledger row, and the release stamp
/// all commit or roll back together.
async fn apply_step(
    connection: &mut sqlx::PgConnection,
    installation: Option<&Installation>,
    step: &PlannedStep<'_>,
) -> Result<MigrationStep, StoreError> {
    let mut tx = sqlx::Connection::begin(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
    let started_at_ms = server_clock_ms(&mut tx).await?;
    let (migration, from_version, to_version, ledger) = match step {
        PlannedStep::Bootstrap => {
            sqlx::raw_sql(SCHEMA_DDL)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            // The bootstrap just created the ledger in the search_path's first
            // schema, so the unqualified name resolves to it.
            (
                format!("bootstrap-{SCHEMA_VERSION}"),
                None,
                SCHEMA_VERSION,
                "lash_migrations".to_string(),
            )
        }
        PlannedStep::Migration(migration) => {
            // plan only produces a catalog step for a resolved installation.
            let Some(installation) = installation else {
                return Err(StoreError::Backend(
                    "a catalog migration was planned for a database with no lash installation"
                        .to_string(),
                ));
            };
            sqlx::raw_sql(migration.statements)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            // Moving the stamp is part of the step: a later open sees either
            // the whole committed step or the version it started from.
            sqlx::query(&format!(
                "INSERT INTO {}.lash_schema_versions (component, version)
                 VALUES ($1, $2)
                 ON CONFLICT (component) DO UPDATE SET version = EXCLUDED.version",
                installation.quoted_namespace()
            ))
            .bind(SCHEMA_COMPONENT)
            .bind(migration.to_version)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            (
                migration.id.to_string(),
                Some(migration.from_version),
                migration.to_version,
                format!("{}.lash_migrations", installation.quoted_namespace()),
            )
        }
    };
    let (release, state, started_at_ms, finished_at_ms) = record_step(
        &mut tx,
        &ledger,
        MigrationPhase::Expand.name(),
        &migration,
        from_version,
        to_version,
        started_at_ms,
    )
    .await?;
    crate::release_stamp::write(&mut tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(MigrationStep {
        phase: MigrationPhase::Expand.name().to_string(),
        migration,
        release,
        state,
        from_version,
        to_version,
        started_at_ms: Some(started_at_ms),
        finished_at_ms,
    })
}

fn report(state: &MigrationState, executed: Vec<MigrationStep>) -> MigrationReport {
    MigrationReport {
        namespace: state
            .installation
            .as_ref()
            .map(|installation| installation.namespace().to_string()),
        found_version: match &state.stamp {
            Some(ComponentVersion::Readable(version)) => *version,
            Some(ComponentVersion::Unreadable) | None => None,
        },
        applied: state.applied.clone(),
        executed,
        planned: Vec::new(),
    }
}

/// Takes the advisory lock in the requested mode on a detached connection.
///
/// Session-scoped locks demand this shape — the connection is owned, so a
/// cancelled future or an error path still releases the lock when the session
/// closes rather than handing a locked connection back to the pool. See
/// [`verify_schema_under_advisory_lock`] for the full reasoning.
async fn lock_connection(pool: &PgPool, shared: bool) -> Result<sqlx::PgConnection, StoreError> {
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    let locked = sqlx::query(if shared {
        "SELECT pg_advisory_lock_shared($1, $2)"
    } else {
        "SELECT pg_advisory_lock($1, $2)"
    })
    .bind(lock_namespace)
    .bind(lock_key)
    .execute(&mut connection)
    .await
    .map_err(store_sqlx_error);
    match locked {
        Ok(_) => Ok(connection),
        Err(error) => {
            let _ = sqlx::Connection::close(connection).await;
            Err(error)
        }
    }
}

/// Reads the migrate state under a `REPEATABLE READ` snapshot: the lock the
/// caller holds is session-scoped, so the transaction must start only after
/// the lock landed — otherwise the snapshot would predate it.
async fn read_state_under_lock(
    connection: &mut sqlx::PgConnection,
) -> Result<MigrationState, StoreError> {
    let mut tx = sqlx::Connection::begin(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let state = read_state(&mut tx).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(state)
}

fn planned_step(step: &PlannedStep<'_>) -> MigrationStep {
    match step {
        PlannedStep::Bootstrap => MigrationStep {
            phase: MigrationPhase::Expand.name().to_string(),
            migration: format!("bootstrap-{SCHEMA_VERSION}"),
            release: String::new(),
            state: String::new(),
            from_version: None,
            to_version: SCHEMA_VERSION,
            started_at_ms: None,
            finished_at_ms: None,
        },
        PlannedStep::Migration(migration) => MigrationStep {
            phase: MigrationPhase::Expand.name().to_string(),
            migration: migration.id.to_string(),
            release: String::new(),
            state: String::new(),
            from_version: Some(migration.from_version),
            to_version: migration.to_version,
            started_at_ms: None,
            finished_at_ms: None,
        },
    }
}

/// Plans what an expand run owes the database without changing it: the shared
/// advisory lock and the repeatable-read snapshot are exactly what a verifying
/// open takes, so the answer cannot describe a half-applied state.
pub(crate) async fn plan_on(
    pool: &PgPool,
    phase: MigrationPhase,
) -> Result<MigrationReport, StoreError> {
    phase.unsupported()?;
    let mut connection = lock_connection(pool, true).await?;
    let result = async {
        let state = read_state_under_lock(&mut connection).await?;
        let pending = plan(&state)?;
        let mut planned_report = report(&state, Vec::new());
        planned_report.planned = pending.iter().map(planned_step).collect();
        Ok(planned_report)
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// Runs the pending expand migrations under the exclusive advisory lock.
///
/// The lock is the same key every verifying open holds while it reads the
/// catalog, so a worker can never verify against a half-applied batch and a
/// second `lash migrate` queues behind rather than racing this one.
pub(crate) async fn migrate_on(
    pool: &PgPool,
    phase: MigrationPhase,
) -> Result<MigrationReport, StoreError> {
    phase.unsupported()?;
    let mut connection = lock_connection(pool, false).await?;
    let result = async {
        let state = read_state_under_lock(&mut connection).await?;
        let pending = plan(&state)?;
        let mut executed = Vec::with_capacity(pending.len());
        for step in &pending {
            executed.push(apply_step(&mut connection, state.installation.as_ref(), step).await?);
        }
        // A run that changed the catalog proves it before releasing the lock:
        // the structural check is the same one an open would run, so a
        // migrated database that cannot open fails here, not at the first
        // worker's startup. The verification also resolves the namespace a
        // bootstrap just installed — the pre-run state had none.
        let mut result = report(&state, executed);
        if !result.executed.is_empty() {
            let verification = verify_schema_shape(&mut connection).await?;
            if !verification.is_conformant() {
                return Err(StoreError::Backend(format!(
                    "`lash migrate` applied {} step(s) but the resulting schema is not \
                     conformant — do not start workers against it: {verification}",
                    result.executed.len()
                )));
            }
            if result.namespace.is_none() {
                result.namespace = verification.schema;
            }
        }
        Ok(result)
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// Provisions or advances the schema for `database_url` with default pool
/// settings — what `lash migrate` invokes.
pub async fn migrate(
    database_url: &str,
    phase: MigrationPhase,
) -> Result<MigrationReport, StoreError> {
    let pool = migrate_pool(database_url).await?;
    let result = migrate_on(&pool, phase).await;
    pool.close().await;
    result
}

/// Plans what [`migrate`] would apply without changing the database.
pub async fn plan_migrations(
    database_url: &str,
    phase: MigrationPhase,
) -> Result<MigrationReport, StoreError> {
    let pool = migrate_pool(database_url).await?;
    let result = plan_on(&pool, phase).await;
    pool.close().await;
    result
}

/// The migrate runner needs two connections at most — one holds the advisory
/// lock while it works — and the usual statement timeouts, not a worker's
/// pool shape.
async fn migrate_pool(database_url: &str) -> Result<PgPool, StoreError> {
    postgres_pool_options(&PostgresStoreConfig {
        max_connections: 2,
        ..PostgresStoreConfig::default()
    })
    .connect(database_url)
    .await
    .map_err(store_sqlx_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upgrade path and the bootstrap provision identical objects: a
    /// catalog step that creates an object states it exactly the way
    /// `schema.sql` does, so the two paths can never disagree about that
    /// object's shape. A step that alters an existing table carries no byte
    /// contract with the artifact — `schema.sql` folds the added column into
    /// its `CREATE TABLE` body — so the structural suites own that
    /// equivalence.
    #[test]
    fn created_objects_match_the_schema_artifact() {
        for migration in EXPAND_MIGRATIONS {
            if migration.statements.starts_with("CREATE") {
                assert!(
                    SCHEMA_DDL.contains(migration.statements),
                    "schema.sql does not contain {}'s DDL verbatim",
                    migration.id
                );
            }
        }
    }

    /// The catalog must chain to the current component: a step targeting a
    /// version the build no longer stamps would leave planning stuck.
    #[test]
    fn the_expand_catalog_chains_to_the_current_component() {
        let mut at = SCHEMA_VERSION;
        while let Some(migration) = EXPAND_MIGRATIONS
            .iter()
            .find(|migration| migration.to_version == at)
        {
            assert_eq!(
                migration.from_version,
                at - 1,
                "{} does not chain from the previous component",
                migration.id
            );
            at = migration.from_version;
        }
    }

    #[test]
    fn phases_round_trip_through_their_names() {
        for phase in [
            MigrationPhase::Expand,
            MigrationPhase::Backfill,
            MigrationPhase::Contract,
        ] {
            assert_eq!(MigrationPhase::parse(phase.name()), Some(phase));
        }
        assert_eq!(MigrationPhase::parse("sideways"), None);
    }

    /// Backfill and contract refuse before the operations arc — the refusal
    /// names FIG-3817 verbatim so the ticket's operators can find it.
    #[test]
    fn later_phases_refuse_with_the_operations_arc_refusal() {
        for phase in [MigrationPhase::Backfill, MigrationPhase::Contract] {
            let error = phase.unsupported().unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("not supported before the operations arc (FIG-3817)"),
                "{phase:?} refusal does not name FIG-3817: {message}"
            );
        }
        assert!(MigrationPhase::Expand.unsupported().is_ok());
    }
}
