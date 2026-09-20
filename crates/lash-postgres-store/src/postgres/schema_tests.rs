use super::*;

// Historical test declarations are frozen independently of the active catalog.
const RETAINED_MIGRATION_ENDPOINT: i32 = 87;

#[test]
fn current_destructive_cutover_has_no_migration_arm() {
    // Post-cutover the catalog is empty: no stamp below SCHEMA_VERSION has an
    // applicable migration, so every pre-cutover database fails at open with
    // the reject-and-recreate refusal rather than upgrading.
    assert!(
        SCHEMA_MIGRATIONS.is_empty(),
        "the post-cutover catalog must offer no migration out of any earlier stamp"
    );

    let immediate = HISTORICAL_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 87 && migration.to == 88)
        .expect("the prior component's immediate predecessor refusal must remain declared");
    assert!(immediate.is_recreate_boundary());

    let predecessor = HISTORICAL_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 68 && migration.to == RETAINED_MIGRATION_ENDPOINT)
        .expect("the historical component 68 refusal boundary must remain declared");
    assert!(
        predecessor.is_recreate_boundary(),
        "component 68 must not migrate into the retained predecessor"
    );

    let declared = HISTORICAL_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 64 && migration.to == RETAINED_MIGRATION_ENDPOINT)
        .expect("the historical component 64 creation-only migration must remain declared");

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
    let declared = HISTORICAL_MIGRATIONS
        .iter()
        .find(|migration| migration.from == 63)
        .expect("component 63 must remain visible to the refusal gate");

    assert!(
        declared.is_recreate_boundary(),
        "component 63 must not migrate SHA-256 identities into the retained predecessor"
    );
}

#[test]
fn component_61_is_a_recreate_boundary_without_its_divergence_witness() {
    let declared = HISTORICAL_MIGRATIONS
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

/// The declared component-53 migration into the retained predecessor, which
/// every case below perturbs.
fn migration() -> &'static SchemaMigration {
    HISTORICAL_MIGRATIONS
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
        expected_version: RETAINED_MIGRATION_ENDPOINT,
        found_version: Some(53),
        findings,
    }
}

/// The full set of findings a genuine published component-53 database
/// produces against this build, which the migration must accept.
fn published_53_findings() -> Vec<SchemaFinding> {
    vec![
        SchemaFinding::VersionMismatch {
            expected: RETAINED_MIGRATION_ENDPOINT,
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

/// The refusal text is derived, not frozen: it must name the direction of the
/// mismatch and the live migration catalog, and must never carry the retired
/// doc-site pointer or a hard-coded historical cutover paragraph (FIG-3172,
/// FIG-3173).
#[test]
fn version_mismatch_refusal_derives_direction_and_catalog() {
    let older = version_mismatch_error(Some(SCHEMA_VERSION - 1), None).to_string();
    let newer = version_mismatch_error(Some(SCHEMA_VERSION + 1), None).to_string();
    let unstamped = version_mismatch_error(None, None).to_string();

    assert!(
        older.contains("provisioned by an older build"),
        "an older stamp must be named as such: {older}"
    );
    assert!(
        older.contains(&forward_migration_sentence(SCHEMA_VERSION - 1)),
        "an older stamp must carry the live catalog's own verdict: {older}"
    );
    assert!(
        newer.contains("provisioned by a newer build")
            && newer.contains("never migrates a schema backwards"),
        "a newer stamp must be refused as a downgrade, not as a missing migration: {newer}"
    );
    assert!(
        !newer.contains("upgrade path") && !newer.contains("forward migration"),
        "the newer-store direction must not borrow the older-store explanation: {newer}"
    );
    assert!(
        unstamped.contains("no version stamp") && unstamped.contains("lash_schema_versions"),
        "an unstamped database must be told which row is missing: {unstamped}"
    );

    for message in [&older, &newer, &unstamped] {
        assert!(
            message.contains("has no applicable migration"),
            "the version-bump companion classifies this refusal by that phrase: {message}"
        );
        assert!(
            !message.contains("persistence.html"),
            "the retired doc site must not be offered as a remedy: {message}"
        );
        assert!(
            !message.contains("component-50") && !message.contains("append-identity"),
            "the remedy must not restate a cutover the live catalog left behind: {message}"
        );
        assert!(
            message.contains("crates/lash-postgres-store/schema.sql")
                && message.contains("await-event revocation ledger")
                && message.contains("0081-destructive-schema-changes"),
            "the remedy must state the recreate procedure inline: {message}"
        );
    }
}

/// The version-bump companion classifies a refusal by a marker only one error
/// carries, and counts anything but exactly one match as a failure. The shared
/// recreate remedy must therefore not carry the version-mismatch marker into the
/// migration refusals (FIG-3172).
#[test]
fn only_the_version_mismatch_refusal_claims_no_applicable_migration() {
    let divergence = schema_migration_divergence_error(
        SCHEMA_VERSION - 1,
        &["public.lash_sessions".to_string()],
    )
    .to_string();
    assert!(
        !divergence.contains("has no applicable migration"),
        "the divergence refusal must stay distinguishable from the version refusal: {divergence}"
    );
    assert!(
        divergence.contains("schema artifacts newer than the recorded version"),
        "the divergence refusal must keep its own marker: {divergence}"
    );
}
