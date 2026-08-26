use std::collections::BTreeSet;

const SQLITE_SCHEMA_SOURCE: &str = include_str!("../../lash-sqlite-store/src/schema.rs");
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
    pair("attachment_condemnations", "lash_attachment_condemnations"),
    pair("attachment_manifest", "lash_attachment_manifest"),
    pair("await_event_meta", "lash_await_event_meta"),
    pair(
        "await_event_revoked_sessions",
        "lash_await_event_revoked_sessions",
    ),
    pair("await_event_waits", "lash_await_event_waits"),
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
    pair("process_events", "lash_process_events"),
    pair("process_leases", "lash_process_leases"),
    pair("process_observers", "lash_process_observers"),
    pair("process_parent_end_plans", "lash_process_parent_end_plans"),
    pair(
        "process_segment_handovers",
        "lash_process_segment_handovers",
    ),
    pair("process_tombstones", "lash_process_tombstones"),
    pair("process_wake_deliveries", "lash_process_wake_deliveries"),
    pair("processes", "lash_processes"),
    pair("queued_work_batches", "lash_queued_work_batches"),
    pair("queued_work_items", "lash_queued_work_items"),
    pair("runtime_effect_group", "lash_runtime_effect_group"),
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
    pair("session_meta", "lash_session_meta"),
    pair(
        "session_meta_fork_inheritance_processes",
        "lash_session_meta_fork_inheritance_processes",
    ),
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
    pair("trigger_occurrences", "lash_trigger_occurrences"),
    pair("trigger_subscriptions", "lash_trigger_subscriptions"),
    TablePair {
        sqlite_table: Some("turn_cancel_requests"),
        postgres_table: Some("lash_turn_cancel_requests"),
        parity: Parity::Divergent {
            reason: "SQLite stores the typed cancel record as one JSON value while Postgres keeps request fields and ordered outcome arrays structural",
            sqlite_only_columns: &["record_json"],
            postgres_only_columns: &[
                "affected_dispositions",
                "affected_input_ids",
                "disposition",
                "origin",
                "reason",
                "request_id",
            ],
        },
    },
    TablePair {
        sqlite_table: Some("usage_deltas"),
        postgres_table: Some("lash_usage_deltas"),
        parity: Parity::Divergent {
            reason: "TokenLedgerEntry identity is pinned by payload_encoding_version and \
                     payload_hash columns with a permanent tag registry",
            sqlite_only_columns: &[
                "cache_read_input_tokens",
                "cache_write_input_tokens",
                "input_tokens",
                "model",
                "output_tokens",
                "reasoning_output_tokens",
                "source",
            ],
            postgres_only_columns: &["entry_json"],
        },
    },
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
    let declaration = format!("table {table}\n");
    source
        .split_once(&declaration)
        .unwrap_or_else(|| panic!("Postgres schema shape is missing registered table `{table}`"))
        .1
        .lines()
        .take_while(|line| !line.starts_with("table "))
        .filter_map(|line| {
            line.strip_prefix("  column ")
                .and_then(|line| line.split_whitespace().next())
                .map(str::to_string)
        })
        .collect()
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

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn schema_congruence_registry_matches_both_backends() {
    if let Err(failures) = validate_registry(SQLITE_SCHEMA_SOURCE, POSTGRES_SCHEMA_SHAPE) {
        panic!("cross-backend schema registry validation failed:\n{failures}");
    }
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
fn pending_observer_intent_attribution_check_is_registered_on_both_backends() {
    fn normalized(source: &str) -> String {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    let expected =
        "attribution TEXT NOT NULL CHECK (attribution IN ('host_requested', 'fork_inherited'))";
    for (backend, source) in [
        ("SQLite", SQLITE_SCHEMA_SOURCE),
        ("Postgres", POSTGRES_SCHEMA_SOURCE),
    ] {
        assert!(
            normalized(source).contains(expected),
            "{backend} pending-observer-intent attribution CHECK drifted from the registered contract"
        );
    }
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
