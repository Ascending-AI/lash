//! Catalog introspection: reads the live shape of lash's tables and the data
//! preconditions no structural comparison can see.
//!
//! Separated from the shape model in the parent module so that the model — the
//! artifact format, the diff, and the rendering — never depends on how a shape is
//! obtained, and the SQL lives in one place with the portability notes that
//! explain it.

use std::collections::BTreeMap;

use sqlx::{PgConnection, Row};

use super::{
    ANCHOR_TABLE, AWAIT_EVENT_SIGNING_SECRET_BYTES, ColumnShape, ColumnValueSource,
    ForeignKeyAction, ForeignKeyShape, SEED_ROWS, SchemaFinding, SchemaReport, SchemaShape,
    TableShape, UniqueGuard,
};
use crate::{SCHEMA_COMPONENT, SCHEMA_VERSION, StoreError, store_sqlx_error};

/// The single lash installation a verification reads.
///
/// Anchoring on one namespace is what makes the check a statement about a
/// database rather than about a set of objects: `search_path` can front a partial
/// installation with another, and resolving each table on its own would accept
/// the union of two halves that runtime writes then split across.
pub(crate) struct Installation {
    namespace_oid: i64,
    /// Namespace name, already quoted for interpolation into a statement.
    quoted_namespace: String,
    /// Namespace name as the catalog reports it.
    namespace: String,
}

/// Resolves the namespace holding this database's lash installation.
///
/// Returns `None` when `lash_schema_versions` does not resolve at all, which is
/// the unprovisioned database.
pub(crate) async fn resolve_installation(
    connection: &mut PgConnection,
) -> Result<Option<Installation>, StoreError> {
    let row = sqlx::query(
        "SELECT namespace.oid::bigint AS namespace_oid,
                namespace.nspname::text AS namespace,
                pg_catalog.quote_ident(namespace.nspname) AS quoted_namespace
         FROM pg_catalog.pg_class AS relation
         JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace
         WHERE relation.oid = pg_catalog.to_regclass(pg_catalog.quote_ident($1))::oid
           AND relation.relkind IN ('r', 'p')",
    )
    .bind(ANCHOR_TABLE)
    .fetch_optional(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    Ok(row.map(|row| Installation {
        namespace_oid: row.get("namespace_oid"),
        namespace: row.get("namespace"),
        quoted_namespace: row.get("quoted_namespace"),
    }))
}

/// Reads the component version stamp from the anchored installation.
///
/// Qualified by namespace rather than resolved through `search_path`, so the
/// version this reports is the version of the installation the rest of the check
/// reads. Tolerates a `lash_schema_versions` whose own columns are mis-ported:
/// that surfaces as an unstamped database plus the column diff, never as a raw
/// Postgres error.
pub(crate) async fn read_component_version(
    connection: &mut PgConnection,
    installation: &Installation,
) -> Result<Option<i32>, StoreError> {
    let readable: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
         FROM pg_catalog.pg_attribute
         WHERE attrelid = ($1 || '.' || pg_catalog.quote_ident($2))::regclass
           AND attname IN ('component', 'version')
           AND attnum > 0
           AND NOT attisdropped",
    )
    .bind(&installation.quoted_namespace)
    .bind(ANCHOR_TABLE)
    .fetch_one(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    if !readable {
        return Ok(None);
    }
    sqlx::query_scalar(&format!(
        "SELECT version FROM {}.{ANCHOR_TABLE} WHERE component = $1",
        installation.quoted_namespace
    ))
    .bind(SCHEMA_COMPONENT)
    .fetch_optional(&mut *connection)
    .await
    .map_err(store_sqlx_error)
}

/// Reads the live shape of lash's tables and diffs it against this build's
/// expectation, then checks the seed rows the structural diff cannot see.
///
/// A version-stamp mismatch short-circuits the structural diff: the database is
/// a different schema generation, so a per-column diff of it is noise rather
/// than a diagnosis.
pub(crate) async fn verify_schema_shape(
    connection: &mut PgConnection,
) -> Result<SchemaReport, StoreError> {
    let expected = SchemaShape::expected();
    let table_names: Vec<String> = expected.tables.keys().cloned().collect();
    let Some(installation) = resolve_installation(connection).await? else {
        return Ok(SchemaReport {
            schema: None,
            expected_version: SCHEMA_VERSION,
            found_version: None,
            findings: vec![SchemaFinding::VersionMismatch {
                expected: SCHEMA_VERSION,
                found: None,
            }],
        });
    };
    let found_version = read_component_version(connection, &installation).await?;
    let mut report = SchemaReport {
        schema: Some(installation.namespace.clone()),
        expected_version: SCHEMA_VERSION,
        found_version,
        findings: Vec::new(),
    };
    if found_version != Some(SCHEMA_VERSION) {
        report.findings.push(SchemaFinding::VersionMismatch {
            expected: SCHEMA_VERSION,
            found: found_version,
        });
        return Ok(report);
    }
    report.findings = read_shadow_findings(connection, &installation, &table_names).await?;
    let resolved = resolve_tables(connection, &installation, &table_names).await?;
    let found = read_live_shape(connection, &resolved).await?;
    report.findings.extend(expected.diff(&found));
    report
        .findings
        .extend(read_seed_row_findings(connection, &installation, &found).await?);
    Ok(report)
}

/// A lash table in the anchored installation.
pub(crate) struct ResolvedTable {
    oid: i64,
}

/// Reports every lash-named relation that `search_path` resolves outside the
/// anchored namespace.
///
/// These are the relations lash's own unqualified statements would hit, so a
/// database that has them is not the installation the rest of the check just
/// verified — regardless of whether the anchored copy is itself perfect.
async fn read_shadow_findings(
    connection: &mut PgConnection,
    installation: &Installation,
    table_names: &[String],
) -> Result<Vec<SchemaFinding>, StoreError> {
    let rows = sqlx::query(
        "SELECT expected.name AS name,
                namespace.nspname::text AS found_schema
         FROM unnest($1::text[]) AS expected(name)
         JOIN pg_catalog.pg_class AS relation
             ON relation.oid = pg_catalog.to_regclass(pg_catalog.quote_ident(expected.name))::oid
         JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace
         WHERE relation.relnamespace <> $2::oid",
    )
    .bind(table_names)
    .bind(installation.namespace_oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    Ok(rows
        .into_iter()
        .map(|row| SchemaFinding::ShadowedTable {
            table: row.get("name"),
            expected_schema: installation.namespace.clone(),
            found_schema: row.get("found_schema"),
        })
        .collect())
}

/// Looks every expected table up by `(namespace, name)` inside the anchored
/// installation. A table absent from it is missing even if a same-named relation
/// exists elsewhere on the `search_path`.
pub(crate) async fn resolve_tables(
    connection: &mut PgConnection,
    installation: &Installation,
    table_names: &[String],
) -> Result<BTreeMap<String, ResolvedTable>, StoreError> {
    let rows = sqlx::query(
        "SELECT relation.relname::text AS name,
                relation.oid::bigint AS oid
         FROM unnest($1::text[]) AS expected(name)
         JOIN pg_catalog.pg_class AS relation
             ON relation.relnamespace = $2::oid
            AND relation.relname = expected.name
            AND relation.relkind IN ('r', 'p')",
    )
    .bind(table_names)
    .bind(installation.namespace_oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    let mut resolved = BTreeMap::new();
    for row in rows {
        resolved.insert(
            row.get::<String, _>("name"),
            ResolvedTable {
                oid: row.get("oid"),
            },
        );
    }
    Ok(resolved)
}

pub(crate) async fn read_live_shape(
    connection: &mut PgConnection,
    resolved: &BTreeMap<String, ResolvedTable>,
) -> Result<SchemaShape, StoreError> {
    let oids: Vec<i64> = resolved.values().map(|table| table.oid).collect();
    let by_oid: BTreeMap<i64, &str> = resolved
        .iter()
        .map(|(name, table)| (table.oid, name.as_str()))
        .collect();
    let mut shape = SchemaShape::default();
    for name in resolved.keys() {
        shape.tables.insert(name.clone(), TableShape::default());
    }
    let table_of = |oid: i64| by_oid.get(&oid).copied();

    // Nullability comes from `pg_attribute.attnotnull`, which is stable across
    // every supported major. PostgreSQL 18 additionally materializes NOT NULL as
    // `pg_constraint` rows with `contype = 'n'`; nothing here enumerates
    // `pg_constraint` unfiltered, so those rows cannot enter the comparison.
    let column_rows = sqlx::query(
        "SELECT attribute.attrelid::bigint AS table_oid,
                attribute.attname::text AS column_name,
                pg_catalog.format_type(attribute.atttypid, attribute.atttypmod) AS sql_type,
                attribute.attnotnull AS not_null,
                attribute.attidentity::text AS identity,
                attribute.attgenerated::text AS generated,
                attribute.atthasdef AS has_default
         FROM pg_catalog.pg_attribute AS attribute
         WHERE attribute.attrelid::bigint = ANY($1::bigint[])
           AND attribute.attnum > 0
           AND NOT attribute.attisdropped",
    )
    .bind(&oids)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    for row in column_rows {
        let Some(table) = table_of(row.get("table_oid")) else {
            continue;
        };
        let column = ColumnShape {
            name: row.get("column_name"),
            sql_type: row.get("sql_type"),
            nullable: !row.get::<bool, _>("not_null"),
            value_source: ColumnValueSource::from_catalog(
                &row.get::<String, _>("identity"),
                &row.get::<String, _>("generated"),
                row.get("has_default"),
            ),
        };
        shape
            .tables
            .get_mut(table)
            .expect("every resolved table was seeded")
            .columns
            .insert(column.name.clone(), column);
    }

    // Every uniqueness guarantee is read from `pg_index`, not `pg_constraint`:
    // the exactly-once dedup guard is a partial unique index with no constraint
    // row, and a constraints-only view would silently miss its absence.
    // The `indnkeyatts` bound trims trailing INCLUDE columns, which carry no
    // uniqueness. `indkey` is an `int2vector` with a zero lower bound, so the
    // ordinality of `unnest` — always one-based — is what the bound applies to.
    // `indnullsnotdistinct` is read through `to_jsonb` rather than named directly:
    // the column does not exist before PostgreSQL 15, where naming it is a parse
    // error, and `->>` on an absent key yields NULL, which normalizes to the
    // pre-15 behaviour of NULLs always being distinct.
    let index_rows = sqlx::query(
        "SELECT index_catalog.indrelid::bigint AS table_oid,
                index_catalog.indisprimary AS is_primary,
                ARRAY(
                    SELECT COALESCE(attribute.attname::text, '<expression>')
                    FROM unnest(index_catalog.indkey::int2[])
                        WITH ORDINALITY AS key(attnum, ordinality)
                    LEFT JOIN pg_catalog.pg_attribute AS attribute
                        ON attribute.attrelid = index_catalog.indrelid
                       AND attribute.attnum = key.attnum
                    WHERE key.ordinality <= index_catalog.indnkeyatts
                    ORDER BY key.ordinality
                ) AS columns,
                pg_catalog.pg_get_expr(index_catalog.indpred, index_catalog.indrelid) AS predicate,
                COALESCE(
                    (pg_catalog.to_jsonb(index_catalog) ->> 'indnullsnotdistinct')::boolean,
                    false
                ) AS nulls_not_distinct
         FROM pg_catalog.pg_index AS index_catalog
         WHERE index_catalog.indrelid::bigint = ANY($1::bigint[])
           AND index_catalog.indisunique
           AND index_catalog.indisvalid
           AND index_catalog.indislive",
    )
    .bind(&oids)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    for row in index_rows {
        let Some(table) = table_of(row.get("table_oid")) else {
            continue;
        };
        let guard = UniqueGuard {
            primary_key: row.get("is_primary"),
            columns: row.get("columns"),
            predicate: row
                .get::<Option<String>, _>("predicate")
                .map(|predicate| normalize_predicate(&predicate)),
            nulls_not_distinct: row.get("nulls_not_distinct"),
        };
        shape
            .tables
            .get_mut(table)
            .expect("every resolved table was seeded")
            .unique_guards
            .insert(guard);
    }

    // `contype = 'f'` is filtered explicitly. `confdeltype` is a single stable
    // character, so the on-delete action carries no rendered expression text.
    let foreign_key_rows = sqlx::query(
        "SELECT constraint_catalog.conrelid::bigint AS table_oid,
                ARRAY(
                    SELECT attribute.attname::text
                    FROM unnest(constraint_catalog.conkey) WITH ORDINALITY AS key(attnum, ordinality)
                    JOIN pg_catalog.pg_attribute AS attribute
                        ON attribute.attrelid = constraint_catalog.conrelid
                       AND attribute.attnum = key.attnum
                    ORDER BY key.ordinality
                ) AS columns,
                CASE
                    WHEN parent.relnamespace = child.relnamespace THEN parent.relname::text
                    ELSE pg_catalog.quote_ident(parent_namespace.nspname) || '.'
                         || parent.relname::text
                END AS parent_table,
                ARRAY(
                    SELECT attribute.attname::text
                    FROM unnest(constraint_catalog.confkey)
                        WITH ORDINALITY AS key(attnum, ordinality)
                    JOIN pg_catalog.pg_attribute AS attribute
                        ON attribute.attrelid = constraint_catalog.confrelid
                       AND attribute.attnum = key.attnum
                    ORDER BY key.ordinality
                ) AS parent_columns,
                constraint_catalog.confdeltype::text AS on_delete
         FROM pg_catalog.pg_constraint AS constraint_catalog
         JOIN pg_catalog.pg_class AS child ON child.oid = constraint_catalog.conrelid
         JOIN pg_catalog.pg_class AS parent ON parent.oid = constraint_catalog.confrelid
         JOIN pg_catalog.pg_namespace AS parent_namespace
             ON parent_namespace.oid = parent.relnamespace
         WHERE constraint_catalog.contype = 'f'
           AND constraint_catalog.conrelid::bigint = ANY($1::bigint[])",
    )
    .bind(&oids)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    for row in foreign_key_rows {
        let Some(table) = table_of(row.get("table_oid")) else {
            continue;
        };
        let key = ForeignKeyShape {
            columns: row.get("columns"),
            parent_table: row.get("parent_table"),
            parent_columns: row.get("parent_columns"),
            on_delete: ForeignKeyAction::from_catalog(&row.get::<String, _>("on_delete")),
        };
        shape
            .tables
            .get_mut(table)
            .expect("every resolved table was seeded")
            .foreign_keys
            .insert(key);
    }
    Ok(shape)
}

/// Canonicalizes a partial-index predicate as rendered by `pg_get_expr`.
///
/// Outer parentheses, internal whitespace runs, and letter case are the three
/// ways the same predicate can render differently; nothing else about the
/// predicate is interpreted.
pub(crate) fn normalize_predicate(predicate: &str) -> String {
    let mut text = predicate.trim();
    while let Some(inner) = text
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        // Only strip a genuinely enclosing pair, not `(a) AND (b)`.
        let mut depth = 0usize;
        let mut encloses = true;
        for character in inner.chars() {
            match character {
                '(' => depth += 1,
                ')' => match depth.checked_sub(1) {
                    Some(next) => depth = next,
                    None => {
                        encloses = false;
                        break;
                    }
                },
                _ => {}
            }
        }
        if !encloses || depth != 0 {
            break;
        }
        text = inner.trim();
    }
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Checks the seed rows `schema.sql` inserts.
///
/// These are invisible to any structural comparison and lash cannot run without
/// them: a missing `lash_process_change_clock` row breaks every process-registry
/// write, and the await-event signing secret authenticates every durable promise.
///
/// Each row is looked up by its `singleton = TRUE` key rather than by table
/// non-emptiness. `CHECK (singleton)` is deliberately outside the verified scope,
/// so nothing else would stop a host port from holding a `singleton = FALSE` row
/// that satisfies "the table has rows" and fails every runtime read.
async fn read_seed_row_findings(
    connection: &mut PgConnection,
    installation: &Installation,
    found: &SchemaShape,
) -> Result<Vec<SchemaFinding>, StoreError> {
    let mut findings = Vec::new();
    for (table, detail) in SEED_ROWS {
        // A mis-shaped table has already been reported column by column; probing
        // it for a row it cannot hold would raise a raw Postgres error instead.
        if !found
            .tables
            .get(table)
            .is_some_and(|shape| shape.columns.contains_key("singleton"))
        {
            continue;
        }
        let present: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT 1::BIGINT FROM {}.{table} WHERE singleton = TRUE",
            installation.quoted_namespace
        ))
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        if present.is_none() {
            findings.push(SchemaFinding::MissingSeedRow {
                table: table.to_string(),
                detail: detail.to_string(),
            });
        }
    }

    // The secret's width is a precondition open enforces, so the report has to
    // carry it too: a host gating its migration CI on `verify_schema` would
    // otherwise pass a database whose secret it seeded at the wrong width and
    // fail at the production open instead.
    if found
        .tables
        .get("lash_await_event_meta")
        .is_some_and(|shape| shape.columns.contains_key("signing_secret"))
    {
        let secret: Option<Vec<u8>> = sqlx::query_scalar(&format!(
            "SELECT signing_secret FROM {}.lash_await_event_meta WHERE singleton = TRUE",
            installation.quoted_namespace
        ))
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(secret) = secret
            && secret.len() != AWAIT_EVENT_SIGNING_SECRET_BYTES
        {
            findings.push(SchemaFinding::InvalidSeedRow {
                table: "lash_await_event_meta".to_string(),
                detail: format!(
                    "await-event signing secret has {} bytes, expected {AWAIT_EVENT_SIGNING_SECRET_BYTES}",
                    secret.len()
                ),
            });
        }
    }
    Ok(findings)
}
