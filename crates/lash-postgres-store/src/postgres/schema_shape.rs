//! Structural verification of the PostgreSQL objects `lash-postgres-store` owns.
//!
//! A host may provision the database itself (see `schema.sql`), in which case the
//! component version stamp in `lash_schema_versions` is written by the host and
//! proves nothing about the shape of the tables it stamps. The check in this
//! module reads the live catalog and diffs it against a generated expectation
//! artifact, so a mis-ported vendored schema is rejected at open with a
//! per-object diff instead of failing at the first query — or, worse, silently
//! dropping a guard the durability semantics depend on.
//!
//! # Scope
//!
//! Per lash-owned table, keyed by name and insensitive to declaration order:
//!
//! - the table's presence;
//! - every column as (name, type, nullability, has-auto-generated-value);
//! - primary keys, unique constraints, and bare unique indexes — all read from
//!   `pg_index.indisunique`, including normalized partial predicates, because
//!   the exactly-once dedup guard `idx_lash_process_events_key` is a partial
//!   unique *index* with no `pg_constraint` row;
//! - foreign keys with their on-delete action.
//!
//! Deliberately out of scope: `CHECK` constraints, non-unique indexes,
//! constraint and index names, column ordinal positions, default expression
//! text, and `NULLS NOT DISTINCT` (absent from `pg_index` before PostgreSQL 15).
//! Every attribute that remains renders identically on PostgreSQL 14 through
//! 18, which the version matrix in CI asserts.
//!
//! Host additions outside lash's tables are invisible to the check: only the
//! tables named by the artifact are introspected. Additions *on* a lash table —
//! an extra column, an extra unique guard, an extra foreign key — are reported,
//! because lash owns those tables by contract and each of them can break its
//! writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sqlx::{PgConnection, Row};

use crate::{SCHEMA_COMPONENT, SCHEMA_VERSION, StoreError, store_sqlx_error};

/// Generated description of the schema this build expects, regenerated from
/// [`crate::PostgresStorage::schema_ddl`] by the `schema_shape` test suite.
const SHAPE_ARTIFACT: &str = include_str!("../../schema-shape.txt");

/// Tables whose seed rows are a data precondition no structural check can see.
const SEED_ROW_TABLES: [&str; 3] = [
    "lash_schema_versions",
    "lash_process_change_clock",
    "lash_await_event_meta",
];

/// What a [`crate::PostgresStorage`] does when the live schema does not match
/// the shape this build expects.
///
/// The default is [`SchemaCheck::Enforce`]: a mismatch is a hard error at open,
/// because the guards it protects (the exactly-once dedup unique index, the
/// cascade foreign keys) fail silently rather than loudly when they are absent.
/// [`SchemaCheck::WarnOnly`] exists for the one failure this crate cannot rule
/// out by testing — a bug in the expectation artifact itself against a
/// PostgreSQL build lash has not seen. It is deliberately an API-level choice a
/// host commits to in reviewed code, never an environment variable an operator
/// can flip at 3am.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SchemaCheck {
    /// Reject the open with a per-object diff when the live schema drifts.
    #[default]
    Enforce,
    /// Log the same per-object diff at `WARN` and open anyway.
    WarnOnly,
}

/// Who owns the DDL for the database a [`crate::PostgresStorage`] opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SchemaProvisioning {
    /// lash applies its own idempotent creation DDL at open, then verifies the
    /// result. Requires `CREATE` on the target schema. This is the default and
    /// the historical behaviour.
    #[default]
    LashManaged,
    /// The host provisioned the schema — from `schema.sql`, vendored into its own
    /// migration tooling — and lash runs no DDL at all. Open reads the component
    /// version stamp, verifies the structure, and verifies the seed rows, so it
    /// needs no privilege beyond `SELECT` on lash's tables (plus the writes the
    /// runtime itself performs).
    HostProvisioned,
}

/// Referential action a foreign key takes when its parent row is deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ForeignKeyAction {
    /// `NO ACTION` — PostgreSQL's default.
    NoAction,
    /// `RESTRICT`.
    Restrict,
    /// `CASCADE`.
    Cascade,
    /// `SET NULL`.
    SetNull,
    /// `SET DEFAULT`.
    SetDefault,
}

impl ForeignKeyAction {
    /// Parses the single-character `pg_constraint.confdeltype` encoding, mapping
    /// anything unrecognized to [`ForeignKeyAction::NoAction`] so a future
    /// PostgreSQL encoding cannot panic an open.
    fn from_catalog(code: &str) -> Self {
        match code {
            "r" => Self::Restrict,
            "c" => Self::Cascade,
            "n" => Self::SetNull,
            "d" => Self::SetDefault,
            _ => Self::NoAction,
        }
    }

    /// Renders the action as it appears in DDL and in the shape artifact.
    fn as_sql(self) -> &'static str {
        match self {
            Self::NoAction => "no action",
            Self::Restrict => "restrict",
            Self::Cascade => "cascade",
            Self::SetNull => "set null",
            Self::SetDefault => "set default",
        }
    }

    /// Parses the rendering produced by [`ForeignKeyAction::as_sql`].
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "no action" => Self::NoAction,
            "restrict" => Self::Restrict,
            "cascade" => Self::Cascade,
            "set null" => Self::SetNull,
            "set default" => Self::SetDefault,
            _ => return None,
        })
    }
}

impl fmt::Display for ForeignKeyAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_sql())
    }
}

/// One column of a lash-owned table, restricted to the attributes that render
/// identically on every supported PostgreSQL major.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnShape {
    /// Column name. Columns are matched by name, never by ordinal position.
    pub name: String,
    /// `format_type` rendering of the column's type, e.g. `text` or `bigint`.
    pub sql_type: String,
    /// Whether the column accepts `NULL`.
    pub nullable: bool,
    /// Whether the column supplies its own value when an insert omits it —
    /// either an identity column or any column default. `BIGSERIAL` and
    /// `GENERATED ... AS IDENTITY` are deliberately equivalent here, and no
    /// default *expression* is ever compared.
    pub auto_generated: bool,
}

impl fmt::Display for ColumnShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}",
            self.sql_type,
            if self.nullable {
                "nullable"
            } else {
                "not-null"
            }
        )?;
        if self.auto_generated {
            formatter.write_str(" auto-generated")?;
        }
        Ok(())
    }
}

/// A uniqueness guarantee on a lash-owned table: a primary key, a unique
/// constraint, or a bare (possibly partial) unique index. All three are read
/// from `pg_index`, so a constraint-backed guard and a hand-written index over
/// the same columns compare equal, and the guard is identified by its column
/// list rather than by its auto-generated name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UniqueGuard {
    /// Whether this guard is the table's primary key.
    pub primary_key: bool,
    /// Key columns in index order.
    pub columns: Vec<String>,
    /// Normalized partial-index predicate, if the guard is partial. This is the
    /// only free-text element in the whole comparison; it is lower-cased with
    /// collapsed whitespace and outer parentheses stripped.
    pub predicate: Option<String>,
}

impl fmt::Display for UniqueGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({})",
            if self.primary_key {
                "primary key"
            } else {
                "unique"
            },
            self.columns.join(", ")
        )?;
        if let Some(predicate) = &self.predicate {
            write!(formatter, " where {predicate}")?;
        }
        Ok(())
    }
}

impl UniqueGuard {
    /// Identity used to pair a missing guard with a found one so a predicate
    /// difference reports as a mismatch rather than as an unrelated pair.
    fn pairing_key(&self) -> (bool, &[String]) {
        (self.primary_key, self.columns.as_slice())
    }
}

/// A foreign key declared on a lash-owned table, with the on-delete action lash
/// pruning depends on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForeignKeyShape {
    /// Referencing columns on the lash-owned table, in constraint order.
    pub columns: Vec<String>,
    /// Referenced table. Unqualified when it lives in the same schema as the
    /// referencing table, schema-qualified otherwise.
    pub parent_table: String,
    /// Referenced columns, in constraint order.
    pub parent_columns: Vec<String>,
    /// Action taken when a referenced row is deleted.
    pub on_delete: ForeignKeyAction,
}

impl fmt::Display for ForeignKeyShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "({}) references {} ({}) on delete {}",
            self.columns.join(", "),
            self.parent_table,
            self.parent_columns.join(", "),
            self.on_delete
        )
    }
}

impl ForeignKeyShape {
    /// Identity used to pair a missing key with a found one so an on-delete
    /// difference reports as a mismatch rather than as an unrelated pair.
    fn pairing_key(&self) -> (&[String], &str, &[String]) {
        (
            self.columns.as_slice(),
            self.parent_table.as_str(),
            self.parent_columns.as_slice(),
        )
    }
}

/// The shape of one lash-owned table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableShape {
    /// Columns keyed by name, so declaration order never enters the comparison.
    pub columns: BTreeMap<String, ColumnShape>,
    /// Every uniqueness guarantee on the table.
    pub unique_guards: BTreeSet<UniqueGuard>,
    /// Every foreign key declared on the table.
    pub foreign_keys: BTreeSet<ForeignKeyShape>,
}

/// The shape of every table lash owns, as either expected by this build or read
/// from a live database.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaShape {
    /// Tables keyed by unqualified name.
    pub tables: BTreeMap<String, TableShape>,
}

impl SchemaShape {
    /// The shape this build expects, parsed from the generated artifact.
    ///
    /// # Panics
    ///
    /// Panics if the compiled-in artifact is malformed or stamped with a
    /// component version other than the one this build implements. Both are
    /// build-time defects in this crate, not host conditions.
    pub fn expected() -> Self {
        let (version, shape) =
            Self::parse(SHAPE_ARTIFACT).expect("the compiled-in schema shape artifact must parse");
        assert_eq!(
            version, SCHEMA_VERSION,
            "the schema shape artifact is stamped for component version {version} but this build \
             implements {SCHEMA_VERSION}; regenerate crates/lash-postgres-store/schema-shape.txt"
        );
        shape
    }

    /// Parses the generated artifact format, returning its component version and
    /// the shape it describes.
    fn parse(text: &str) -> Result<(i32, Self), String> {
        let mut version: Option<i32> = None;
        let mut shape = Self::default();
        let mut current: Option<String> = None;
        for (index, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim_end();
            let number = index + 1;
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let bad = |what: &str| format!("line {number}: {what}: {trimmed}");
            if let Some(rest) = trimmed.strip_prefix("version ") {
                version = Some(rest.trim().parse().map_err(|_| bad("bad version"))?);
            } else if let Some(rest) = trimmed.strip_prefix("table ") {
                let name = rest.trim().to_string();
                if shape
                    .tables
                    .insert(name.clone(), TableShape::default())
                    .is_some()
                {
                    return Err(bad("duplicate table"));
                }
                current = Some(name);
            } else {
                let table_name = current.as_ref().ok_or_else(|| bad("no enclosing table"))?;
                let table = shape
                    .tables
                    .get_mut(table_name)
                    .expect("the current table was inserted before its members");
                if let Some(rest) = trimmed.strip_prefix("column ") {
                    let column = parse_column_line(rest).ok_or_else(|| bad("bad column"))?;
                    table.columns.insert(column.name.clone(), column);
                } else if let Some(rest) = trimmed.strip_prefix("primary-key ") {
                    let guard =
                        parse_guard_line(rest, true).ok_or_else(|| bad("bad primary key"))?;
                    table.unique_guards.insert(guard);
                } else if let Some(rest) = trimmed.strip_prefix("unique ") {
                    let guard = parse_guard_line(rest, false).ok_or_else(|| bad("bad unique"))?;
                    table.unique_guards.insert(guard);
                } else if let Some(rest) = trimmed.strip_prefix("foreign-key ") {
                    let key = parse_foreign_key_line(rest).ok_or_else(|| bad("bad foreign key"))?;
                    table.foreign_keys.insert(key);
                } else {
                    return Err(bad("unrecognized directive"));
                }
            }
        }
        let version = version.ok_or_else(|| "missing `version` line".to_string())?;
        if shape.tables.is_empty() {
            return Err("artifact declares no tables".to_string());
        }
        Ok((version, shape))
    }

    /// Renders the artifact format. Output is fully sorted, so regenerating an
    /// unchanged schema is a byte-identical no-op.
    pub fn render(&self, version: i32) -> String {
        let mut out = String::new();
        out.push_str(ARTIFACT_HEADER);
        out.push_str(&format!("version {version}\n"));
        for (name, table) in &self.tables {
            out.push_str(&format!("table {name}\n"));
            for column in table.columns.values() {
                out.push_str(&format!(
                    "  column {} {} {}",
                    column.name,
                    column.sql_type,
                    if column.nullable {
                        "nullable"
                    } else {
                        "not-null"
                    }
                ));
                if column.auto_generated {
                    out.push_str(" auto-generated");
                }
                out.push('\n');
            }
            for guard in &table.unique_guards {
                out.push_str(&format!(
                    "  {} ({})",
                    if guard.primary_key {
                        "primary-key"
                    } else {
                        "unique"
                    },
                    guard.columns.join(", ")
                ));
                if let Some(predicate) = &guard.predicate {
                    out.push_str(&format!(" where {predicate}"));
                }
                out.push('\n');
            }
            for key in &table.foreign_keys {
                out.push_str(&format!("  foreign-key {key}\n"));
            }
        }
        out
    }

    /// Diffs a live shape against this expected shape.
    fn diff(&self, found: &Self) -> Vec<SchemaFinding> {
        let mut findings = Vec::new();
        for (name, expected_table) in &self.tables {
            let Some(found_table) = found.tables.get(name) else {
                findings.push(SchemaFinding::MissingTable {
                    table: name.clone(),
                });
                continue;
            };
            diff_columns(name, expected_table, found_table, &mut findings);
            diff_unique_guards(name, expected_table, found_table, &mut findings);
            diff_foreign_keys(name, expected_table, found_table, &mut findings);
        }
        findings
    }
}

const ARTIFACT_HEADER: &str = "\
# lash-postgres-store expected schema shape.
#
# Generated artifact -- never edit by hand. Regenerate after any change to
# schema.sql by running the crate's `schema_shape` suite against a live
# PostgreSQL with LASH_UPDATE_SCHEMA_SHAPE=1, which rewrites this file from the
# catalog the DDL artifact actually produces. Every attribute recorded here
# renders identically on PostgreSQL 14 through 18; CI asserts that on all three.
#
# Columns are matched by name (never ordinal position); uniqueness guards and
# foreign keys are matched by column set and kind (never by constraint name).
";

fn parse_column_line(rest: &str) -> Option<ColumnShape> {
    let mut tokens: Vec<&str> = rest.split_whitespace().collect();
    let auto_generated = tokens.last() == Some(&"auto-generated");
    if auto_generated {
        tokens.pop();
    }
    let nullable = match tokens.pop()? {
        "nullable" => true,
        "not-null" => false,
        _ => return None,
    };
    let name = tokens.first()?.to_string();
    let sql_type = tokens.get(1..)?.join(" ");
    if name.is_empty() || sql_type.is_empty() {
        return None;
    }
    Some(ColumnShape {
        name,
        sql_type,
        nullable,
        auto_generated,
    })
}

fn parse_guard_line(rest: &str, primary_key: bool) -> Option<UniqueGuard> {
    let (columns, tail) = parse_column_list(rest)?;
    let tail = tail.trim();
    let predicate = if tail.is_empty() {
        None
    } else {
        Some(tail.strip_prefix("where ")?.trim().to_string())
    };
    Some(UniqueGuard {
        primary_key,
        columns,
        predicate,
    })
}

fn parse_foreign_key_line(rest: &str) -> Option<ForeignKeyShape> {
    let (columns, tail) = parse_column_list(rest)?;
    let tail = tail.trim().strip_prefix("references ")?;
    let open = tail.find('(')?;
    let parent_table = tail[..open].trim().to_string();
    let (parent_columns, tail) = parse_column_list(&tail[open..])?;
    let on_delete = ForeignKeyAction::parse(tail.trim().strip_prefix("on delete ")?.trim())?;
    if parent_table.is_empty() {
        return None;
    }
    Some(ForeignKeyShape {
        columns,
        parent_table,
        parent_columns,
        on_delete,
    })
}

/// Splits a leading `(a, b)` column list off `text`, returning the columns and
/// the remaining text.
fn parse_column_list(text: &str) -> Option<(Vec<String>, &str)> {
    let text = text.trim_start();
    let inner = text.strip_prefix('(')?;
    let close = inner.find(')')?;
    let columns: Vec<String> = inner[..close]
        .split(',')
        .map(|column| column.trim().to_string())
        .filter(|column| !column.is_empty())
        .collect();
    if columns.is_empty() {
        return None;
    }
    Some((columns, &inner[close + 1..]))
}

fn diff_columns(
    table: &str,
    expected: &TableShape,
    found: &TableShape,
    findings: &mut Vec<SchemaFinding>,
) {
    for (name, expected_column) in &expected.columns {
        match found.columns.get(name) {
            None => findings.push(SchemaFinding::MissingColumn {
                table: table.to_string(),
                expected: expected_column.clone(),
            }),
            Some(found_column) if found_column != expected_column => {
                findings.push(SchemaFinding::ColumnMismatch {
                    table: table.to_string(),
                    expected: expected_column.clone(),
                    found: found_column.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (name, found_column) in &found.columns {
        if !expected.columns.contains_key(name) {
            findings.push(SchemaFinding::UnexpectedColumn {
                table: table.to_string(),
                found: found_column.clone(),
            });
        }
    }
}

fn diff_unique_guards(
    table: &str,
    expected: &TableShape,
    found: &TableShape,
    findings: &mut Vec<SchemaFinding>,
) {
    let mut missing: Vec<&UniqueGuard> = expected
        .unique_guards
        .difference(&found.unique_guards)
        .collect();
    let mut unexpected: Vec<&UniqueGuard> = found
        .unique_guards
        .difference(&expected.unique_guards)
        .collect();
    missing.retain(|expected_guard| {
        let paired = unexpected
            .iter()
            .position(|found_guard| found_guard.pairing_key() == expected_guard.pairing_key());
        match paired {
            Some(index) => {
                let found_guard = unexpected.remove(index);
                findings.push(SchemaFinding::UniqueGuardMismatch {
                    table: table.to_string(),
                    expected: (*expected_guard).clone(),
                    found: found_guard.clone(),
                });
                false
            }
            None => true,
        }
    });
    for guard in missing {
        findings.push(SchemaFinding::MissingUniqueGuard {
            table: table.to_string(),
            expected: guard.clone(),
        });
    }
    for guard in unexpected {
        findings.push(SchemaFinding::UnexpectedUniqueGuard {
            table: table.to_string(),
            found: guard.clone(),
        });
    }
}

fn diff_foreign_keys(
    table: &str,
    expected: &TableShape,
    found: &TableShape,
    findings: &mut Vec<SchemaFinding>,
) {
    let mut missing: Vec<&ForeignKeyShape> = expected
        .foreign_keys
        .difference(&found.foreign_keys)
        .collect();
    let mut unexpected: Vec<&ForeignKeyShape> = found
        .foreign_keys
        .difference(&expected.foreign_keys)
        .collect();
    missing.retain(|expected_key| {
        let paired = unexpected
            .iter()
            .position(|found_key| found_key.pairing_key() == expected_key.pairing_key());
        match paired {
            Some(index) => {
                let found_key = unexpected.remove(index);
                findings.push(SchemaFinding::ForeignKeyMismatch {
                    table: table.to_string(),
                    expected: (*expected_key).clone(),
                    found: found_key.clone(),
                });
                false
            }
            None => true,
        }
    });
    for key in missing {
        findings.push(SchemaFinding::MissingForeignKey {
            table: table.to_string(),
            expected: key.clone(),
        });
    }
    for key in unexpected {
        findings.push(SchemaFinding::UnexpectedForeignKey {
            table: table.to_string(),
            found: key.clone(),
        });
    }
}

/// One drifted object, named. A [`SchemaReport`] carries a list of these rather
/// than a hash comparison, so a host that mis-ported one column learns which
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaFinding {
    /// The component version stamp is absent or names another version.
    VersionMismatch {
        /// Version this build implements.
        expected: i32,
        /// Version the database is stamped with, `None` when unstamped.
        found: Option<i32>,
    },
    /// A lash-owned table does not exist.
    MissingTable {
        /// Unqualified table name.
        table: String,
    },
    /// A lash-owned column does not exist.
    MissingColumn {
        /// Table that should carry the column.
        table: String,
        /// Column this build expects.
        expected: ColumnShape,
    },
    /// A lash-owned column exists with a different type, nullability, or
    /// auto-generated-value flag.
    ColumnMismatch {
        /// Table carrying the column.
        table: String,
        /// Column this build expects.
        expected: ColumnShape,
        /// Column the database has.
        found: ColumnShape,
    },
    /// A lash-owned table carries a column lash does not know about.
    UnexpectedColumn {
        /// Table carrying the column.
        table: String,
        /// Column the database has.
        found: ColumnShape,
    },
    /// A uniqueness guarantee lash depends on is absent.
    MissingUniqueGuard {
        /// Table that should carry the guard.
        table: String,
        /// Guard this build expects.
        expected: UniqueGuard,
    },
    /// A uniqueness guarantee exists over the expected columns but with a
    /// different partial predicate, so it guards a different row set.
    UniqueGuardMismatch {
        /// Table carrying the guard.
        table: String,
        /// Guard this build expects.
        expected: UniqueGuard,
        /// Guard the database has.
        found: UniqueGuard,
    },
    /// A lash-owned table carries a uniqueness guarantee lash does not know
    /// about, which can reject writes lash considers valid.
    UnexpectedUniqueGuard {
        /// Table carrying the guard.
        table: String,
        /// Guard the database has.
        found: UniqueGuard,
    },
    /// A foreign key lash depends on is absent, so deletes leave orphan rows.
    MissingForeignKey {
        /// Table that should carry the key.
        table: String,
        /// Key this build expects.
        expected: ForeignKeyShape,
    },
    /// A foreign key exists over the expected columns with a different
    /// on-delete action.
    ForeignKeyMismatch {
        /// Table carrying the key.
        table: String,
        /// Key this build expects.
        expected: ForeignKeyShape,
        /// Key the database has.
        found: ForeignKeyShape,
    },
    /// A lash-owned table carries a foreign key lash does not know about.
    UnexpectedForeignKey {
        /// Table carrying the key.
        table: String,
        /// Key the database has.
        found: ForeignKeyShape,
    },
    /// A seed row `schema.sql` inserts is absent. No structural check can see
    /// this, and lash cannot run without it.
    MissingSeedRow {
        /// Table that should carry the row.
        table: String,
        /// What the row is for.
        detail: String,
    },
}

impl SchemaFinding {
    fn section(&self) -> &'static str {
        match self {
            Self::VersionMismatch { .. } => "COMPONENT VERSION",
            Self::MissingTable { .. } => "MISSING TABLES",
            Self::MissingColumn { .. }
            | Self::ColumnMismatch { .. }
            | Self::UnexpectedColumn { .. } => "COLUMN DRIFT",
            Self::MissingUniqueGuard { .. }
            | Self::UniqueGuardMismatch { .. }
            | Self::UnexpectedUniqueGuard { .. } => "UNIQUE GUARD DRIFT",
            Self::MissingForeignKey { .. }
            | Self::ForeignKeyMismatch { .. }
            | Self::UnexpectedForeignKey { .. } => "FOREIGN KEY DRIFT",
            Self::MissingSeedRow { .. } => "SEED ROWS",
        }
    }
}

impl fmt::Display for SchemaFinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionMismatch { expected, found } => match found {
                Some(found) => write!(
                    formatter,
                    "component `{SCHEMA_COMPONENT}` is stamped version {found}, expected {expected}"
                ),
                None => write!(
                    formatter,
                    "component `{SCHEMA_COMPONENT}` has no version stamp, expected {expected}"
                ),
            },
            Self::MissingTable { table } => write!(formatter, "{table}: table is missing"),
            Self::MissingColumn { table, expected } => write!(
                formatter,
                "{table}.{}: column is missing, expected {expected}",
                expected.name
            ),
            Self::ColumnMismatch {
                table,
                expected,
                found,
            } => write!(
                formatter,
                "{table}.{}: expected {expected}, found {found}",
                expected.name
            ),
            Self::UnexpectedColumn { table, found } => write!(
                formatter,
                "{table}.{}: unexpected column, found {found}",
                found.name
            ),
            Self::MissingUniqueGuard { table, expected } => {
                write!(formatter, "{table}: missing {expected}")
            }
            Self::UniqueGuardMismatch {
                table,
                expected,
                found,
            } => write!(formatter, "{table}: expected {expected}, found {found}"),
            Self::UnexpectedUniqueGuard { table, found } => {
                write!(formatter, "{table}: unexpected {found}")
            }
            Self::MissingForeignKey { table, expected } => {
                write!(formatter, "{table}: missing foreign key {expected}")
            }
            Self::ForeignKeyMismatch {
                table,
                expected,
                found,
            } => write!(
                formatter,
                "{table}: expected foreign key {expected}, found {found}"
            ),
            Self::UnexpectedForeignKey { table, found } => {
                write!(formatter, "{table}: unexpected foreign key {found}")
            }
            Self::MissingSeedRow { table, detail } => {
                write!(formatter, "{table}: seed row is missing ({detail})")
            }
        }
    }
}

/// Result of comparing a live database against the schema this build expects.
///
/// Obtained from [`crate::PostgresStorage::verify_schema`], which a host can call
/// from its own migration CI to gate a vendored schema before it ever reaches a
/// production open.
#[derive(Clone, Debug)]
pub struct SchemaReport {
    /// PostgreSQL schema the lash tables resolved in, `None` when none of them
    /// exist. Resolution follows the connection's `search_path`; `public` is
    /// never assumed.
    pub schema: Option<String>,
    /// Component version this build implements.
    pub expected_version: i32,
    /// Component version the database is stamped with, if any.
    pub found_version: Option<i32>,
    /// Every drifted object, in section order.
    pub findings: Vec<SchemaFinding>,
}

impl SchemaReport {
    /// Whether the database matches this build's expected schema.
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty()
    }
}

impl fmt::Display for SchemaReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let schema = self.schema.as_deref().unwrap_or("<unresolved>");
        if self.is_conformant() {
            return write!(
                formatter,
                "lash Postgres schema in `{schema}` matches component `{SCHEMA_COMPONENT}` \
                 version {}",
                self.expected_version
            );
        }
        write!(
            formatter,
            "lash Postgres schema in `{schema}` does not match component `{SCHEMA_COMPONENT}` \
             version {}",
            self.expected_version
        )?;
        let sections = [
            "COMPONENT VERSION",
            "MISSING TABLES",
            "COLUMN DRIFT",
            "UNIQUE GUARD DRIFT",
            "FOREIGN KEY DRIFT",
            "SEED ROWS",
        ];
        for section in sections {
            let mut lines = self
                .findings
                .iter()
                .filter(|finding| finding.section() == section)
                .peekable();
            if lines.peek().is_none() {
                continue;
            }
            write!(formatter, "\n\n{section}")?;
            for finding in lines {
                write!(formatter, "\n  {finding}")?;
            }
        }
        write!(
            formatter,
            "\n\nProvision this database from the DDL artifact this build ships \
             (`PostgresStorage::schema_ddl()`, committed as \
             crates/lash-postgres-store/schema.sql) rather than transcribing it. To open against \
             a schema this build rejects, set \
             `PostgresStoreConfig::schema_check = SchemaCheck::WarnOnly`."
        )
    }
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
    let resolved = resolve_tables(connection, &table_names).await?;
    let schema = resolved.values().next().map(|table| table.schema.clone());
    let found_version = read_component_version(connection, &resolved).await?;
    let mut report = SchemaReport {
        schema,
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
    let found = read_live_shape(connection, &resolved).await?;
    report.findings = expected.diff(&found);
    report
        .findings
        .extend(read_seed_row_findings(connection, &resolved).await?);
    Ok(report)
}

/// A lash table resolved through the connection's `search_path`.
struct ResolvedTable {
    oid: i64,
    schema: String,
}

async fn resolve_tables(
    connection: &mut PgConnection,
    table_names: &[String],
) -> Result<BTreeMap<String, ResolvedTable>, StoreError> {
    let rows = sqlx::query(
        "SELECT expected.name AS name,
                relation.oid::bigint AS oid,
                namespace.nspname::text AS schema_name
         FROM unnest($1::text[]) AS expected(name)
         JOIN pg_catalog.pg_class AS relation
             ON relation.oid = pg_catalog.to_regclass(pg_catalog.quote_ident(expected.name))::oid
            AND relation.relkind IN ('r', 'p')
         JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace",
    )
    .bind(table_names)
    .fetch_all(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    let mut resolved = BTreeMap::new();
    for row in rows {
        resolved.insert(
            row.get::<String, _>("name"),
            ResolvedTable {
                oid: row.get("oid"),
                schema: row.get("schema_name"),
            },
        );
    }
    Ok(resolved)
}

async fn read_component_version(
    connection: &mut PgConnection,
    resolved: &BTreeMap<String, ResolvedTable>,
) -> Result<Option<i32>, StoreError> {
    if !resolved.contains_key("lash_schema_versions") {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
        .bind(SCHEMA_COMPONENT)
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)
}

async fn read_live_shape(
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
                (attribute.atthasdef OR attribute.attidentity <> '') AS auto_generated
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
            auto_generated: row.get("auto_generated"),
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
                pg_catalog.pg_get_expr(index_catalog.indpred, index_catalog.indrelid) AS predicate
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
fn normalize_predicate(predicate: &str) -> String {
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

/// Checks the seed rows `schema.sql` inserts. These are invisible to any
/// structural comparison and lash cannot run without them: a missing
/// `lash_process_change_clock` row breaks every process-registry write.
async fn read_seed_row_findings(
    connection: &mut PgConnection,
    resolved: &BTreeMap<String, ResolvedTable>,
) -> Result<Vec<SchemaFinding>, StoreError> {
    let mut findings = Vec::new();
    for table in SEED_ROW_TABLES {
        if table == "lash_schema_versions" {
            // Already covered by the component version stamp.
            continue;
        }
        if !resolved.contains_key(table) {
            continue;
        }
        let present: Option<i64> =
            sqlx::query_scalar(&format!("SELECT 1::BIGINT FROM {table} LIMIT 1"))
                .fetch_optional(&mut *connection)
                .await
                .map_err(store_sqlx_error)?;
        if present.is_none() {
            findings.push(SchemaFinding::MissingSeedRow {
                table: table.to_string(),
                detail: match table {
                    "lash_process_change_clock" => "transactional process-change clock".to_string(),
                    _ => "await-event signing secret".to_string(),
                },
            });
        }
    }
    Ok(findings)
}

#[path = "schema_shape/tests.rs"]
#[cfg(test)]
mod tests;
