//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the source contract and provider route every trigger
/// subscription now captures (component 95), and none can be upgraded by
/// turning a journaled sleep's resolved duration back into the deadline the
/// guest asked for (component 96). Component 95 is therefore retained as the
/// refusal-only endpoint and no row targets component 96.
///
/// Neither generation installs a relation: component 95's capture lives inside
/// the trigger subscription record document, and component 96 moves only the
/// journaled effect-command encoding. That is precisely why both cutovers are
/// refusals rather than creation migrations — the missing fact is data, and no
/// DDL can invent it.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 94,
        to: 95,
        // Component 95 installs no relation and no column: what a component-94
        // store lacks is the source contract and provider route inside each
        // trigger subscription record document. The lists are keyed to the
        // floor, not to one generation, so a relation or column introduced
        // after 95 belongs here too.
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
];
