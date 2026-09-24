use lash_core::store_backend_support::required_constraints::{
    EXPECTED_CONSTRAINTS, EXPECTED_FOREIGN_KEYS, RenderedConstraint, RenderedForeignKey,
    extract_foreign_key_clauses,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::{EffectAddress, ExecutionScope};
use std::collections::{BTreeMap, BTreeSet};

// schema_fragments.rs carries the table sets shared between databases; the
// declarations are parsed out of the concatenated source so a table moved into
// a fragment still counts as declared (FIG-3260).
const SQLITE_SCHEMA_SOURCE: &str = concat!(
    include_str!("../../lash-sqlite-store/src/schema.rs"),
    include_str!("../../lash-sqlite-store/src/schema_fragments.rs"),
);
const POSTGRES_SCHEMA_SOURCE: &str = include_str!("../../lash-postgres-store/schema.sql");
const POSTGRES_SCHEMA_SHAPE: &str = include_str!("../../lash-postgres-store/schema-shape.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // The registry model supports one-sided tables on either backend.
enum Backend {
    SQLite,
    Postgres,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parity {
    Identical,
    Divergent {
        reason: &'static str,
        sqlite_only_columns: &'static [&'static str],
        postgres_only_columns: &'static [&'static str],
    },
    OneBackendOnly {
        side: Backend,
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TablePair {
    sqlite_table: Option<&'static str>,
    postgres_table: Option<&'static str>,
    parity: Parity,
}

const TABLE_REGISTRY: &[TablePair] = &[
    TablePair {
        sqlite_table: Some("attachment_blobs"),
        postgres_table: None,
        parity: Parity::OneBackendOnly {
            side: Backend::SQLite,
            reason: "SQLite keeps attachment bytes in its catalog; a Postgres backend takes an external attachment backend at construction (ADR 0102)",
        },
    },
    pair("attachment_condemnations", "lash_attachment_condemnations"),
    pair("attachment_manifest", "lash_attachment_manifest"),
    pair(
        "artifact_owner_retirements",
        "lash_artifact_owner_retirements",
    ),
    pair("artifact_owners", "lash_artifact_owners"),
    pair("await_event_meta", "lash_await_event_meta"),
    pair(
        "await_event_revoked_sessions",
        "lash_await_event_revoked_sessions",
    ),
    pair("await_event_waits", "lash_await_event_waits"),
    pair("effect_scope_retirements", "lash_effect_scope_retirements"),
    pair("blobs", "lash_blobs"),
    pair("checkpoint_blob_refs", "lash_checkpoint_blob_refs"),
    pair("deleted_sessions", "lash_deleted_sessions"),
    pair("fork_lineage", "lash_fork_lineage"),
    pair("graph_nodes", "lash_graph_nodes"),
    TablePair {
        sqlite_table: Some("artifact_refs"),
        postgres_table: Some("lash_lashlang_artifacts"),
        parity: Parity::Divergent {
            reason: "SQLite stores a blob reference while Postgres stores inline bytes",
            sqlite_only_columns: &["blob_ref"],
            postgres_only_columns: &["artifact_bytes"],
        },
    },
    pair("node_anchors", "lash_node_anchors"),
    pair("pending_turn_inputs", "lash_pending_turn_inputs"),
    pair("process_change_clock", "lash_process_change_clock"),
    pair("release_stamp", "lash_release_stamp"),
    pair("process_events", "lash_process_events"),
    pair("process_leases", "lash_process_leases"),
    pair("process_observers", "lash_process_observers"),
    pair("parent_end_plans", "lash_parent_end_plans"),
    pair("process_artifact_cleanup", "lash_process_artifact_cleanup"),
    pair(
        "process_segment_handovers",
        "lash_process_segment_handovers",
    ),
    pair("process_tombstones", "lash_process_tombstones"),
    pair("process_wake_deliveries", "lash_process_wake_deliveries"),
    pair("processes", "lash_processes"),
    pair("queued_runs", "lash_queued_runs"),
    pair("queued_run_members", "lash_queued_run_members"),
    pair("queued_work_batches", "lash_queued_work_batches"),
    pair("queued_work_items", "lash_queued_work_items"),
    pair("runtime_effect_group", "lash_runtime_effect_group"),
    pair(
        "runtime_effect_group_child",
        "lash_runtime_effect_group_child",
    ),
    pair("runtime_effect_replay", "lash_runtime_effect_replay"),
    pair("runtime_turn_commits", "lash_runtime_turn_commits"),
    TablePair {
        sqlite_table: None,
        postgres_table: Some("lash_schema_versions"),
        parity: Parity::OneBackendOnly {
            side: Backend::Postgres,
            reason: "Postgres uses a schema-version table while SQLite uses PRAGMA user_version",
        },
    },
    pair("session_execution_leases", "lash_session_execution_leases"),
    pair("session_ingress", "lash_session_ingress"),
    pair("session_meta", "lash_session_meta"),
    pair(
        "session_meta_pending_observer_intents",
        "lash_session_meta_pending_observer_intents",
    ),
    pair("session_head", "lash_sessions"),
    pair("tool_intent_submissions", "lash_tool_intent_submissions"),
    pair("trigger_deliveries", "lash_trigger_deliveries"),
    pair(
        "trigger_mutation_receipts",
        "lash_trigger_mutation_receipts",
    ),
    pair("process_definitions", "lash_process_definitions"),
    pair("trigger_occurrences", "lash_trigger_occurrences"),
    pair("trigger_subscriptions", "lash_trigger_subscriptions"),
    pair(
        "turn_cancellation_bindings",
        "lash_turn_cancellation_bindings",
    ),
    pair(
        "turn_cancel_closure_authorizations",
        "lash_turn_cancel_closure_authorizations",
    ),
    pair(
        "turn_cancel_closure_participants",
        "lash_turn_cancel_closure_participants",
    ),
    pair(
        "turn_cancel_retired_scopes",
        "lash_turn_cancel_retired_scopes",
    ),
    pair("turn_parks", "lash_turn_parks"),
    pair("turn_park_clock", "lash_turn_park_clock"),
    pair("turn_park_events", "lash_turn_park_events"),
    TablePair {
        sqlite_table: Some("turn_cancel_requests"),
        postgres_table: Some("lash_turn_cancel_requests"),
        parity: Parity::Divergent {
            reason: "SQLite stores the typed cancel record as one JSON value while Postgres keeps request fields structural",
            sqlite_only_columns: &["record_json"],
            postgres_only_columns: &["disposition", "mode", "origin", "reason", "request_id"],
        },
    },
    TablePair {
        sqlite_table: None,
        postgres_table: Some("lash_turn_cancel_affected_inputs"),
        parity: Parity::OneBackendOnly {
            side: Backend::Postgres,
            reason: "SQLite stores the cancel record's affected inputs inside record_json; only Postgres keeps them as a structural child table",
        },
    },
    pair("usage_deltas", "lash_usage_deltas"),
    pair("wake_allocation_floors", "lash_wake_allocation_floors"),
    pair("wake_redelivery_fences", "lash_wake_redelivery_fences"),
];

const fn pair(sqlite_table: &'static str, postgres_table: &'static str) -> TablePair {
    TablePair {
        sqlite_table: Some(sqlite_table),
        postgres_table: Some(postgres_table),
        parity: Parity::Identical,
    }
}

/// A per-column nullability divergence between the two backends, declared
/// with both sides' actual values and the reason the divergence exists -- the
/// same declaration discipline `Parity::Divergent` applies to column presence
/// and `EXPECTED_CONSTRAINTS` applies to CHECK constraints. A divergence that
/// is fixed must have its row removed: the gate fails on stale declarations.
struct NullabilityDivergence {
    sqlite_table: &'static str,
    column: &'static str,
    sqlite_nullable: bool,
    postgres_nullable: bool,
    reason: &'static str,
}

const NULLABILITY_DIVERGENCES: &[NullabilityDivergence] = &[
    NullabilityDivergence {
        sqlite_table: "deleted_sessions",
        column: "created_at_ms",
        sqlite_nullable: false,
        postgres_nullable: true,
        reason: "Postgres enumeration columns were added by ALTER TABLE ... ADD COLUMN, \
                 which cannot take NOT NULL; legacy rows keep NULL where no source \
                 evidence exists (component-58 migration comment)",
    },
    NullabilityDivergence {
        sqlite_table: "deleted_sessions",
        column: "head_revision",
        sqlite_nullable: false,
        postgres_nullable: true,
        reason: "Postgres enumeration columns were added by ALTER TABLE ... ADD COLUMN, \
                 which cannot take NOT NULL; legacy rows keep NULL where no source \
                 evidence exists (component-58 migration comment)",
    },
    NullabilityDivergence {
        sqlite_table: "deleted_sessions",
        column: "relation_kind",
        sqlite_nullable: false,
        postgres_nullable: true,
        reason: "Postgres enumeration columns were added by ALTER TABLE ... ADD COLUMN, \
                 which cannot take NOT NULL; legacy rows keep NULL where no source \
                 evidence exists (component-58 migration comment)",
    },
    NullabilityDivergence {
        sqlite_table: "session_meta",
        column: "created_at_ms",
        sqlite_nullable: false,
        postgres_nullable: true,
        reason: "Postgres enumeration columns were added by ALTER TABLE ... ADD COLUMN, \
                 which cannot take NOT NULL; legacy rows keep NULL where no source \
                 evidence exists (component-58 migration comment)",
    },
];

fn consume_keyword<'a>(source: &'a str, keyword: &str) -> Option<&'a str> {
    let source = source.trim_start();
    let candidate = source.get(..keyword.len())?;
    if candidate.eq_ignore_ascii_case(keyword)
        && source
            .get(keyword.len()..)
            .is_none_or(|rest| rest.starts_with(char::is_whitespace))
    {
        source.get(keyword.len()..)
    } else {
        None
    }
}

fn consume_identifier(source: &str) -> Option<String> {
    let source = source.trim_start();
    let first = source.chars().next()?;
    let (closing, offset) = match first {
        '"' => ('"', 1),
        '`' => ('`', 1),
        '[' => (']', 1),
        _ => {
            let end = source
                .find(|character: char| character.is_whitespace() || character == '(')
                .unwrap_or(source.len());
            return (end > 0).then(|| source[..end].to_string());
        }
    };
    let rest = &source[offset..];
    let end = rest.find(closing)?;
    Some(rest[..end].to_string())
}

fn sqlite_table_names(source: &str) -> BTreeSet<String> {
    source
        .match_indices(|character: char| character.eq_ignore_ascii_case(&'c'))
        .filter_map(|(offset, _)| {
            let source = source.get(offset..)?;
            let source = consume_keyword(source, "CREATE")?;
            let source = consume_keyword(source, "TABLE")?;
            let source = consume_keyword(source, "IF")
                .and_then(|source| consume_keyword(source, "NOT"))
                .and_then(|source| consume_keyword(source, "EXISTS"))
                .unwrap_or(source);
            consume_identifier(source)
        })
        .filter(|table| {
            !matches!(
                table.as_str(),
                "session_meta_observer_intent_processes"
                    | "session_meta_fork_pending_observer_processes"
            )
        })
        .collect()
}

fn postgres_table_names(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix("table ").map(str::to_string))
        .collect()
}

fn sqlite_table_columns(source: &str, table: &str) -> BTreeSet<String> {
    let declaration = format!("CREATE TABLE IF NOT EXISTS {table} (");
    let body = source
        .split_once(&declaration)
        .unwrap_or_else(|| panic!("SQLite schema is missing registered table `{table}`"))
        .1
        .split_once("\n);")
        .unwrap_or_else(|| panic!("SQLite table `{table}` has no closing declaration"))
        .0;
    body.lines()
        .filter_map(|line| {
            let line = line.split_once("--").map_or(line, |(code, _)| code);
            let name = consume_identifier(line)?;
            (!matches!(
                name.to_ascii_uppercase().as_str(),
                "UNIQUE" | "PRIMARY" | "FOREIGN" | "CHECK" | "CONSTRAINT" | "ON"
            ))
            .then_some(name)
        })
        .collect()
}

fn postgres_table_columns(source: &str, table: &str) -> BTreeSet<String> {
    postgres_table_nullability(source, table)
        .into_keys()
        .collect()
}

/// Maps each column of a SQLite table to `true` when it is nullable. A column
/// counts as non-nullable when its line carries `NOT NULL` or `PRIMARY KEY`,
/// or when a table-level `PRIMARY KEY (...)` clause names it.
fn sqlite_table_nullability(source: &str, table: &str) -> BTreeMap<String, bool> {
    let body = ddl_table_body(source, table)
        .unwrap_or_else(|| panic!("SQLite schema is missing registered table `{table}`"));
    let mut nullable = BTreeMap::new();
    let mut primary_key_columns = Vec::new();
    for line in body.lines() {
        let line = line.split_once("--").map_or(line, |(code, _)| code);
        let Some(name) = consume_identifier(line) else {
            continue;
        };
        let upper_name = name.to_ascii_uppercase();
        if matches!(
            upper_name.as_str(),
            "UNIQUE" | "FOREIGN" | "CHECK" | "CONSTRAINT" | "ON"
        ) {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        if upper_name == "PRIMARY" {
            if let Some((_, list)) = line.split_once('(')
                && let Some((columns, _)) = list.split_once(')')
            {
                primary_key_columns.extend(
                    columns
                        .split(',')
                        .map(|column| column.trim().trim_matches('"').to_string()),
                );
            }
            continue;
        }
        nullable.insert(
            name,
            !(upper.contains("NOT NULL") || upper.contains("PRIMARY KEY")),
        );
    }
    for column in primary_key_columns {
        if let Some(entry) = nullable.get_mut(&column) {
            *entry = false;
        }
    }
    nullable
}

fn postgres_table_nullability(source: &str, table: &str) -> BTreeMap<String, bool> {
    let declaration = format!("table {table}\n");
    source
        .split_once(&declaration)
        .unwrap_or_else(|| panic!("Postgres schema shape is missing registered table `{table}`"))
        .1
        .lines()
        .take_while(|line| !line.starts_with("table "))
        .filter_map(|line| {
            let rest = line.strip_prefix("  column ")?;
            let mut tokens = rest.split_whitespace();
            let name = tokens.next()?.to_string();
            let _type = tokens.next();
            let nullable = match tokens.next() {
                Some("nullable") => true,
                Some("not-null") => false,
                other => panic!(
                    "Postgres column `{table}.{name}` carries an unrecognised nullability marker \
                     {other:?}"
                ),
            };
            Some((name, nullable))
        })
        .collect()
}

fn ddl_table_body<'a>(source: &'a str, table: &str) -> Option<&'a str> {
    let declaration = format!("CREATE TABLE IF NOT EXISTS {table} (");
    source
        .split_once(&declaration)
        .and_then(|(_, rest)| rest.split_once("\n);").map(|(body, _)| body))
}

fn normalize_sql(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ddl_constraints(source: &str) -> BTreeSet<(&str, &str)> {
    let mut constraints = BTreeSet::new();
    let mut table = None;

    for line in source.lines() {
        let line = line.trim();
        if let Some(declaration) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            table = declaration.strip_suffix(" (");
        } else if line == ");" {
            table = None;
        }

        let Some(table_name) = table else {
            continue;
        };
        let Some(declaration) = line.strip_prefix("CONSTRAINT ") else {
            continue;
        };
        let Some((name, _)) = declaration.split_once(" CHECK (") else {
            continue;
        };
        if name.starts_with("ck_") {
            constraints.insert((table_name, name));
        }
    }

    constraints
}

fn ddl_foreign_keys(
    source: &str,
    table: &str,
    dialect: &str,
) -> Result<
    Vec<lash_core::store_backend_support::required_constraints::ParsedForeignKeyClause>,
    String,
> {
    let Some(body) = ddl_table_body(source, table) else {
        return Err(format!("{dialect} DDL is missing table `{table}`"));
    };
    let ddl = format!("CREATE TABLE IF NOT EXISTS {table} (\n{body}\n);");
    extract_foreign_key_clauses(&ddl).map_err(|detail| {
        format!("{dialect} DDL table `{table}` has an unparsable clause: {detail}")
    })
}

fn validate_expected_foreign_keys(
    source: &str,
    registry: &[RenderedForeignKey],
    dialect: &str,
    tables: &[&str],
) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut declared = BTreeSet::new();
    for expected in registry {
        let identity = (
            expected.table,
            expected.columns.to_vec(),
            expected.referenced_table,
            expected.referenced_columns.to_vec(),
        );
        if !declared.insert(identity) {
            failures.push(format!(
                "{dialect} expected-foreign-keys registry duplicates {}({}) -> {}",
                expected.table,
                expected.columns.join(", "),
                expected.referenced_table
            ));
            continue;
        }
        let clauses = match ddl_foreign_keys(source, expected.table, dialect) {
            Ok(clauses) => clauses,
            Err(failure) => {
                failures.push(failure);
                continue;
            }
        };
        let matching: Vec<_> = clauses
            .iter()
            .filter(|clause| {
                clause.columns == expected.columns
                    && clause.referenced_table == expected.referenced_table
                    && clause.referenced_columns == expected.referenced_columns
            })
            .collect();
        let Some(clause) = matching.first() else {
            failures.push(format!(
                "{dialect} DDL table `{}` is missing registered foreign key ({}) REFERENCES {}({})",
                expected.table,
                expected.columns.join(", "),
                expected.referenced_table,
                expected.referenced_columns.join(", ")
            ));
            continue;
        };
        let mut drift = Vec::new();
        if clause.on_delete != expected.on_delete {
            drift.push(format!(
                "on delete: registered `{}`, declared `{}`",
                expected.on_delete, clause.on_delete
            ));
        }
        if clause.on_update != expected.on_update {
            drift.push(format!(
                "on update: registered `{}`, declared `{}`",
                expected.on_update, clause.on_update
            ));
        }
        if clause.deferrable != expected.deferrable {
            drift.push(format!(
                "deferrable: registered {}, declared {}",
                expected.deferrable, clause.deferrable
            ));
        }
        if clause.initially_deferred != expected.initially_deferred {
            drift.push(format!(
                "initially deferred: registered {}, declared {}",
                expected.initially_deferred, clause.initially_deferred
            ));
        }
        if !drift.is_empty() {
            failures.push(format!(
                "{dialect} DDL table `{}` foreign key ({}) REFERENCES {}({}) drifted: {}",
                expected.table,
                expected.columns.join(", "),
                expected.referenced_table,
                expected.referenced_columns.join(", "),
                drift.join("; ")
            ));
        }
    }
    for table in tables {
        let Ok(clauses) = ddl_foreign_keys(source, table, dialect) else {
            continue;
        };
        for clause in clauses {
            if !registry.iter().any(|expected| {
                expected.table == *table
                    && clause.columns == expected.columns
                    && clause.referenced_table == expected.referenced_table
                    && clause.referenced_columns == expected.referenced_columns
            }) {
                failures.push(format!(
                    "{dialect} DDL table `{table}` declares unregistered foreign key ({}) REFERENCES {}({}); add it to the expected-foreign-keys registry",
                    clause.columns.join(", "),
                    clause.referenced_table,
                    clause.referenced_columns.join(", ")
                ));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn validate_expected_constraints(
    source: &str,
    registry: &[RenderedConstraint],
    dialect: &str,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut declared = BTreeSet::new();
    for expected in registry {
        if !declared.insert((expected.table, expected.name)) {
            failures.push(format!(
                "{dialect} expected-constraints registry duplicates {}.{}",
                expected.table, expected.name
            ));
            continue;
        }
        let Some(body) = ddl_table_body(source, expected.table) else {
            failures.push(format!(
                "{dialect} DDL is missing table `{}` required by constraint `{}`",
                expected.table, expected.name
            ));
            continue;
        };
        let declaration = format!(
            "CONSTRAINT {} CHECK ({})",
            expected.name, expected.expression
        );
        if !normalize_sql(body).contains(&normalize_sql(&declaration)) {
            failures.push(format!(
                "{dialect} DDL table `{}` is missing registered constraint `{}` with expression `{}`",
                expected.table, expected.name, expected.expression
            ));
        }
    }
    for (table, name) in ddl_constraints(source) {
        if !registry
            .iter()
            .any(|expected| expected.table == table && expected.name == name)
        {
            failures.push(format!(
                "{dialect} DDL table `{table}` contains unregistered constraint `{name}`; add it to the expected-constraints registry"
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn row_name(row: &TablePair) -> String {
    format!(
        "TABLE_REGISTRY row sqlite_table={:?}, postgres_table={:?}",
        row.sqlite_table, row.postgres_table
    )
}

fn validate_registry(sqlite_source: &str, postgres_source: &str) -> Result<(), String> {
    let sqlite_tables = sqlite_table_names(sqlite_source);
    let postgres_tables = postgres_table_names(postgres_source);
    let mut failures = Vec::new();

    if sqlite_tables.is_empty() {
        failures.push("SQLite schema scrape returned no tables".to_string());
    }
    if postgres_tables.is_empty() {
        failures.push("Postgres schema scrape returned no tables".to_string());
    }

    for table in &sqlite_tables {
        if !TABLE_REGISTRY
            .iter()
            .any(|row| row.sqlite_table == Some(table.as_str()))
        {
            failures.push(format!(
                "unregistered SQLite table `{table}`; add a TABLE_REGISTRY row with \
                 sqlite_table=Some(\"{table}\")"
            ));
        }
    }
    for table in &postgres_tables {
        if !TABLE_REGISTRY
            .iter()
            .any(|row| row.postgres_table == Some(table.as_str()))
        {
            failures.push(format!(
                "unregistered Postgres table `{table}`; add a TABLE_REGISTRY row with \
                 postgres_table=Some(\"{table}\")"
            ));
        }
    }

    let mut registered_sqlite = BTreeSet::new();
    let mut registered_postgres = BTreeSet::new();
    for row in TABLE_REGISTRY {
        let row_name = row_name(row);
        if let Some(table) = row.sqlite_table {
            if !registered_sqlite.insert(table) {
                failures.push(format!("{row_name} duplicates SQLite table `{table}`"));
            }
            if !sqlite_tables.contains(table) {
                failures.push(format!(
                    "{row_name} names SQLite table `{table}` missing from the scrape; edit this row"
                ));
            }
        }
        if let Some(table) = row.postgres_table {
            if !registered_postgres.insert(table) {
                failures.push(format!("{row_name} duplicates Postgres table `{table}`"));
            }
            if !postgres_tables.contains(table) {
                failures.push(format!(
                    "{row_name} names Postgres table `{table}` missing from the scrape; edit this row"
                ));
            }
        }

        match row.parity {
            Parity::Identical => {
                let (Some(sqlite_table), Some(postgres_table)) =
                    (row.sqlite_table, row.postgres_table)
                else {
                    failures.push(format!(
                        "{row_name} is Identical but does not name both backend tables; edit this row"
                    ));
                    continue;
                };
                if sqlite_tables.contains(sqlite_table) && postgres_tables.contains(postgres_table)
                {
                    let sqlite_columns = sqlite_table_columns(sqlite_source, sqlite_table);
                    let postgres_columns = postgres_table_columns(postgres_source, postgres_table);
                    if sqlite_columns != postgres_columns {
                        failures.push(format!(
                            "{row_name} is Identical but column sets differ: sqlite_only={:?}, \
                             postgres_only={:?}; edit this registry row",
                            sqlite_columns
                                .difference(&postgres_columns)
                                .collect::<Vec<_>>(),
                            postgres_columns
                                .difference(&sqlite_columns)
                                .collect::<Vec<_>>()
                        ));
                    }
                }
            }
            Parity::Divergent {
                reason,
                sqlite_only_columns,
                postgres_only_columns,
            } => {
                let (Some(sqlite_table), Some(postgres_table)) =
                    (row.sqlite_table, row.postgres_table)
                else {
                    failures.push(format!(
                        "{row_name} is Divergent but does not name both backend tables; edit this row"
                    ));
                    continue;
                };
                if sqlite_tables.contains(sqlite_table) && postgres_tables.contains(postgres_table)
                {
                    let sqlite_columns = sqlite_table_columns(sqlite_source, sqlite_table);
                    let postgres_columns = postgres_table_columns(postgres_source, postgres_table);
                    let actual_sqlite_only = sqlite_columns
                        .difference(&postgres_columns)
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>();
                    let actual_postgres_only = postgres_columns
                        .difference(&sqlite_columns)
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>();
                    let declared_sqlite_only =
                        sqlite_only_columns.iter().copied().collect::<BTreeSet<_>>();
                    let declared_postgres_only = postgres_only_columns
                        .iter()
                        .copied()
                        .collect::<BTreeSet<_>>();
                    if actual_sqlite_only != declared_sqlite_only
                        || actual_postgres_only != declared_postgres_only
                    {
                        failures.push(format!(
                            "{row_name} has divergence beyond its declaration ({reason}): \
                             declared sqlite_only={declared_sqlite_only:?}, actual \
                             sqlite_only={actual_sqlite_only:?}, declared \
                             postgres_only={declared_postgres_only:?}, actual \
                             postgres_only={actual_postgres_only:?}; edit this registry row"
                        ));
                    }
                }
            }
            Parity::OneBackendOnly { side, reason } => {
                let valid = match side {
                    Backend::SQLite => row.sqlite_table.is_some() && row.postgres_table.is_none(),
                    Backend::Postgres => row.sqlite_table.is_none() && row.postgres_table.is_some(),
                };
                if !valid {
                    failures.push(format!(
                        "{row_name} is OneBackendOnly({side:?}) but its table names disagree \
                         ({reason}); edit this row"
                    ));
                }
            }
        }
    }

    validate_nullability(
        sqlite_source,
        postgres_source,
        &sqlite_tables,
        &postgres_tables,
        &mut failures,
    );

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// Each mismatch must be declared in `NULLABILITY_DIVERGENCES` with the correct direction and
/// a reason; a declaration whose columns no longer disagree is itself a failure, so the list
/// cannot go stale.
fn validate_nullability(
    sqlite_source: &str,
    postgres_source: &str,
    sqlite_tables: &BTreeSet<String>,
    postgres_tables: &BTreeSet<String>,
    failures: &mut Vec<String>,
) {
    let mut declared = BTreeSet::new();
    for divergence in NULLABILITY_DIVERGENCES {
        if !declared.insert((divergence.sqlite_table, divergence.column)) {
            failures.push(format!(
                "NULLABILITY_DIVERGENCES duplicates {}.{}",
                divergence.sqlite_table, divergence.column
            ));
        }
    }

    for row in TABLE_REGISTRY {
        let (Some(sqlite_table), Some(postgres_table)) = (row.sqlite_table, row.postgres_table)
        else {
            continue;
        };
        if !sqlite_tables.contains(sqlite_table) || !postgres_tables.contains(postgres_table) {
            continue;
        }
        let row_name = row_name(row);
        let sqlite_columns = sqlite_table_nullability(sqlite_source, sqlite_table);
        let postgres_columns = postgres_table_nullability(postgres_source, postgres_table);

        // The nullability scraper must see exactly the columns the name
        // scraper reports; otherwise a parser drift could skip a column and
        // let a real mismatch pass unseen.
        let sqlite_names = sqlite_table_columns(sqlite_source, sqlite_table);
        if sqlite_columns.keys().cloned().collect::<BTreeSet<_>>() != sqlite_names {
            failures.push(format!(
                "{row_name} nullability scrape disagrees with the column scrape on SQLite \
                 table `{sqlite_table}`; fix the scraper"
            ));
            continue;
        }

        let mut actual: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
        for (column, sqlite_nullable) in &sqlite_columns {
            if let Some(postgres_nullable) = postgres_columns.get(column.as_str())
                && postgres_nullable != sqlite_nullable
            {
                actual.insert(column.as_str(), (*sqlite_nullable, *postgres_nullable));
            }
        }

        for divergence in NULLABILITY_DIVERGENCES
            .iter()
            .filter(|divergence| divergence.sqlite_table == sqlite_table)
        {
            match actual.get(divergence.column) {
                Some(&(sqlite_nullable, postgres_nullable))
                    if sqlite_nullable == divergence.sqlite_nullable
                        && postgres_nullable == divergence.postgres_nullable => {}
                Some(&(sqlite_nullable, postgres_nullable)) => failures.push(format!(
                    "{row_name} column `{}` is declared divergent with \
                     sqlite_nullable={}, postgres_nullable={} but actually \
                     sqlite_nullable={sqlite_nullable}, \
                     postgres_nullable={postgres_nullable}; edit the declaration",
                    divergence.column, divergence.sqlite_nullable, divergence.postgres_nullable
                )),
                None => failures.push(format!(
                    "{row_name} column `{}` is declared divergent ({}) but the backends now \
                     agree or the column is not shared; remove the declaration",
                    divergence.column, divergence.reason
                )),
            }
        }

        for (column, (sqlite_nullable, postgres_nullable)) in &actual {
            if !NULLABILITY_DIVERGENCES.iter().any(|divergence| {
                divergence.sqlite_table == sqlite_table && divergence.column == *column
            }) {
                failures.push(format!(
                    "{row_name} column `{column}` disagrees on nullability \
                     (sqlite_nullable={sqlite_nullable}, \
                     postgres_nullable={postgres_nullable}); declare it in \
                     NULLABILITY_DIVERGENCES with its reason or align the DDL"
                ));
            }
        }
    }
}

#[test]
fn schema_congruence_registry_matches_both_backends() {
    if let Err(failures) = validate_registry(SQLITE_SCHEMA_SOURCE, POSTGRES_SCHEMA_SHAPE) {
        panic!("cross-backend schema registry validation failed:\n{failures}");
    }
}

fn sqlite_expected_constraints() -> Vec<RenderedConstraint> {
    EXPECTED_CONSTRAINTS
        .iter()
        .filter_map(|constraint| constraint.sqlite)
        .collect()
}

fn postgres_expected_constraints() -> Vec<RenderedConstraint> {
    EXPECTED_CONSTRAINTS
        .iter()
        .filter_map(|constraint| constraint.postgres)
        .collect()
}

#[test]
fn schema_congruence_expected_constraints_match_both_backends() {
    let sqlite_registry = sqlite_expected_constraints();
    let postgres_registry = postgres_expected_constraints();
    for (dialect, source, registry) in [
        ("SQLite", SQLITE_SCHEMA_SOURCE, &sqlite_registry[..]),
        ("Postgres", POSTGRES_SCHEMA_SOURCE, &postgres_registry[..]),
    ] {
        if let Err(failures) = validate_expected_constraints(source, registry, dialect) {
            panic!("{dialect} expected-constraints validation failed:\n{failures}");
        }
    }
}

fn sqlite_expected_foreign_keys() -> Vec<RenderedForeignKey> {
    EXPECTED_FOREIGN_KEYS
        .iter()
        .filter_map(|key| key.sqlite)
        .collect()
}

fn postgres_expected_foreign_keys() -> Vec<RenderedForeignKey> {
    EXPECTED_FOREIGN_KEYS
        .iter()
        .filter_map(|key| key.postgres)
        .collect()
}

#[test]
fn schema_congruence_expected_foreign_keys_match_both_backends() {
    let sqlite_registry = sqlite_expected_foreign_keys();
    let postgres_registry = postgres_expected_foreign_keys();
    let sqlite_tables: Vec<&str> = TABLE_REGISTRY
        .iter()
        .filter_map(|row| row.sqlite_table)
        .collect();
    let postgres_tables: Vec<&str> = TABLE_REGISTRY
        .iter()
        .filter_map(|row| row.postgres_table)
        .collect();
    for (dialect, source, registry, tables) in [
        (
            "SQLite",
            SQLITE_SCHEMA_SOURCE,
            &sqlite_registry[..],
            &sqlite_tables[..],
        ),
        (
            "Postgres",
            POSTGRES_SCHEMA_SOURCE,
            &postgres_registry[..],
            &postgres_tables[..],
        ),
    ] {
        if let Err(failures) = validate_expected_foreign_keys(source, registry, dialect, tables) {
            panic!("{dialect} expected-foreign-keys validation failed:\n{failures}");
        }
    }
}

#[test]
fn schema_congruence_rejects_a_dropped_registered_foreign_key() {
    let sqlite_registry = sqlite_expected_foreign_keys();
    let postgres_registry = postgres_expected_foreign_keys();
    for (dialect, source, registry, declaration) in [
        (
            "SQLite",
            SQLITE_SCHEMA_SOURCE,
            &sqlite_registry[..],
            "    CONSTRAINT fk_runtime_effect_group_child_group FOREIGN KEY (group_key) REFERENCES runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,\n",
        ),
        (
            "Postgres",
            POSTGRES_SCHEMA_SOURCE,
            &postgres_registry[..],
            "    CONSTRAINT fk_runtime_effect_group_child_group FOREIGN KEY (group_key) REFERENCES lash_runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED,\n",
        ),
    ] {
        let dropped = source.replacen(declaration, "", 1);
        assert_ne!(dropped, source, "{dialect} witness did not drop a key");
        let failure = validate_expected_foreign_keys(&dropped, registry, dialect, &[])
            .expect_err("dropping a registered foreign key must fail the congruence gate");
        assert!(
            failure.contains("missing registered foreign key (group_key) REFERENCES"),
            "unexpected {dialect} dropped-key failure: {failure}"
        );
    }
}

#[test]
fn schema_congruence_rejects_an_unregistered_foreign_key() {
    let sqlite_registry = sqlite_expected_foreign_keys();
    let postgres_registry = postgres_expected_foreign_keys();
    for (dialect, source, registry, table) in [
        (
            "SQLite",
            SQLITE_SCHEMA_SOURCE,
            &sqlite_registry[..],
            "blobs",
        ),
        (
            "Postgres",
            POSTGRES_SCHEMA_SOURCE,
            &postgres_registry[..],
            "lash_blobs",
        ),
    ] {
        let Some(body) = ddl_table_body(source, table) else {
            panic!("{dialect} witness table `{table}` is missing");
        };
        let tampered = source.replacen(
            &format!("CREATE TABLE IF NOT EXISTS {table} ({body}\n);"),
            &format!(
                "CREATE TABLE IF NOT EXISTS {table} ({body},\n    FOREIGN KEY (orphan_probe) REFERENCES {table}(hash)\n);"
            ),
            1,
        );
        assert_ne!(tampered, source, "{dialect} witness did not add a key");
        let failure = validate_expected_foreign_keys(&tampered, registry, dialect, &[table])
            .expect_err("an unregistered foreign key must fail the congruence gate");
        assert!(
            failure.contains("unregistered foreign key"),
            "unexpected {dialect} unregistered-key failure: {failure}"
        );
    }
}

#[test]
fn every_check_in_the_ddl_is_named_and_registered() {
    // The inspector matches CHECKs by name, so an anonymous CHECK is invisible
    // to the congruence gate: dropping one would pass. Every CHECK in a table
    // body must be `CONSTRAINT ck_...`-named (and therefore registered, since
    // `validate_expected_constraints` rejects unregistered names).
    for (dialect, source) in [
        ("SQLite", SQLITE_SCHEMA_SOURCE),
        ("Postgres", POSTGRES_SCHEMA_SOURCE),
    ] {
        let mut failures = Vec::new();
        let mut rest = source;
        // Only `CREATE TABLE IF NOT EXISTS` bodies are live DDL -- plain
        // `CREATE TABLE` appears only in migration scripts and test fixtures,
        // which are deliberately allowed anonymous CHECKs.
        while let Some(start) = rest.find("CREATE TABLE IF NOT EXISTS ") {
            let after = &rest[start + "CREATE TABLE IF NOT EXISTS ".len()..];
            let Some((name, body_and_rest)) = after.split_once(" (") else {
                break;
            };
            let Some((body, tail)) = body_and_rest.split_once("\n);") else {
                // Not a schema-style table (e.g. an indented fixture string):
                // skip past the opening line and keep scanning.
                rest = body_and_rest;
                continue;
            };
            // Line comments may carry the word "check"; strip them first.
            let body = body
                .lines()
                .map(|line| {
                    let line = line.split_once("--").map_or(line, |(code, _)| code);
                    line.split_once("//").map_or(line, |(code, _)| code)
                })
                .collect::<Vec<_>>()
                .join(" ");
            let normalized = normalize_sql(&body);
            let mut scan = normalized.as_str();
            while let Some(position) = scan.find(" CHECK") {
                let before = scan[..position].trim_end();
                let mut words = before.rsplit(' ');
                let constraint_name = words.next().unwrap_or("");
                let keyword = words.next().unwrap_or("");
                if !(keyword == "CONSTRAINT" && constraint_name.starts_with("ck_")) {
                    let context = &before[before.len().saturating_sub(60)..];
                    failures.push(format!(
                        "{dialect} `{name}` anonymous CHECK near `{context}`"
                    ));
                }
                scan = &scan[position + " CHECK".len()..];
            }
            rest = tail;
        }
        assert!(
            failures.is_empty(),
            "{dialect} anonymous CHECKs found:\n{}",
            failures.join("\n")
        );
    }
}

#[test]
fn schema_congruence_rejects_a_dropped_registered_constraint() {
    let sqlite_registry = sqlite_expected_constraints();
    let postgres_registry = postgres_expected_constraints();
    for (dialect, source, registry, declaration) in [
        (
            "SQLite",
            SQLITE_SCHEMA_SOURCE,
            &sqlite_registry[..],
            "    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),\n",
        ),
        (
            "Postgres",
            POSTGRES_SCHEMA_SOURCE,
            &postgres_registry[..],
            "    CONSTRAINT ck_processes_status CHECK (status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')),\n",
        ),
    ] {
        let dropped = source.replacen(declaration, "", 1);
        assert_ne!(
            dropped, source,
            "{dialect} witness did not drop a constraint"
        );
        let failure = validate_expected_constraints(&dropped, registry, dialect)
            .expect_err("dropping a registered constraint must fail the congruence gate");
        assert!(
            failure.contains("missing registered constraint `ck_processes_status`"),
            "unexpected {dialect} dropped-constraint failure: {failure}"
        );
    }
}

#[test]
fn registered_constraint_vocabularies_match_the_rust_writers() {
    use lash_core::facade_support::effect_replay_driver::EffectRowStatus;
    use lash_core::store_backend_support::SessionMetaCodec;
    use lash_core::{
        CausalRef, DeliveryPolicy, GroupWakePolicy, LoserPolicy, ProcessStatus, QueuedWorkKind,
        SessionMeta, SessionRelation, ToolIntentKind, TurnInputCheckpointBoundary,
        TurnInputIngress, TurnInputState, TurnInputStateKind, WakeDeliveryState, WakeDiscardReason,
    };

    assert_eq!(
        TurnInputStateKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>(),
        [
            "pending_active",
            "deferred_next_turn",
            "accepted",
            "cancelled",
            "completed",
        ]
    );
    let active_ingress =
        TurnInputIngress::active_turn("turn", TurnInputCheckpointBoundary::AfterWork);
    let next_ingress = TurnInputIngress::next_turn();
    let opened = TurnInputState::open(active_ingress.clone());
    assert_eq!(opened.kind(), TurnInputStateKind::PendingActive);
    assert_eq!(opened.ingress(), active_ingress);
    assert_eq!(
        TurnInputState::open(next_ingress.clone()),
        TurnInputState::DeferredNextTurn
    );
    assert_eq!(
        serde_json::to_value(&active_ingress)
            .expect("serialize active-turn ingress")
            .pointer("/scope")
            .and_then(serde_json::Value::as_str),
        Some("active_turn")
    );
    assert_eq!(
        serde_json::to_value(&next_ingress)
            .expect("serialize next-turn ingress")
            .pointer("/scope")
            .and_then(serde_json::Value::as_str),
        Some("next_turn")
    );

    assert_eq!(
        [QueuedWorkKind::Turn, QueuedWorkKind::Control].map(QueuedWorkKind::as_str),
        ["turn", "control"]
    );
    assert_eq!(
        [
            DeliveryPolicy::EarliestSafeBoundary,
            DeliveryPolicy::AfterCurrentTurnCommit,
        ]
        .map(DeliveryPolicy::as_str),
        ["earliest_safe_boundary", "after_current_turn_commit"]
    );

    assert_eq!(
        [
            ProcessStatus::Running,
            ProcessStatus::Waiting,
            ProcessStatus::Completed,
            ProcessStatus::Failed,
            ProcessStatus::Cancelled,
            ProcessStatus::Abandoned,
            ProcessStatus::CallerDeparted,
        ]
        .map(|status| status.label()),
        [
            "running",
            "waiting",
            "completed",
            "failed",
            "cancelled",
            "abandoned",
            "caller_departed",
        ]
    );
    assert_eq!(
        [
            WakeDeliveryState::Pending,
            WakeDeliveryState::Enqueuing,
            WakeDeliveryState::Enqueued,
            WakeDeliveryState::Discarded,
        ]
        .map(WakeDeliveryState::as_str),
        ["pending", "enqueuing", "enqueued", "discarded"]
    );
    assert_eq!(
        [
            WakeDiscardReason::Expired,
            WakeDiscardReason::TargetGone,
            WakeDiscardReason::Retargeted,
            WakeDiscardReason::SequenceRewound,
        ]
        .map(WakeDiscardReason::as_str),
        ["expired", "target_gone", "retargeted", "sequence_rewound"]
    );
    assert_eq!(
        [
            EffectRowStatus::InProgress,
            EffectRowStatus::Completed,
            EffectRowStatus::Failed,
        ]
        .map(EffectRowStatus::column),
        ["in_progress", "completed", "failed"]
    );
    // The effect-group policy columns spell the same snake_case strings their
    // serde encoding uses; `EffectGroupColumn` is deliberately unexported, so
    // the DDL vocabulary is pinned here through serialization.
    assert_eq!(
        [
            GroupWakePolicy::First,
            GroupWakePolicy::FirstSuccess,
            GroupWakePolicy::All,
        ]
        .map(|policy| {
            serde_json::to_value(policy)
                .expect("serialize group wake policy")
                .as_str()
                .expect("group wake policy serializes as a string")
                .to_string()
        }),
        ["first", "first_success", "all"]
    );
    assert_eq!(
        [LoserPolicy::RunToCompletion, LoserPolicy::Cancel].map(|policy| {
            serde_json::to_value(policy)
                .expect("serialize loser policy")
                .as_str()
                .expect("loser policy serializes as a string")
                .to_string()
        }),
        ["run_to_completion", "cancel"]
    );
    assert_eq!(
        [
            ToolIntentKind::StartProcess,
            ToolIntentKind::SignalProcess,
            ToolIntentKind::CancelProcess,
            ToolIntentKind::EmitProcessEvent,
            ToolIntentKind::EmitTrigger,
        ]
        .map(ToolIntentKind::as_str),
        [
            "start_process",
            "signal_process",
            "cancel_process",
            "emit_process_event",
            "emit_trigger",
        ]
    );

    let codec = SessionMetaCodec::new("INTEGER");
    let encode = |relation| {
        codec
            .encode(&SessionMeta {
                session_id: SessionId::from("session"),
                relation,
                pending_observer_intents: Vec::new(),
            })
            .expect("encode session metadata vocabulary witness")
    };
    assert_eq!(encode(SessionRelation::Root).relation_kind, "root");
    assert_eq!(
        encode(SessionRelation::Child {
            parent_session_id: SessionId::from("parent"),
            caused_by: None,
        })
        .relation_kind,
        "child"
    );
    let causal_kinds = [
        CausalRef::Turn {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
        },
        CausalRef::Effect {
            address: EffectAddress::new(ExecutionScope::runtime_operation("operation"), "effect")
                .expect("valid effect cause address"),
        },
        CausalRef::ToolCall {
            session_id: SessionId::from("session"),
            call_id: "call".to_string(),
        },
        CausalRef::Process {
            process_id: ProcessId::from("process"),
        },
        CausalRef::ProcessEvent {
            process_id: ProcessId::from("process"),
            sequence: 1,
        },
        CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence".to_string(),
            subscription_id: None,
            subscription_incarnation: None,
            subscription_revision: None,
        },
        CausalRef::SessionNode {
            session_id: SessionId::from("session"),
            node_id: "node".to_string(),
        },
    ]
    .map(|caused_by| {
        encode(SessionRelation::Child {
            parent_session_id: SessionId::from("parent"),
            caused_by: Some(caused_by),
        })
        .cause
        .kind
        .expect("causal metadata carries a kind")
    });
    assert_eq!(
        causal_kinds,
        [
            "turn",
            "effect_address",
            "tool_call",
            "process",
            "process_event",
            "trigger_occurrence",
            "session_node",
        ]
    );

    // Exhaustiveness guards. The vocabularies above are spelled by hand so a
    // silent drift in one generator cannot move both the DDL and the test, but
    // a hand-written list cannot notice a *new* variant on its own. These
    // matches make adding one a compile error until the variant is spelled
    // above and admitted by both dialects' CHECK. `WakeDiscardReason` is
    // `#[non_exhaustive]` and deliberately additive, so it has no guard: the
    // wake-delivery `discard_reason` CHECK must be widened deliberately.
    fn exhaustive_process_status(status: ProcessStatus) {
        match status {
            ProcessStatus::Running
            | ProcessStatus::Waiting
            | ProcessStatus::Completed
            | ProcessStatus::Failed
            | ProcessStatus::Cancelled
            | ProcessStatus::Abandoned
            | ProcessStatus::CallerDeparted => {}
        }
    }
    fn exhaustive_turn_input_state(state: TurnInputState) {
        match state {
            TurnInputState::PendingActive(_)
            | TurnInputState::DeferredNextTurn
            | TurnInputState::Accepted(_)
            | TurnInputState::Cancelled(_)
            | TurnInputState::Completed(_) => {}
        }
    }
    fn exhaustive_turn_input_ingress(ingress: &TurnInputIngress) {
        match ingress {
            TurnInputIngress::ActiveTurn { .. } | TurnInputIngress::NextTurn => {}
        }
    }
    fn exhaustive_queued_work_kind(kind: QueuedWorkKind) {
        match kind {
            QueuedWorkKind::Turn | QueuedWorkKind::Control => {}
        }
    }
    fn exhaustive_delivery_policy(policy: DeliveryPolicy) {
        match policy {
            DeliveryPolicy::EarliestSafeBoundary | DeliveryPolicy::AfterCurrentTurnCommit => {}
        }
    }
    fn exhaustive_wake_delivery_state(state: WakeDeliveryState) {
        match state {
            WakeDeliveryState::Pending
            | WakeDeliveryState::Enqueuing
            | WakeDeliveryState::Enqueued
            | WakeDeliveryState::Discarded => {}
        }
    }
    fn exhaustive_tool_intent_kind(kind: ToolIntentKind) {
        match kind {
            ToolIntentKind::StartProcess
            | ToolIntentKind::SignalProcess
            | ToolIntentKind::CancelProcess
            | ToolIntentKind::EmitProcessEvent
            | ToolIntentKind::EmitTrigger
            | ToolIntentKind::RegisterProcessDefinition
            | ToolIntentKind::RegisterTrigger => {}
        }
    }
    fn exhaustive_effect_row_status(status: EffectRowStatus) {
        match status {
            EffectRowStatus::InProgress | EffectRowStatus::Completed | EffectRowStatus::Failed => {}
        }
    }
    fn exhaustive_group_wake_policy(policy: GroupWakePolicy) {
        match policy {
            GroupWakePolicy::First | GroupWakePolicy::FirstSuccess | GroupWakePolicy::All => {}
        }
    }
    fn exhaustive_loser_policy(policy: LoserPolicy) {
        match policy {
            LoserPolicy::RunToCompletion | LoserPolicy::Cancel => {}
        }
    }
    fn exhaustive_session_relation(relation: &SessionRelation) {
        match relation {
            SessionRelation::Root
            | SessionRelation::Child { .. }
            | SessionRelation::Fork { .. } => {}
        }
    }
    fn exhaustive_causal_ref(caused_by: &CausalRef) {
        match caused_by {
            CausalRef::Turn { .. }
            | CausalRef::Effect { .. }
            | CausalRef::ToolCall { .. }
            | CausalRef::Process { .. }
            | CausalRef::ProcessEvent { .. }
            | CausalRef::TriggerOccurrence { .. }
            | CausalRef::SessionNode { .. } => {}
        }
    }
    exhaustive_process_status(ProcessStatus::Running);
    exhaustive_turn_input_state(TurnInputState::open(active_ingress.clone()));
    exhaustive_turn_input_ingress(&next_ingress);
    exhaustive_queued_work_kind(QueuedWorkKind::Turn);
    exhaustive_delivery_policy(DeliveryPolicy::EarliestSafeBoundary);
    exhaustive_wake_delivery_state(WakeDeliveryState::Pending);
    exhaustive_tool_intent_kind(ToolIntentKind::StartProcess);
    exhaustive_effect_row_status(EffectRowStatus::InProgress);
    exhaustive_group_wake_policy(GroupWakePolicy::First);
    exhaustive_loser_policy(LoserPolicy::Cancel);
    exhaustive_session_relation(&SessionRelation::Root);
    exhaustive_causal_ref(&CausalRef::Process {
        process_id: ProcessId::from("process"),
    });
}

#[test]
fn schema_congruence_scrapes_must_not_be_empty() {
    let sqlite_failure = validate_registry("", POSTGRES_SCHEMA_SHAPE)
        .expect_err("an empty SQLite scrape must fail registry validation");
    assert!(
        sqlite_failure.contains("SQLite schema scrape returned no tables"),
        "unexpected empty-SQLite failure: {sqlite_failure}"
    );

    let postgres_failure = validate_registry(SQLITE_SCHEMA_SOURCE, "")
        .expect_err("an empty Postgres scrape must fail registry validation");
    assert!(
        postgres_failure.contains("Postgres schema scrape returned no tables"),
        "unexpected empty-Postgres failure: {postgres_failure}"
    );
}

#[test]
fn schema_congruence_no_space_table_constraint_is_not_a_column() {
    let columns = sqlite_table_columns(SQLITE_SCHEMA_SOURCE, "trigger_subscriptions");
    assert!(!columns.contains("UNIQUE"));
    assert!(!columns.iter().any(|column| column.starts_with("UNIQUE(")));
}

#[test]
fn schema_congruence_identical_row_rejects_a_fabricated_difference() {
    let fabricated_sqlite = SQLITE_SCHEMA_SOURCE.replacen(
        "    content BLOB NOT NULL\n);",
        "    content BLOB NOT NULL,\n    injected_column TEXT\n);",
        1,
    );
    assert_ne!(fabricated_sqlite, SQLITE_SCHEMA_SOURCE);

    let failure = validate_registry(&fabricated_sqlite, POSTGRES_SCHEMA_SHAPE)
        .expect_err("an Identical row must reject a fabricated column difference");
    assert!(
        failure.contains(
            "sqlite_table=Some(\"blobs\"), postgres_table=Some(\"lash_blobs\") is Identical"
        ),
        "unexpected fabricated-difference failure: {failure}"
    );
    assert!(failure.contains("injected_column"));
}

#[test]
fn schema_congruence_rejects_an_undeclared_nullability_divergence() {
    // `blobs.content` is NOT NULL on both backends; relaxing only the SQLite
    // side must fail the gate and name the column.
    let fabricated_sqlite =
        SQLITE_SCHEMA_SOURCE.replacen("    content BLOB NOT NULL\n);", "    content BLOB\n);", 1);
    assert_ne!(fabricated_sqlite, SQLITE_SCHEMA_SOURCE);

    let failure = validate_registry(&fabricated_sqlite, POSTGRES_SCHEMA_SHAPE)
        .expect_err("an undeclared nullability divergence must fail the gate");
    assert!(
        failure.contains("column `content` disagrees on nullability"),
        "unexpected nullability failure: {failure}"
    );
    assert!(
        failure.contains("sqlite_table=Some(\"blobs\"), postgres_table=Some(\"lash_blobs\")"),
        "nullability failure must name the registry row: {failure}"
    );

    // The reverse direction is gated too: `graph_nodes.parent_node_id` is
    // nullable on both backends; tightening only the Postgres side must fail.
    let fabricated_postgres = POSTGRES_SCHEMA_SHAPE.replacen(
        "  column parent_node_id text nullable",
        "  column parent_node_id text not-null",
        1,
    );
    assert_ne!(fabricated_postgres, POSTGRES_SCHEMA_SHAPE);
    let failure = validate_registry(SQLITE_SCHEMA_SOURCE, &fabricated_postgres)
        .expect_err("a Postgres-side nullability change must fail the gate");
    assert!(
        failure.contains("column `parent_node_id` disagrees on nullability"),
        "unexpected Postgres-side nullability failure: {failure}"
    );
}

#[test]
fn schema_congruence_declared_nullability_divergences_are_not_stale() {
    // Removing a real divergence from the SQLite DDL (making it agree with
    // Postgres) must fail the gate: the declaration would be a lie.
    let aligned_sqlite = SQLITE_SCHEMA_SOURCE.replacen(
        "    relation_kind     TEXT NOT NULL,\n    parent_session_id TEXT\n);",
        "    relation_kind     TEXT,\n    parent_session_id TEXT\n);",
        1,
    );
    assert_ne!(aligned_sqlite, SQLITE_SCHEMA_SOURCE);
    let failure = validate_registry(&aligned_sqlite, POSTGRES_SCHEMA_SHAPE)
        .expect_err("resolving a declared divergence must fail until the declaration is removed");
    assert!(
        failure.contains("column `relation_kind` is declared divergent"),
        "unexpected stale-declaration failure: {failure}"
    );
}

#[test]
fn schema_congruence_divergent_row_rejects_an_unrecorded_difference() {
    let fabricated_sqlite = SQLITE_SCHEMA_SOURCE.replacen(
        "    blob_ref     TEXT NOT NULL,\n    PRIMARY KEY (namespace, artifact_ref)",
        "    blob_ref     TEXT NOT NULL,\n    unrecorded_column TEXT,\n    PRIMARY KEY (namespace, artifact_ref)",
        1,
    );
    assert_ne!(fabricated_sqlite, SQLITE_SCHEMA_SOURCE);

    let failure = validate_registry(&fabricated_sqlite, POSTGRES_SCHEMA_SHAPE)
        .expect_err("a Divergent row must reject an unrecorded column difference");
    assert!(
        failure.contains(
            "sqlite_table=Some(\"artifact_refs\"), \
             postgres_table=Some(\"lash_lashlang_artifacts\") has divergence beyond its \
             declaration"
        ),
        "unexpected unrecorded-divergence failure: {failure}"
    );
    assert!(failure.contains("unrecorded_column"));
}
