//! Explicit read-only inspection of Lash's registered SQLite `CHECK`s.

use std::collections::BTreeMap;
use std::path::Path;

use lash_core_execution::StoreError;
use lash_core_execution::store_backend_support::required_constraints::{
    EXPECTED_CONSTRAINTS, EXPECTED_FOREIGN_KEYS, InspectedConstraint, InspectedForeignKey,
    RequiredConstraintReport, SqliteConstraintDatabase, compare_required_constraints,
    compare_required_foreign_keys, extract_foreign_key_clauses, extract_named_check_expressions,
};

use crate::{SqliteDatabase, conn::SqliteConnection, sqlite_error};

/// Inspect the registered named `CHECK`s in one existing SQLite database.
///
/// The database is opened with SQLite's read-only flag. This does not provision
/// a missing database, apply DDL, stamp a version, change rows, or run during
/// normal store startup. SQLite may create recoverable `-shm`/`-wal` sidecars
/// while opening a WAL database read-only; inspection preserves durable main
/// database and WAL content and reads committed changes still held in the WAL.
///
/// An empty report establishes only that every registered named check for
/// `database` matched in this read; it says nothing about the schema version,
/// openability, unregistered checks, or existing row integrity.
///
/// ```no_run
/// # async fn inspect(path: &std::path::Path) -> Result<(), lash_core_execution::StoreError> {
/// let report = lash_sqlite_store::inspect_required_constraints_at(
///     path,
///     lash_sqlite_store::SqliteDatabase::DurableCore,
/// ).await?;
/// assert!(report.is_conformant(), "{report:?}");
/// # Ok(())
/// # }
/// ```
pub async fn inspect_required_constraints_at(
    path: impl AsRef<Path>,
    database: SqliteDatabase,
) -> Result<RequiredConstraintReport, StoreError> {
    let connection = SqliteConnection::open_readonly(&crate::location::DatabaseTarget::File(
        path.as_ref().to_path_buf(),
    ))
    .await
    .map_err(sqlite_async_error)?;
    let tables = connection
        .call(|connection| {
            let mut statement = connection.prepare(crate::connection_sql::SELECT_TABLE_DDL)?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(sqlite_error)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();

    let mut expected = Vec::new();
    for constraint in EXPECTED_CONSTRAINTS {
        if constraint
            .sqlite_databases
            .iter()
            .any(|component| sqlite_database(*component) == database)
            && let Some(sqlite) = constraint.sqlite
        {
            expected.push(sqlite);
        }
    }
    let mut actual = Vec::new();
    for constraint in &expected {
        let Some(ddl) = tables.get(constraint.table) else {
            continue;
        };
        let checks = extract_named_check_expressions(ddl).map_err(|detail| {
            StoreError::RequiredConstraintInspectionInconclusive {
                backend: "sqlite",
                table: constraint.table.to_string(),
                constraint: constraint.name.to_string(),
                detail,
            }
        })?;
        if let Some(expression) = checks.get(constraint.name) {
            actual.push(InspectedConstraint {
                table: constraint.table.to_string(),
                name: constraint.name.to_string(),
                expression: expression.clone(),
                validated: true,
                enforced: true,
            });
        }
    }
    let mut report = compare_required_constraints("sqlite", &expected, actual)?;

    let mut expected_foreign_keys = Vec::new();
    for key in EXPECTED_FOREIGN_KEYS {
        if key
            .sqlite_databases
            .iter()
            .any(|component| sqlite_database(*component) == database)
            && let Some(sqlite) = key.sqlite
        {
            expected_foreign_keys.push(sqlite);
        }
    }
    let mut actual_foreign_keys = Vec::new();
    for (table, ddl) in &tables {
        for clause in extract_foreign_key_clauses(ddl).map_err(|detail| {
            StoreError::RequiredConstraintInspectionInconclusive {
                backend: "sqlite",
                table: table.clone(),
                constraint: "<foreign key>".to_string(),
                detail,
            }
        })? {
            actual_foreign_keys.push(InspectedForeignKey {
                table: table.clone(),
                columns: clause.columns,
                referenced_table: clause.referenced_table,
                referenced_columns: clause.referenced_columns,
                on_delete: clause.on_delete,
                on_update: clause.on_update,
                deferrable: clause.deferrable,
                initially_deferred: clause.initially_deferred,
                validated: true,
                enforced: true,
            });
        }
    }
    report.set_foreign_key_findings(compare_required_foreign_keys(
        "sqlite",
        &expected_foreign_keys,
        actual_foreign_keys,
    )?);
    Ok(report)
}

fn sqlite_async_error(error: tokio_rusqlite::Error) -> StoreError {
    match error {
        tokio_rusqlite::Error::Error(error) => sqlite_error(error),
        error => StoreError::StorageFailure {
            backend: "sqlite",
            message: error.to_string(),
        },
    }
}

fn sqlite_database(database: SqliteConstraintDatabase) -> SqliteDatabase {
    match database {
        SqliteConstraintDatabase::DurableCore => SqliteDatabase::DurableCore,
        SqliteConstraintDatabase::ProcessRegistry => SqliteDatabase::ProcessRegistry,
        SqliteConstraintDatabase::Triggers => SqliteDatabase::Triggers,
        SqliteConstraintDatabase::EffectReplay => SqliteDatabase::EffectReplay,
    }
}
