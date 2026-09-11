//! Explicit read-only inspection of Lash's registered SQLite `CHECK`s.

use std::collections::BTreeMap;
use std::path::Path;

use lash_core::StoreError;
use lash_core::store_backend_support::required_constraints::{
    InspectedConstraint, RequiredConstraintReport, SQLITE_EXPECTED_CONSTRAINTS,
    SqliteConstraintDatabase, compare_required_constraints, extract_named_check_expressions,
};

use crate::{SqliteDatabase, conn::SqliteConnection, sqlite_error};

/// Inspect the registered named `CHECK`s in one existing SQLite database.
///
/// The file is opened read-only. This does not create a database, apply DDL,
/// repair rows, or run during normal store startup. An empty report establishes
/// only that every registered named check for `database` matched in this read;
/// it says nothing about the schema version, openability, unregistered checks,
/// or existing row integrity.
///
/// ```no_run
/// # async fn inspect(path: &std::path::Path) -> Result<(), lash_core::StoreError> {
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
    let connection = SqliteConnection::open_readonly(path.as_ref())
        .await
        .map_err(sqlite_async_error)?;
    let tables = connection
        .call(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, sql
                 FROM sqlite_schema
                 WHERE type = 'table' AND sql IS NOT NULL",
            )?;
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
    for constraint in SQLITE_EXPECTED_CONSTRAINTS {
        let Some(component) = constraint.sqlite_database else {
            return Err(StoreError::RequiredConstraintInspectionInconclusive {
                backend: "sqlite",
                table: constraint.table.to_string(),
                constraint: constraint.name.to_string(),
                detail: "registered SQLite constraint has no database component".to_string(),
            });
        };
        if sqlite_database(component) == database {
            expected.push(*constraint);
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
    compare_required_constraints("sqlite", &expected, actual)
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
