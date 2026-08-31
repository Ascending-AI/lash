use super::*;

#[test]
fn current_destructive_cutover_has_no_migration_arm() {
    assert!(
        SCHEMA_MIGRATIONS
            .iter()
            .all(|migration| migration.to != SCHEMA_VERSION),
        "component 67 must reject every pre-CHECK schema rather than migrate it"
    );

    let declared = SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 64 && migration.to == 65)
        .expect("the historical component 64 to 65 creation-only migration must remain declared");

    assert_eq!(
        declared.introduced_relations,
        &["idx_lash_processes_updated"]
    );
    assert_eq!(
        declared.statements,
        &[PROCESS_UPDATED_INDEX_DDL],
        "the historical migration must still create the bounded-poll index"
    );
}

#[test]
fn component_63_remains_a_recreate_boundary_at_the_blake3_cutover() {
    let declared = SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 63)
        .expect("component 63 must remain visible to the refusal gate");

    assert!(
        declared.is_recreate_boundary(),
        "component 63 must not migrate SHA-256 identities into component 65"
    );
}

#[test]
fn component_61_is_a_recreate_boundary_without_its_divergence_witness() {
    let declared = SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 61)
        .expect("component 61 must remain visible to the refusal gate");
    let witnessless = SchemaMigration {
        from: declared.from,
        to: declared.to,
        source_missing_tables: declared.source_missing_tables,
        source_missing_columns: declared.source_missing_columns,
        source_missing_guards: declared.source_missing_guards,
        introduced_relations: &[],
        statements: declared.statements,
    };

    assert!(
        witnessless.is_recreate_boundary(),
        "component 61 must be refused before source-shape matching or migration DDL"
    );
}

/// The declared 53 -> 65 migration, which every case below perturbs.
fn migration() -> &'static SchemaMigration {
    SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 53)
        .expect("the component-53 migration is declared")
}

fn column(name: &str, nullable: bool, value_source: ColumnValueSource) -> ColumnShape {
    ColumnShape {
        name: name.to_string(),
        sql_type: "text".to_string(),
        nullable,
        value_source,
    }
}

fn guard(primary_key: bool, predicate: Option<&str>, nulls_not_distinct: bool) -> UniqueGuard {
    UniqueGuard {
        primary_key,
        columns: vec!["group_key".to_string(), "settlement_seq".to_string()],
        predicate: predicate.map(str::to_string),
        nulls_not_distinct,
    }
}

/// The exact partial guard the 54 generation adds, as the shape checker
/// renders it.
fn declared_guard() -> UniqueGuard {
    guard(
        false,
        Some("(group_key is not null) and (settlement_seq is not null)"),
        false,
    )
}

fn report(mut findings: Vec<SchemaFinding>) -> SchemaReport {
    findings.push(SchemaFinding::MissingTable {
        table: "lash_session_meta_pending_observer_intents".to_string(),
    });
    findings.push(SchemaFinding::UnexpectedColumn {
        table: "lash_session_meta".to_string(),
        found: ColumnShape {
            name: "observer_intent_depth".to_string(),
            sql_type: "bigint".to_string(),
            nullable: false,
            value_source: ColumnValueSource::Supplied,
        },
    });
    SchemaReport {
        schema: Some("public".to_string()),
        expected_version: 65,
        found_version: Some(53),
        findings,
    }
}

/// The full set of findings a genuine published component-53 database
/// produces against this build, which the migration must accept.
fn published_53_findings() -> Vec<SchemaFinding> {
    vec![
        SchemaFinding::VersionMismatch {
            expected: 65,
            found: Some(53),
        },
        SchemaFinding::UnexpectedColumn {
            table: "lash_runtime_turn_commits".to_string(),
            found: column(
                "requested_ancestor_node_id",
                true,
                ColumnValueSource::Supplied,
            ),
        },
        SchemaFinding::MissingTable {
            table: "lash_runtime_effect_group".to_string(),
        },
        SchemaFinding::MissingTable {
            table: "lash_checkpoint_blob_refs".to_string(),
        },
        SchemaFinding::MissingTable {
            table: "lash_turn_cancel_requests".to_string(),
        },
        SchemaFinding::MissingColumn {
            table: "lash_session_meta".to_string(),
            expected: ColumnShape {
                name: "session_state_version".to_string(),
                sql_type: "integer".to_string(),
                nullable: true,
                value_source: ColumnValueSource::Supplied,
            },
        },
        SchemaFinding::MissingColumn {
            table: "lash_runtime_effect_replay".to_string(),
            expected: column("group_key", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_runtime_effect_replay".to_string(),
            expected: column("settlement_seq", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_trigger_occurrences".to_string(),
            expected: ColumnShape {
                name: "reclaimable_at_ms".to_string(),
                sql_type: "bigint".to_string(),
                nullable: true,
                value_source: ColumnValueSource::Supplied,
            },
        },
        SchemaFinding::MissingUniqueGuard {
            table: "lash_runtime_effect_replay".to_string(),
            expected: declared_guard(),
        },
        SchemaFinding::MissingColumn {
            table: "lash_session_meta".to_string(),
            expected: column("created_at_ms", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_session_meta".to_string(),
            expected: column("last_commit_at_ms", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_deleted_sessions".to_string(),
            expected: column("created_at_ms", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_deleted_sessions".to_string(),
            expected: column("last_commit_at_ms", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_deleted_sessions".to_string(),
            expected: column("head_revision", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_deleted_sessions".to_string(),
            expected: column("relation_kind", true, ColumnValueSource::Supplied),
        },
        SchemaFinding::MissingColumn {
            table: "lash_deleted_sessions".to_string(),
            expected: column("parent_session_id", true, ColumnValueSource::Supplied),
        },
    ]
}

#[test]
fn the_published_predecessor_shape_is_accepted() {
    assert!(
        migration().matches_source_shape(&report(published_53_findings())),
        "the shape the migration exists for must pass its own preflight"
    );
}

/// A declaration names a column; it does not license every column shape that
/// could wear the name. `NOT NULL` and a value source each make the `ALTER`
/// write a value into every existing row — a full table rewrite under lock,
/// which is the one thing the creation-only class promises never happens.
#[test]
fn a_column_that_would_rewrite_every_row_is_refused_by_the_creation_only_door() {
    for (label, expected) in [
        (
            "NOT NULL",
            column("group_key", false, ColumnValueSource::Supplied),
        ),
        (
            "a default",
            column("group_key", true, ColumnValueSource::Default),
        ),
        (
            "an identity",
            column("group_key", true, ColumnValueSource::IdentityByDefault),
        ),
        (
            "a generated value",
            column("group_key", true, ColumnValueSource::Generated),
        ),
    ] {
        let mut findings = published_53_findings();
        findings[5] = SchemaFinding::MissingColumn {
            table: "lash_runtime_effect_replay".to_string(),
            expected,
        };
        assert!(
            !migration().matches_source_shape(&report(findings)),
            "a missing column with {label} must not pass the creation-only door"
        );
    }
}

/// A declared partial guard is permission for that guard alone. A missing
/// `PRIMARY KEY` or full `UNIQUE` over the same columns guards strictly more
/// rows, so tolerating it would migrate a database that is genuinely drifted
/// — and silently drop a uniqueness guarantee lash depends on.
#[test]
fn a_stronger_missing_guard_over_the_same_columns_is_refused() {
    for (label, expected) in [
        ("a primary key", guard(true, None, false)),
        ("a full unique guard", guard(false, None, false)),
        (
            "a differently-predicated guard",
            guard(false, Some("(group_key is not null)"), false),
        ),
        (
            "a NULLS NOT DISTINCT rebuild",
            guard(
                false,
                Some("(group_key is not null) and (settlement_seq is not null)"),
                true,
            ),
        ),
    ] {
        let mut findings = published_53_findings();
        findings[8] = SchemaFinding::MissingUniqueGuard {
            table: "lash_runtime_effect_replay".to_string(),
            expected,
        };
        assert!(
            !migration().matches_source_shape(&report(findings)),
            "{label} must not be consumed by the declaration for the partial guard"
        );
    }
}

/// The declaration is per table, not per column set: the same key columns on
/// a table the migration says nothing about is drift.
#[test]
fn a_declared_guard_does_not_travel_to_another_table() {
    let mut findings = published_53_findings();
    findings[8] = SchemaFinding::MissingUniqueGuard {
        table: "lash_queued_work_batches".to_string(),
        expected: declared_guard(),
    };
    assert!(!migration().matches_source_shape(&report(findings)));
}
