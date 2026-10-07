//! The structure the store's tables must have, and the check that reads it
//! from the catalog.
//!
//! `postgres-live-replay-schema-shape.txt` is generated from the catalog the
//! published DDL produces. Each table is a set of member lines (its
//! persistence, its columns by name with type and nullability, and its unique
//! guards by key column set and predicate), so declaration order, object
//! names, `CHECK` constraints and non-unique indexes never enter the
//! comparison. A table missing, or a member line missing or extra, is drift.
//! Tables the artifact does not name are outside the comparison.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use lash_core::LiveReplayStoreError;
use sqlx::{PgConnection, Row as _};

use super::schema::db_error;

/// The published DDL, executed verbatim by the `install` schema mode.
pub(super) const SCHEMA_DDL: &str = include_str!("../../postgres-live-replay-schema.sql");

/// The structure the published DDL produces, generated from its catalog.
pub(super) const SHAPE_ARTIFACT: &str = include_str!("../../postgres-live-replay-schema-shape.txt");

const ARTIFACT_HEADER: &str = "\
# lash PostgreSQL live replay store expected schema shape.
#
# Generated artifact -- never edit by hand. Regenerate after any change to
# postgres-live-replay-schema.sql by running the live_replay suite's
# `regenerate_live_replay_schema_shape` against PostgreSQL 18 with
# LASH_REGENERATE=1, which rewrites this file from the catalog the DDL
# artifact produces.
#
# Each table is a set of member lines: its persistence, its columns by name,
# and its unique guards by key column set (sorted) and partial predicate.
# Member order, object names, CHECK constraints and non-unique indexes are
# never compared.
";

/// Tables keyed by name, each a set of member lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Shape(BTreeMap<String, BTreeSet<String>>);

impl Shape {
    /// The shape this build expects.
    ///
    /// # Panics
    ///
    /// Panics if the compiled-in artifact is malformed: a build-time defect
    /// in this crate, not a host condition.
    #[expect(
        clippy::expect_used,
        reason = "the artifact is compiled in from this crate's own generated file"
    )]
    pub(super) fn expected() -> Self {
        Self::parse(SHAPE_ARTIFACT).expect("the compiled-in live replay shape artifact parses")
    }

    fn parse(text: &str) -> Result<Self, String> {
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

    fn render(&self) -> String {
        let mut out = ARTIFACT_HEADER.to_string();
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

    fn diff(&self, found: &Self) -> Vec<PostgresLiveReplaySchemaFinding> {
        let mut findings = Vec::new();
        for (table, expected) in &self.0 {
            let Some(found) = found.0.get(table) else {
                findings.push(PostgresLiveReplaySchemaFinding::MissingTable {
                    table: table.clone(),
                });
                continue;
            };
            for object in expected.difference(found) {
                findings.push(PostgresLiveReplaySchemaFinding::Missing {
                    table: table.clone(),
                    object: object.clone(),
                });
            }
            for object in found.difference(expected) {
                findings.push(PostgresLiveReplaySchemaFinding::Unexpected {
                    table: table.clone(),
                    object: object.clone(),
                });
            }
        }
        findings
    }
}

/// One way the live replay tables differ from the published artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostgresLiveReplaySchemaFinding {
    /// A table the artifact declares is absent from the schema.
    MissingTable {
        /// The table's name.
        table: String,
    },
    /// A member the artifact declares (persistence, column or unique guard,
    /// as a line of the shape artifact) is absent or differs.
    Missing {
        /// The table's name.
        table: String,
        /// The expected member, as its shape artifact line.
        object: String,
    },
    /// A member the artifact does not declare is present: an extra column or
    /// unique guard, or a column or guard that differs from the declared one.
    Unexpected {
        /// The table's name.
        table: String,
        /// The found member, as a shape artifact line.
        object: String,
    },
}

impl fmt::Display for PostgresLiveReplaySchemaFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTable { table } => write!(f, "{table}: table missing"),
            Self::Missing { table, object } => write!(f, "{table}: missing `{object}`"),
            Self::Unexpected { table, object } => write!(f, "{table}: unexpected `{object}`"),
        }
    }
}

/// What a check of the live replay tables found: conformant when it holds
/// no findings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostgresLiveReplaySchemaReport {
    schema: String,
    findings: Vec<PostgresLiveReplaySchemaFinding>,
    found: Shape,
}

impl PostgresLiveReplaySchemaReport {
    /// The PostgreSQL schema the check read.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Every difference from the published artifact.
    pub fn findings(&self) -> &[PostgresLiveReplaySchemaFinding] {
        &self.findings
    }

    /// Whether the tables match the published artifact.
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty()
    }

    /// The structure the check read, every table of the schema included, in
    /// the shape artifact's format: diff it against
    /// [`PostgresLiveReplayStore::schema_shape`](super::PostgresLiveReplayStore::schema_shape).
    pub fn found_shape(&self) -> String {
        self.found.render()
    }
}

impl fmt::Display for PostgresLiveReplaySchemaReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.findings.is_empty() {
            return write!(f, "schema `{}` conforms", self.schema);
        }
        write!(
            f,
            "schema `{}` differs from the published live replay artifact:",
            self.schema
        )?;
        for finding in &self.findings {
            write!(f, "\n  {finding}")?;
        }
        Ok(())
    }
}

/// Read every table of `schema` from the catalog and compare it with the
/// artifact. The caller holds the store's advisory lock.
pub(super) async fn verify(
    connection: &mut PgConnection,
    schema: &str,
) -> Result<PostgresLiveReplaySchemaReport, LiveReplayStoreError> {
    let mut found = Shape::default();
    let tables = sqlx::query(
        "SELECT c.relname::text AS table_name, c.relpersistence::text AS persistence \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(db_error("verify schema"))?;
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
    .await
    .map_err(db_error("verify schema"))?;
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
    .await
    .map_err(db_error("verify schema"))?;
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
    Ok(PostgresLiveReplaySchemaReport {
        schema: schema.to_string(),
        findings: Shape::expected().diff(&found),
        found,
    })
}
