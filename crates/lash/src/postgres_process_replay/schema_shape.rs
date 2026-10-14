//! The structure the store's tables must have, and the check that reads it
//! from the catalog.
//!
//! `postgres-process-replay-schema-shape.txt` is generated from the catalog the
//! published DDL produces. Each table is a set of member lines (its
//! persistence, its columns by name with type and nullability, and its unique
//! guards by key column set and predicate), so declaration order, object
//! names, `CHECK` constraints and non-unique indexes never enter the
//! comparison. A table missing, or a member line missing or extra, is drift.
//! Tables the artifact does not name are outside the comparison.

use std::fmt;

use lash_core::ProcessReplayStoreError;
use sqlx::PgConnection;

use crate::postgres_replay::schema::{self, Finding, Shape};

use super::schema::db_error;

/// The published DDL, executed verbatim by the `install` schema mode.
pub(super) const SCHEMA_DDL: &str = include_str!("../../postgres-process-replay-schema.sql");

/// The structure the published DDL produces, generated from its catalog.
pub(super) const SHAPE_ARTIFACT: &str =
    include_str!("../../postgres-process-replay-schema-shape.txt");

const ARTIFACT_HEADER: &str = "\
# lash PostgreSQL process replay store expected schema shape.
#
# Generated artifact -- never edit by hand. Regenerate after any change to
# postgres-process-replay-schema.sql by running the process_replay suite's
# `regenerate_process_replay_schema_shape` against PostgreSQL 18 with
# LASH_REGENERATE=1, which rewrites this file from the catalog the DDL
# artifact produces.
#
# Each table is a set of member lines: its persistence, its columns by name,
# and its unique guards by key column set (sorted) and partial predicate.
# Member order, object names, CHECK constraints and non-unique indexes are
# never compared.
";

/// One way the process replay tables differ from the published artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostgresProcessReplaySchemaFinding {
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

impl fmt::Display for PostgresProcessReplaySchemaFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTable { table } => write!(f, "{table}: table missing"),
            Self::Missing { table, object } => write!(f, "{table}: missing `{object}`"),
            Self::Unexpected { table, object } => write!(f, "{table}: unexpected `{object}`"),
        }
    }
}

/// What a check of the process replay tables found: conformant when it holds
/// no findings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostgresProcessReplaySchemaReport {
    schema: String,
    findings: Vec<PostgresProcessReplaySchemaFinding>,
    found: Shape,
}

impl PostgresProcessReplaySchemaReport {
    /// The PostgreSQL schema the check read.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Every difference from the published artifact.
    pub fn findings(&self) -> &[PostgresProcessReplaySchemaFinding] {
        &self.findings
    }

    /// Whether the tables match the published artifact.
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty()
    }

    /// The structure the check read, every table of the schema included, in
    /// the shape artifact's format: diff it against
    /// [`PostgresProcessReplayStore::schema_shape`](super::PostgresProcessReplayStore::schema_shape).
    pub fn found_shape(&self) -> String {
        self.found.render(ARTIFACT_HEADER)
    }
}

impl fmt::Display for PostgresProcessReplaySchemaReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.findings.is_empty() {
            return write!(f, "schema `{}` conforms", self.schema);
        }
        write!(
            f,
            "schema `{}` differs from the published process replay artifact:",
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
#[expect(
    clippy::expect_used,
    reason = "the artifact is compiled in from this crate's own generated file"
)]
pub(super) async fn verify(
    connection: &mut PgConnection,
    schema: &str,
) -> Result<PostgresProcessReplaySchemaReport, ProcessReplayStoreError> {
    let found = schema::inspect(connection, schema)
        .await
        .map_err(db_error("verify schema"))?;
    let expected =
        Shape::parse(SHAPE_ARTIFACT).expect("the compiled-in process replay shape artifact parses");
    Ok(PostgresProcessReplaySchemaReport {
        schema: schema.to_string(),
        findings: expected.diff(&found).into_iter().map(Into::into).collect(),
        found,
    })
}

impl From<Finding> for PostgresProcessReplaySchemaFinding {
    fn from(finding: Finding) -> Self {
        match finding {
            Finding::MissingTable { table } => Self::MissingTable { table },
            Finding::Missing { table, object } => Self::Missing { table, object },
            Finding::Unexpected { table, object } => Self::Unexpected { table, object },
        }
    }
}
