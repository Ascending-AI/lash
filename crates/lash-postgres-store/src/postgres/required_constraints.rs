//! Explicit read-only inspection of registered PostgreSQL `CHECK`s.

use std::collections::BTreeMap;

use lash_core::StoreError;
use lash_core::store_backend_support::required_constraints::{
    InspectedConstraint, POSTGRES_EXPECTED_CONSTRAINTS, RequiredConstraintReport,
    compare_required_constraints,
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
        sqlx::query("SELECT pg_advisory_lock_shared($1, $2)")
            .bind(lock_namespace)
            .bind(lock_key)
            .execute(&mut connection)
            .await
            .map_err(store_sqlx_error)?;
        let mut transaction = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
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

pub(crate) async fn inspect_required_constraints(
    connection: &mut PgConnection,
) -> Result<RequiredConstraintReport, StoreError> {
    let search_path = read_search_path(connection).await?;
    let Some(installation) = resolve_installation(connection, &search_path).await? else {
        return compare_required_constraints("postgres", POSTGRES_EXPECTED_CONSTRAINTS, Vec::new());
    };
    let table_names = POSTGRES_EXPECTED_CONSTRAINTS
        .iter()
        .map(|constraint| constraint.table.to_string())
        .collect::<Vec<_>>();
    let resolved = resolve_tables(connection, &installation, &table_names).await?;
    let by_oid = resolved
        .iter()
        .map(|(name, table)| (table.oid(), name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let table_oids = by_oid.keys().copied().collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT c.conrelid::bigint AS table_oid,
                c.conname::text AS name,
                c.convalidated AS validated,
                COALESCE(
                    (pg_catalog.to_jsonb(c) ->> 'conenforced')::boolean,
                    TRUE
                ) AS enforced,
                pg_catalog.pg_get_expr(
                    c.conbin,
                    c.conrelid,
                    false
                ) AS expression
         FROM pg_catalog.pg_constraint AS c
         WHERE c.contype = 'c'
           AND c.conrelid::bigint = ANY($1::bigint[])",
    )
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
        if !POSTGRES_EXPECTED_CONSTRAINTS
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
    compare_required_constraints("postgres", POSTGRES_EXPECTED_CONSTRAINTS, actual)
}
