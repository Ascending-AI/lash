//! Structural inspection shared by the two PostgreSQL replay feeds.

use sqlx::{PgConnection, Row as _};
use std::collections::{BTreeMap, BTreeSet};

/// Tables keyed by name, each a set of member lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape(BTreeMap<String, BTreeSet<String>>);

impl Shape {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut shape = Self::default();
        let mut current = None;
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(table) = trimmed.strip_prefix("table ") {
                if shape.0.insert(table.to_string(), BTreeSet::new()).is_some() {
                    return Err(format!("line {}: duplicate table {table}", index + 1));
                }
                current = Some(table.to_string());
            } else {
                let table = current
                    .as_ref()
                    .and_then(|table| shape.0.get_mut(table))
                    .ok_or_else(|| format!("line {}: no enclosing table", index + 1))?;
                table.insert(trimmed.to_string());
            }
        }
        if shape.0.is_empty() {
            return Err("the artifact declares no tables".to_string());
        }
        Ok(shape)
    }

    pub(crate) fn render(&self, header: &str) -> String {
        let mut out = header.to_string();
        for (table, members) in &self.0 {
            out.push_str(&format!("table {table}\n"));
            for member in members {
                out.push_str(&format!("  {member}\n"));
            }
        }
        out
    }

    fn member(&mut self, table: String, line: String) {
        self.0.entry(table).or_default().insert(line);
    }

    pub(crate) fn diff(&self, found: &Self) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (table, expected) in &self.0 {
            let Some(found) = found.0.get(table) else {
                findings.push(Finding::MissingTable {
                    table: table.clone(),
                });
                continue;
            };
            for object in expected.difference(found) {
                findings.push(Finding::Missing {
                    table: table.clone(),
                    object: object.clone(),
                });
            }
            for object in found.difference(expected) {
                findings.push(Finding::Unexpected {
                    table: table.clone(),
                    object: object.clone(),
                });
            }
        }
        findings
    }
}

pub(crate) enum Finding {
    MissingTable { table: String },
    Missing { table: String, object: String },
    Unexpected { table: String, object: String },
}

/// Read the catalog under the caller's existing schema advisory lock.
pub(crate) async fn inspect(
    connection: &mut PgConnection,
    schema: &str,
) -> Result<Shape, sqlx::Error> {
    let mut found = Shape::default();
    let tables = sqlx::query(
        "SELECT c.relname::text AS table_name, c.relpersistence::text AS persistence \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await?;
    for row in tables {
        let persistence = match row.get::<String, _>("persistence").as_str() {
            "u" => "unlogged",
            "t" => "temporary",
            _ => "logged",
        };
        found.member(row.get("table_name"), format!("persistence {persistence}"));
    }
    let columns = sqlx::query(
        "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
                format_type(a.atttypid, a.atttypmod) AS column_type, a.attnotnull AS not_null \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await?;
    for row in columns {
        let nullability = if row.get("not_null") {
            "not-null"
        } else {
            "nullable"
        };
        found.member(
            row.get("table_name"),
            format!(
                "column {} {} {nullability}",
                row.get::<String, _>("column_name"),
                row.get::<String, _>("column_type"),
            ),
        );
    }
    let guards = sqlx::query(
        "SELECT c.relname::text AS table_name, i.indisprimary AS primary_key, \
                ARRAY(SELECT a.attname::text \
                      FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                      WHERE k.ord <= i.indnkeyatts ORDER BY a.attname) AS key_columns, \
                pg_get_expr(i.indpred, i.indrelid) AS predicate, \
                i.indnullsnotdistinct AS nulls_not_distinct \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND i.indisunique",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await?;
    for row in guards {
        let mut line = format!(
            "{} ({})",
            if row.get("primary_key") {
                "primary-key"
            } else {
                "unique"
            },
            row.get::<Vec<String>, _>("key_columns").join(", ")
        );
        if row.get("nulls_not_distinct") {
            line.push_str(" nulls not distinct");
        }
        if let Some(predicate) = row.get::<Option<String>, _>("predicate") {
            line.push_str(&format!(" where {predicate}"));
        }
        found.member(row.get("table_name"), line);
    }
    Ok(found)
}
