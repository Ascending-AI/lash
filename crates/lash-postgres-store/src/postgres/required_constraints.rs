//! Explicit read-only inspection of registered PostgreSQL `CHECK`s.

use std::collections::BTreeMap;

use lash_core::StoreError;
use lash_core::store_backend_support::required_constraints::{
    EXPECTED_CONSTRAINTS, EXPECTED_FOREIGN_KEYS, InspectedConstraint, InspectedForeignKey,
    RenderedConstraint, RenderedForeignKey, RequiredConstraintReport, compare_required_constraints,
    compare_required_foreign_keys,
};
use sqlx::{Connection as _, PgConnection, PgPool, Row};

use super::schema_shape::{read_search_path, resolve_installation, resolve_tables};
use crate::{SCHEMA_ADVISORY_LOCK_KEY, store_sqlx_error};

pub(crate) async fn inspect_required_constraints_under_advisory_lock(
    pool: &PgPool,
) -> Result<RequiredConstraintReport, StoreError> {
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    let inspected = async {
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_shared_by_pair
                .sql(),
        )
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut connection)
        .await
        .map_err(store_sqlx_error)?;
        let mut transaction = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read
                .sql(),
        )
        .execute(&mut *transaction)
        .await
        .map_err(store_sqlx_error)?;
        let report = inspect_required_constraints(&mut transaction).await?;
        transaction.commit().await.map_err(store_sqlx_error)?;
        Ok(report)
    }
    .await;
    let _ = connection.close().await;
    inspected
}

/// Maps the single-character `pg_constraint.confdeltype`/`confupdtype`
/// encoding onto the canonical action spellings the registry pins.
fn referential_action(code: &str) -> String {
    match code {
        "r" => "restrict",
        "c" => "cascade",
        "n" => "set null",
        "d" => "set default",
        _ => "no action",
    }
    .to_string()
}

pub(crate) async fn inspect_required_constraints(
    connection: &mut PgConnection,
) -> Result<RequiredConstraintReport, StoreError> {
    let expected: Vec<RenderedConstraint> = EXPECTED_CONSTRAINTS
        .iter()
        .filter_map(|constraint| constraint.postgres)
        .collect();
    let expected_foreign_keys: Vec<RenderedForeignKey> = EXPECTED_FOREIGN_KEYS
        .iter()
        .filter_map(|key| key.postgres)
        .collect();
    let search_path = read_search_path(connection).await?;
    let Some(installation) = resolve_installation(connection, &search_path).await? else {
        let mut report = compare_required_constraints("postgres", &expected, Vec::new())?;
        report.set_foreign_key_findings(compare_required_foreign_keys(
            "postgres",
            &expected_foreign_keys,
            Vec::new(),
        )?);
        return Ok(report);
    };
    let table_names = expected
        .iter()
        .map(|constraint| constraint.table.to_string())
        .chain(
            expected_foreign_keys
                .iter()
                .map(|key| key.table.to_string()),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let resolved = resolve_tables(connection, &installation, &table_names).await?;
    let by_oid = resolved
        .iter()
        .map(|(name, table)| (table.oid(), name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let table_oids = by_oid.keys().copied().collect::<Vec<_>>();
    let rows = sqlx::query(crate::connection_sql::SELECT_CHECK_CONSTRAINTS)
        .bind(&table_oids)
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;

    let mut actual = Vec::new();
    for row in rows {
        let table_oid: i64 = row.get("table_oid");
        let Some(table) = by_oid.get(&table_oid) else {
            continue;
        };
        let name: String = row.get("name");
        if !expected
            .iter()
            .any(|expected| expected.table == *table && expected.name == name)
        {
            continue;
        }
        actual.push(InspectedConstraint {
            table: (*table).to_string(),
            name,
            expression: row.get("expression"),
            validated: row.get("validated"),
            enforced: row.get("enforced"),
        });
    }
    let mut report = compare_required_constraints("postgres", &expected, actual)?;

    let foreign_key_rows = sqlx::query(crate::connection_sql::SELECT_FOREIGN_KEY_CONSTRAINTS)
        .bind(&table_oids)
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
    let mut actual_foreign_keys = Vec::new();
    for row in foreign_key_rows {
        let table_oid: i64 = row.get("table_oid");
        let Some(table) = by_oid.get(&table_oid) else {
            continue;
        };
        actual_foreign_keys.push(InspectedForeignKey {
            table: (*table).to_string(),
            columns: row.get("columns"),
            referenced_table: row.get("referenced_table"),
            referenced_columns: row.get("referenced_columns"),
            on_delete: referential_action(&row.get::<String, _>("on_delete")),
            on_update: referential_action(&row.get::<String, _>("on_update")),
            deferrable: row.get("deferrable"),
            initially_deferred: row.get("initially_deferred"),
            validated: row.get("validated"),
            enforced: row.get("enforced"),
        });
    }
    report.set_foreign_key_findings(compare_required_foreign_keys(
        "postgres",
        &expected_foreign_keys,
        actual_foreign_keys,
    )?);
    Ok(report)
}
