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
//! # One installation, not an assembly
//!
//! Every object is looked up in a single namespace — the one where
//! `lash_schema_versions` resolves through the connection's `search_path` — and
//! every expected object must be present *there*. Resolving each table
//! independently would let two partial installations on one `search_path` verify
//! as conformant while runtime writes split across them, so a lash-named relation
//! that `search_path` resolves outside the anchored namespace is a reported
//! finding rather than something silently used.
//!
//! # Scope
//!
//! Per lash-owned table, keyed by name and insensitive to declaration order:
//!
//! - the table's presence in the anchored namespace;
//! - every column as (name, type, nullability, value source) — where the value
//!   source classifies whether the column supplies its own value and whether it
//!   accepts an explicit one, which is what lash's inserts actually depend on;
//! - primary keys, unique constraints, and bare unique indexes — all read from
//!   `pg_index.indisunique`, including normalized partial predicates and
//!   `NULLS NOT DISTINCT`, because the exactly-once dedup guard
//!   `idx_lash_process_events_key` is a partial unique *index* with no
//!   `pg_constraint` row — matched by column *set*, since key order changes which
//!   index prefixes can be scanned rather than which rows are rejected;
//! - foreign keys with their on-delete action.
//!
//! Deliberately out of scope: `CHECK` constraints, non-unique indexes, triggers,
//! row-level security, constraint and index names, column ordinal positions, and
//! default expression text. Every attribute that remains renders identically on
//! PostgreSQL 14 through 18, which the version matrix in CI asserts —
//! `indnullsnotdistinct` is read through `to_jsonb`, which yields `NULL` on 14
//! where the catalog column does not yet exist and is normalized to `false`.
//!
//! Host additions outside lash's tables are invisible to the check: only the
//! tables named by the artifact are introspected. Additions *on* a lash table —
//! an extra column, an extra unique guard, an extra foreign key — are reported,
//! because lash owns those tables by contract and each of them can reject writes
//! lash considers valid. Host-added triggers, `CHECK` constraints, and row-level
//! security are *not* read and are the host's own risk; see ADR 0052.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::{SCHEMA_COMPONENT, SCHEMA_VERSION};

/// Generated description of the schema this build expects, regenerated from
/// [`crate::PostgresStorage::schema_ddl`] by the `schema_shape` test suite.
const SHAPE_ARTIFACT: &str = include_str!("../../schema-shape.txt");

/// Seed rows `schema.sql` inserts, paired with what each one is for. They are a
/// data precondition no structural comparison can see. The component version
/// stamp is seeded the same way but reported as a version mismatch instead.
///
/// Each is a singleton row keyed `singleton = TRUE`, and the check queries that
/// key rather than table non-emptiness: `CHECK (singleton)` is deliberately
/// outside the verified scope, so a host port that omits it can hold a
/// `singleton = FALSE` row that satisfies "the table has rows" and then fails
/// every runtime read.
const SEED_ROWS: [(&str, &str); 2] = [
    (
        "lash_process_change_clock",
        "transactional process-change clock",
    ),
    ("lash_await_event_meta", "await-event signing secret"),
];

/// Required width of the store-resident await-event signing secret.
pub(crate) const AWAIT_EVENT_SIGNING_SECRET_BYTES: usize = 32;

/// The namespace-anchoring table. Its resolution through `search_path` decides
/// which installation every other object is read from.
const ANCHOR_TABLE: &str = "lash_schema_versions";

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
///
/// # What no `SchemaCheck` relaxes
///
/// This governs the catalog comparison and nothing else. Two preconditions sit
/// outside it and reject an open in every mode:
///
/// - **The component version stamp.** It is the reject-and-recreate boundary, and
///   a valve adopted for a structural false positive must not become a path that
///   silently runs one build against another schema generation.
/// - **The await-event signing secret row.** Without it there is no key to
///   authenticate durable promises with, so there is nothing for open to return:
///   no secret, no store.
///
/// The `lash_process_change_clock` seed row is deliberately *not* in that list.
/// It is reported as an ordinary finding, so [`SchemaCheck::WarnOnly`] opens
/// without it and every process-registry write then fails at runtime. The
/// asymmetry is not that one row matters more than the other — both are required
/// — it is that the secret is something open must physically read and hand back,
/// while the clock row is only something writes will later need. A valve that
/// could not be overridden for the clock row would be a valve that cannot be used
/// to work around a checker bug, which is its entire purpose.
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

/// Where a column's value comes from, classified by what it means for a write.
///
/// lash omits some columns from its inserts and names others explicitly, so the
/// two properties that matter are whether the column supplies its own value and
/// whether it accepts one. A single "has an auto-generated value" bit conflates
/// states that differ on the second property: `GENERATED ALWAYS AS IDENTITY` and
/// a stored generated column both supply a value and both *reject* an explicit
/// one, which breaks every insert that names the column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ColumnValueSource {
    /// No default and no generation: a write must supply the value.
    Supplied,
    /// An ordinary column default, including the `nextval` default `BIGSERIAL`
    /// installs. Omitting or naming the column both work. No default
    /// *expression* is ever compared.
    Default,
    /// `GENERATED BY DEFAULT AS IDENTITY`. Write-equivalent to
    /// [`ColumnValueSource::Default`] for lash: omitting or naming the column
    /// both work.
    IdentityByDefault,
    /// `GENERATED ALWAYS AS IDENTITY`. Supplies a value but rejects an explicit
    /// one without `OVERRIDING SYSTEM VALUE`.
    IdentityAlways,
    /// A generated column — stored (`attgenerated = 's'`) or, from PostgreSQL 18,
    /// virtual. Supplies a value and rejects any explicit one outright.
    Generated,
}

impl ColumnValueSource {
    /// Whether an insert may omit the column and still get a value.
    fn supplies_own_value(self) -> bool {
        !matches!(self, Self::Supplied)
    }

    /// Whether an insert may name the column and supply a value for it.
    fn accepts_explicit_value(self) -> bool {
        matches!(
            self,
            Self::Supplied | Self::Default | Self::IdentityByDefault
        )
    }

    /// Classifies from the catalog. `attgenerated` is tested before
    /// `attidentity` and `atthasdef` because a generated column carries a
    /// `pg_attrdef` row too, so the coarser signals would mask it.
    fn from_catalog(identity: &str, generated: &str, has_default: bool) -> Self {
        if !generated.is_empty() {
            return Self::Generated;
        }
        match identity {
            "a" => Self::IdentityAlways,
            "d" => Self::IdentityByDefault,
            _ if has_default => Self::Default,
            _ => Self::Supplied,
        }
    }

    /// Token used in the artifact and in diffs. [`ColumnValueSource::Supplied`]
    /// renders as nothing, since it is the common case.
    fn as_token(self) -> Option<&'static str> {
        Some(match self {
            Self::Supplied => return None,
            Self::Default => "default",
            Self::IdentityByDefault => "identity-by-default",
            Self::IdentityAlways => "identity-always",
            Self::Generated => "generated",
        })
    }

    /// Parses the token produced by [`ColumnValueSource::as_token`].
    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "default" => Self::Default,
            "identity-by-default" => Self::IdentityByDefault,
            "identity-always" => Self::IdentityAlways,
            "generated" => Self::Generated,
            _ => return None,
        })
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
    /// Where the column's value comes from.
    pub value_source: ColumnValueSource,
}

impl ColumnShape {
    /// Whether a column can stand in for one this build expects.
    ///
    /// Type and nullability must match exactly. The value source only has to be
    /// *write-compatible*: it must keep every capability the expected column has,
    /// so a host may modernize `BIGSERIAL` into `GENERATED BY DEFAULT AS IDENTITY`
    /// (same capabilities) but not into `GENERATED ALWAYS AS IDENTITY`, which
    /// rejects the explicit values lash supplies for `enqueue_seq`.
    fn satisfies(&self, expected: &Self) -> bool {
        self.name == expected.name
            && self.sql_type == expected.sql_type
            && self.nullable == expected.nullable
            && (!expected.value_source.supplies_own_value()
                || self.value_source.supplies_own_value())
            && (!expected.value_source.accepts_explicit_value()
                || self.value_source.accepts_explicit_value())
    }
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
        if let Some(token) = self.value_source.as_token() {
            write!(formatter, " {token}")?;
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
    /// Whether the guard treats `NULL`s as equal (`UNIQUE NULLS NOT DISTINCT`,
    /// PostgreSQL 15+).
    ///
    /// lash's own guards never set it, and two of them —
    /// `UNIQUE (session_id, source_key)` on `lash_queued_work_batches` and on
    /// `lash_pending_turn_inputs` — depend on the default: `source_key` is
    /// nullable and lash writes `NULL` for every batch or input without a source
    /// key, so under `NULLS NOT DISTINCT` only one such row per session would be
    /// permitted. Read as `false` on PostgreSQL 14, where the catalog column does
    /// not exist and the feature does not either.
    pub nulls_not_distinct: bool,
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
        if self.nulls_not_distinct {
            formatter.write_str(" nulls not distinct")?;
        }
        if let Some(predicate) = &self.predicate {
            write!(formatter, " where {predicate}")?;
        }
        Ok(())
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

/// The shape of one lash-owned table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TableShape {
    /// Columns keyed by name, so declaration order never enters the comparison.
    pub(crate) columns: BTreeMap<String, ColumnShape>,
    /// Every uniqueness guarantee on the table.
    pub(crate) unique_guards: BTreeSet<UniqueGuard>,
    /// Every foreign key declared on the table.
    pub(crate) foreign_keys: BTreeSet<ForeignKeyShape>,
}

/// The shape of every table lash owns, as either expected by this build or read
/// from a live database.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchemaShape {
    /// Tables keyed by unqualified name.
    pub(crate) tables: BTreeMap<String, TableShape>,
}

impl SchemaShape {
    /// The shape this build expects, parsed from the generated artifact.
    ///
    /// # Panics
    ///
    /// Panics if the compiled-in artifact is malformed or stamped with a
    /// component version other than the one this build implements. Both are
    /// build-time defects in this crate, not host conditions.
    pub(crate) fn expected() -> Self {
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
    ///
    /// Rendering exists to regenerate the committed artifact from a live database;
    /// nothing at runtime ever writes the format, only parses it.
    #[cfg(test)]
    pub(crate) fn render(&self, version: i32) -> String {
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
                if let Some(token) = column.value_source.as_token() {
                    out.push_str(&format!(" {token}"));
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
                if guard.nulls_not_distinct {
                    out.push_str(" nulls not distinct");
                }
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
            diff_paired_objects(
                name,
                &expected_table.unique_guards,
                &found_table.unique_guards,
                &mut findings,
            );
            diff_paired_objects(
                name,
                &expected_table.foreign_keys,
                &found_table.foreign_keys,
                &mut findings,
            );
        }
        findings
    }
}

#[cfg(test)]
const ARTIFACT_HEADER: &str = "\
# lash-postgres-store expected schema shape.
#
# Generated artifact -- never edit by hand. Regenerate after any change to
# schema.sql by running the crate's `schema_shape` suite against a live
# PostgreSQL with LASH_UPDATE_SCHEMA_SHAPE=1, which rewrites this file from the
# catalog the DDL artifact actually produces. Every attribute recorded here
# renders identically on PostgreSQL 14 through 18; CI asserts that on all three.
#
# Columns are matched by name, never by ordinal position. Uniqueness guards and
# foreign keys are matched by their column SET and kind, never by constraint or
# index name and never by column order -- UNIQUE (a, b) and UNIQUE (b, a) reject
# the same rows, so one standing in for the other is not drift. What does differ
# between same-set guards is compared: the partial predicate and null-distinctness
# for guards, the delete action for foreign keys. Column order in this file records
# how the DDL declares it and is documentation, not a requirement. Every object is
# read from the one namespace where lash_schema_versions resolves.
";

fn parse_column_line(rest: &str) -> Option<ColumnShape> {
    let mut tokens: Vec<&str> = rest.split_whitespace().collect();
    let value_source = match tokens.last().copied().and_then(ColumnValueSource::parse) {
        Some(source) => {
            tokens.pop();
            source
        }
        None => ColumnValueSource::Supplied,
    };
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
        value_source,
    })
}

fn parse_guard_line(rest: &str, primary_key: bool) -> Option<UniqueGuard> {
    let (columns, tail) = parse_column_list(rest)?;
    let mut tail = tail.trim();
    let nulls_not_distinct = match tail.strip_prefix("nulls not distinct") {
        Some(remainder) => {
            tail = remainder.trim();
            true
        }
        None => false,
    };
    let predicate = if tail.is_empty() {
        None
    } else {
        Some(tail.strip_prefix("where ")?.trim().to_string())
    };
    Some(UniqueGuard {
        primary_key,
        columns,
        predicate,
        nulls_not_distinct,
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
            Some(found_column) if !found_column.satisfies(expected_column) => {
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

/// An object class matched by *what it enforces* rather than by how it was
/// written.
///
/// Two constraints that impose the same rule are the same constraint. Column
/// order inside a uniqueness key is the clearest case: `UNIQUE (a, b)` and
/// `UNIQUE (b, a)` reject exactly the same rows, so a host that rebuilt one as the
/// other has not drifted and must not be refused. Names are the same story, and
/// PostgreSQL generates them anyway.
///
/// Matching therefore happens on [`PairedObject::identity`], and a matched pair is
/// compared on [`PairedObject::enforces_same_as`] — the attributes that do change
/// which rows are accepted. Splitting the two is what lets a same-identity pair
/// with a different partial predicate report as one precise mismatch instead of an
/// unrelated missing/unexpected pair.
trait PairedObject: Ord + Clone {
    /// What makes two objects the same constraint. Order-insensitive, and never a
    /// name.
    type Identity: Eq;

    fn identity(&self) -> Self::Identity;

    /// Whether a found object imposes the same rule as the expected one it
    /// matched. Only reached for objects that already share an identity.
    fn enforces_same_as(&self, expected: &Self) -> bool;

    fn missing(table: &str, expected: &Self) -> SchemaFinding;
    fn mismatch(table: &str, expected: &Self, found: &Self) -> SchemaFinding;
    fn unexpected(table: &str, found: &Self) -> SchemaFinding;
}

impl PairedObject for UniqueGuard {
    /// Kind plus the key column *set*. Column order affects which index prefixes
    /// can be scanned, not which rows are rejected, and this check verifies
    /// semantics rather than access-path performance — the same reason non-unique
    /// indexes are out of scope entirely.
    type Identity = (bool, BTreeSet<String>);

    fn identity(&self) -> Self::Identity {
        (self.primary_key, self.columns.iter().cloned().collect())
    }

    /// The partial predicate and null-distinctness both change the row set the
    /// guard covers, so they are compared even when the column set matches.
    fn enforces_same_as(&self, expected: &Self) -> bool {
        self.predicate == expected.predicate
            && self.nulls_not_distinct == expected.nulls_not_distinct
    }

    fn missing(table: &str, expected: &Self) -> SchemaFinding {
        SchemaFinding::MissingUniqueGuard {
            table: table.to_string(),
            expected: expected.clone(),
        }
    }

    fn mismatch(table: &str, expected: &Self, found: &Self) -> SchemaFinding {
        SchemaFinding::UniqueGuardMismatch {
            table: table.to_string(),
            expected: expected.clone(),
            found: found.clone(),
        }
    }

    fn unexpected(table: &str, found: &Self) -> SchemaFinding {
        SchemaFinding::UnexpectedUniqueGuard {
            table: table.to_string(),
            found: found.clone(),
        }
    }
}

impl PairedObject for ForeignKeyShape {
    /// The parent table plus the *set of column pairings*. Declaration order is
    /// irrelevant, but which child column references which parent column is not —
    /// pairing the columns before collecting them keeps
    /// `(a, b) -> (x, y)` distinct from `(a, b) -> (y, x)` while accepting
    /// `(b, a) -> (y, x)` as the same constraint.
    type Identity = (String, BTreeSet<(String, String)>);

    fn identity(&self) -> Self::Identity {
        (
            self.parent_table.clone(),
            self.columns
                .iter()
                .cloned()
                .zip(self.parent_columns.iter().cloned())
                .collect(),
        )
    }

    /// The delete action is what process pruning depends on.
    fn enforces_same_as(&self, expected: &Self) -> bool {
        self.on_delete == expected.on_delete
    }

    fn missing(table: &str, expected: &Self) -> SchemaFinding {
        SchemaFinding::MissingForeignKey {
            table: table.to_string(),
            expected: expected.clone(),
        }
    }

    fn mismatch(table: &str, expected: &Self, found: &Self) -> SchemaFinding {
        SchemaFinding::ForeignKeyMismatch {
            table: table.to_string(),
            expected: expected.clone(),
            found: found.clone(),
        }
    }

    fn unexpected(table: &str, found: &Self) -> SchemaFinding {
        SchemaFinding::UnexpectedForeignKey {
            table: table.to_string(),
            found: found.clone(),
        }
    }
}

/// Matches two sets of the same object class by identity, then compares each
/// matched pair on what it enforces.
///
/// Unique guards and foreign keys want exactly this walk; having it once keeps the
/// two from drifting apart as the finding vocabulary grows.
fn diff_paired_objects<T: PairedObject>(
    table: &str,
    expected: &BTreeSet<T>,
    found: &BTreeSet<T>,
    findings: &mut Vec<SchemaFinding>,
) {
    let mut unmatched: Vec<&T> = found.iter().collect();
    for expected_object in expected {
        let matched = unmatched
            .iter()
            .position(|found_object| found_object.identity() == expected_object.identity());
        match matched {
            Some(index) => {
                let found_object = unmatched.remove(index);
                if !found_object.enforces_same_as(expected_object) {
                    findings.push(T::mismatch(table, expected_object, found_object));
                }
            }
            None => findings.push(T::missing(table, expected_object)),
        }
    }
    findings.extend(
        unmatched
            .into_iter()
            .map(|found_object| T::unexpected(table, found_object)),
    );
}

/// One drifted object, named. A [`SchemaReport`] carries a list of these rather
/// than a hash comparison, so a host that mis-ported one column learns which
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaFinding {
    /// The component version stamp is absent or names another version. Always
    /// fatal at open, in every provisioning mode and regardless of
    /// [`SchemaCheck`].
    VersionMismatch {
        /// Version this build implements.
        expected: i32,
        /// Version the database is stamped with, `None` when unstamped.
        found: Option<i32>,
    },
    /// A lash-owned table does not exist in the anchored namespace.
    MissingTable {
        /// Unqualified table name.
        table: String,
    },
    /// A lash-named relation exists in a namespace earlier on the `search_path`
    /// than the anchored installation, so lash's own unqualified statements would
    /// resolve to it instead. Two partial installations must never be assembled
    /// into one apparently-conformant database.
    ShadowedTable {
        /// Unqualified table name.
        table: String,
        /// Namespace the installation is anchored in.
        expected_schema: String,
        /// Namespace `search_path` actually resolves the name to.
        found_schema: String,
    },
    /// A lash-owned column does not exist.
    MissingColumn {
        /// Table that should carry the column.
        table: String,
        /// Column this build expects.
        expected: ColumnShape,
    },
    /// A lash-owned column exists with a different type or nullability, or with a
    /// value source that does not keep every write capability lash needs — a
    /// column lash names explicitly rebuilt as `GENERATED ALWAYS`, or one lash
    /// omits rebuilt without any value source.
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
    /// A uniqueness guarantee exists over the expected column set but guards a
    /// different row set — a different key order, a different partial predicate,
    /// or `NULLS NOT DISTINCT`.
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
    /// A seed row exists but carries a value lash cannot use.
    InvalidSeedRow {
        /// Table carrying the row.
        table: String,
        /// What is wrong with it.
        detail: String,
    },
}

impl SchemaFinding {
    fn section(&self) -> &'static str {
        match self {
            Self::VersionMismatch { .. } => "COMPONENT VERSION",
            Self::MissingTable { .. } => "MISSING TABLES",
            Self::ShadowedTable { .. } => "SHADOWED TABLES",
            Self::MissingColumn { .. }
            | Self::ColumnMismatch { .. }
            | Self::UnexpectedColumn { .. } => "COLUMN DRIFT",
            Self::MissingUniqueGuard { .. }
            | Self::UniqueGuardMismatch { .. }
            | Self::UnexpectedUniqueGuard { .. } => "UNIQUE GUARD DRIFT",
            Self::MissingForeignKey { .. }
            | Self::ForeignKeyMismatch { .. }
            | Self::UnexpectedForeignKey { .. } => "FOREIGN KEY DRIFT",
            Self::MissingSeedRow { .. } | Self::InvalidSeedRow { .. } => "SEED ROWS",
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
            Self::ShadowedTable {
                table,
                expected_schema,
                found_schema,
            } => write!(
                formatter,
                "{table}: shadowed — this installation is anchored in `{expected_schema}` but \
                 search_path resolves the name to `{found_schema}`"
            ),
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
            Self::InvalidSeedRow { table, detail } => {
                write!(formatter, "{table}: seed row is unusable ({detail})")
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

    /// Whether any finding concerns the component version stamp, which no
    /// [`SchemaCheck`] can relax.
    fn has_version_finding(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| matches!(finding, SchemaFinding::VersionMismatch { .. }))
    }

    /// Whether any finding concerns the catalog's shape or seed data, which
    /// [`SchemaCheck::WarnOnly`] does relax.
    fn has_shape_finding(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| !matches!(finding, SchemaFinding::VersionMismatch { .. }))
    }

    /// Counts of findings by section, for the gate's decision evidence.
    pub(crate) fn finding_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for finding in &self.findings {
            *counts.entry(finding.section()).or_insert(0) += 1;
        }
        counts
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
            "SHADOWED TABLES",
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
        // The remedy has to match the findings. A version mismatch is
        // unconditional — `SchemaCheck::WarnOnly` cannot open past it — so
        // recommending the valve there would send a host down a path that cannot
        // work. Both remedies appear when both classes are present, which is the
        // unreadable-stamp case.
        write!(
            formatter,
            "\n\nProvision this database from the DDL artifact this build ships \
             (`PostgresStorage::schema_ddl()`, committed as \
             crates/lash-postgres-store/schema.sql) rather than transcribing it."
        )?;
        if self.has_version_finding() {
            write!(
                formatter,
                " The component schema is a reject-and-recreate boundary with no migration \
                 chain, so a database stamped for another generation needs a fresh one: this \
                 gate is unconditional and no `SchemaCheck` relaxes it."
            )?;
        }
        if self.has_shape_finding() {
            write!(
                formatter,
                " To open against a structurally drifted schema anyway, set \
                 `PostgresStoreConfig::schema_check = SchemaCheck::WarnOnly`."
            )?;
        }
        Ok(())
    }
}

#[path = "schema_shape/introspect.rs"]
mod introspect;

pub(crate) use introspect::{
    ComponentVersion, read_component_version, resolve_installation, verify_schema_shape,
};
/// Reached only by the artifact-generation and catalog tests, which drive the
/// introspection directly rather than through a full verification.
#[cfg(test)]
pub(crate) use introspect::{normalize_predicate, read_live_shape, resolve_tables};

#[path = "schema_shape/tests.rs"]
#[cfg(test)]
mod tests;
