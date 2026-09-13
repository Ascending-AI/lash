//! The explicit schema-migration catalog.
//!
//! Split out of `schema.rs` so the catalog can grow without the module it came
//! from crossing the production file-size budget.

use super::*;

/// Explicit, creation-only migrations retained for the current component.
///
/// These rows are an admission catalog, not a history of shipped edges.
/// Destructive cutovers remove obsolete admissions: component 87 cannot be
/// migrated through typed cancellation at component 88, so the only admitted
/// predecessor is component 88 itself.
///
/// The version-bump recreation harness
/// (`runbooks/restate-postgres-workers/src/bin/version_bump.rs`) pins its
/// fixtures to this table's generation. `scripts/check_version_bump_fixtures.py`
/// recomputes those fixtures from this table and fails when they drift.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Component 88 is the sole admissible source after the cancellation cutover.
    SchemaMigration {
        from: 88,
        to: 89,
        source_missing_tables: &[
            "lash_artifact_owners",
            "lash_artifact_owner_retirements",
            "lash_process_artifact_cleanup",
        ],
        source_missing_columns: &[(
            "lash_effect_scope_retirements",
            "artifact_cleanup_completed",
        )],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_artifact_owners",
            "idx_lash_artifact_owners_owner",
            "lash_artifact_owner_retirements",
            "lash_process_artifact_cleanup",
        ],
        statements: &[],
    },
];
