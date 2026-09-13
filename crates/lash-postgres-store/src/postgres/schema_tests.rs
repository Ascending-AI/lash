use super::*;

#[test]
fn current_catalog_refuses_every_pre_cancellation_component() {
    assert_eq!(
        SCHEMA_MIGRATIONS.len(),
        1,
        "only the immediate predecessor is admitted"
    );
    let predecessor = &SCHEMA_MIGRATIONS[0];
    assert_eq!(predecessor.from, 88);
    assert_eq!(predecessor.to, SCHEMA_VERSION);
    assert!(
        predecessor.is_recreate_boundary(),
        "component 88 must be refused before FIG-677 ownership rows are read"
    );
    assert_eq!(
        predecessor.introduced_relations,
        &[
            "lash_artifact_owners",
            "idx_lash_artifact_owners_owner",
            "lash_artifact_owner_retirements",
            "lash_process_artifact_cleanup",
        ],
        "the component-88 divergence must name every FIG-677 relation witness"
    );
}

const TEST_GUARDS: &[DeclaredGuard] = &[DeclaredGuard {
    table: "lash_runtime_effect_replay",
    columns: &["group_key", "settlement_seq"],
    predicate: "(group_key is not null) and (settlement_seq is not null)",
}];

const TEST_MIGRATION: SchemaMigration = SchemaMigration {
    from: 88,
    to: 89,
    source_missing_tables: &["lash_new_relation"],
    source_missing_columns: &[("lash_runtime_effect_replay", "group_key")],
    source_missing_guards: TEST_GUARDS,
    introduced_relations: &["lash_new_relation"],
    statements: &["SELECT 1"],
};

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

fn declared_guard() -> UniqueGuard {
    guard(
        false,
        Some("(group_key is not null) and (settlement_seq is not null)"),
        false,
    )
}

fn report(column_shape: ColumnShape, unique_guard: UniqueGuard) -> SchemaReport {
    SchemaReport {
        schema: Some("public".to_string()),
        expected_version: 89,
        found_version: Some(88),
        findings: vec![
            SchemaFinding::VersionMismatch {
                expected: 89,
                found: Some(88),
            },
            SchemaFinding::MissingTable {
                table: "lash_new_relation".to_string(),
            },
            SchemaFinding::MissingColumn {
                table: "lash_runtime_effect_replay".to_string(),
                expected: column_shape,
            },
            SchemaFinding::MissingUniqueGuard {
                table: "lash_runtime_effect_replay".to_string(),
                expected: unique_guard,
            },
        ],
    }
}

#[test]
fn exact_creation_only_source_shape_is_accepted() {
    assert!(TEST_MIGRATION.matches_source_shape(&report(
        column("group_key", true, ColumnValueSource::Supplied),
        declared_guard(),
    )));
}

#[test]
fn a_column_that_would_rewrite_every_row_is_refused() {
    for (label, shape) in [
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
        assert!(
            !TEST_MIGRATION.matches_source_shape(&report(shape, declared_guard())),
            "a missing column with {label} must not pass the creation-only door"
        );
    }
}

#[test]
fn a_stronger_or_different_missing_guard_is_refused() {
    for (label, unique_guard) in [
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
        assert!(
            !TEST_MIGRATION.matches_source_shape(&report(
                column("group_key", true, ColumnValueSource::Supplied),
                unique_guard,
            )),
            "{label} must not be consumed by the declared partial guard"
        );
    }
}

#[test]
fn a_declared_guard_does_not_travel_to_another_table() {
    let mut source = report(
        column("group_key", true, ColumnValueSource::Supplied),
        declared_guard(),
    );
    source.findings[3] = SchemaFinding::MissingUniqueGuard {
        table: "lash_queued_work_batches".to_string(),
        expected: declared_guard(),
    };
    assert!(!TEST_MIGRATION.matches_source_shape(&source));
}
