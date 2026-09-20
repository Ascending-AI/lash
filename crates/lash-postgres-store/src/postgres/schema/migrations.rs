//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// Post-cutover the table is empty: the component-102 window closed with no
/// migration out of any earlier stamp, so a store provisioned before the
/// cutover has no applicable migration and is refused at open with the
/// reject-and-recreate remedy. The migration framework itself is retained
/// (FIG-1665's migrate-at-admission doctrine): a future additive component
/// that can be upgraded in place declares its entry here, and the
/// version-bump fixture checker derives its pinned artifact lists from
/// whatever the table then holds.
///
/// When entries return, keep the outer list expanded — one `SchemaMigration`
/// per row — so the source-derived fixture checker can split them.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[];
