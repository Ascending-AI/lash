//! The bytes every converted SQLite statement renders to, pinned.
//!
//! The effect and wait families were converted by FIG-3380 against a
//! `Dialect` that carried one schema qualifier for a whole statement. FIG-3406
//! replaced that with a per-table layout, and the whole claim of that change is
//! that **nothing these families issue moved a byte**. A claim like that is
//! worth exactly as much as the pin behind it, so this file holds the rendered
//! text of every statement in both families, for every schema a SQLite
//! connection addresses them through, and compares character for character.
//!
//! It is a pin, not a specification: when a statement's text is *meant* to
//! change, regenerate it in the same commit as the change and read the diff.
//!
//! ```text
//! LASH_UPDATE_RENDERED_SQL_PIN=1 kiln run //crates/lash-sqlite-store:lash-sqlite-store__unit_test -- \
//!     every_converted_statement_keeps_its_rendered_bytes
//! ```

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_store_sql::Statement;

use crate::schema_layout::Schema;

/// Every statement set the effect and wait families declare on this backend,
/// shared and SQLite-only, in a fixed order.
///
/// The sets are walked through their `NEUTRAL` inventories rather than field
/// by field, so a statement added to any of them joins the pin automatically
/// instead of being silently unpinned.
fn statement_sets() -> Vec<(&'static str, &'static [Statement])> {
    vec![
        (
            "effect::EffectJournalStatements",
            lash_store_sql::effect::EffectJournalStatements::NEUTRAL,
        ),
        (
            "effect::replay::ReplayStatements",
            lash_store_sql::effect::replay::ReplayStatements::NEUTRAL,
        ),
        (
            "effect::group::GroupStatements",
            lash_store_sql::effect::group::GroupStatements::NEUTRAL,
        ),
        (
            "effect::scope_retirement::ScopeRetirementStatements",
            lash_store_sql::effect::scope_retirement::ScopeRetirementStatements::NEUTRAL,
        ),
        (
            "wait::waits::WaitStatements",
            lash_store_sql::wait::waits::WaitStatements::NEUTRAL,
        ),
        (
            "wait::revoked_sessions::RevokedSessionStatements",
            lash_store_sql::wait::revoked_sessions::RevokedSessionStatements::NEUTRAL,
        ),
        (
            "sqlite::EffectJournalSqliteStatements",
            crate::effect_replay::EffectJournalSqliteStatements::NEUTRAL,
        ),
        (
            "sqlite::ReplaySqliteStatements",
            crate::effect_replay::ReplaySqliteStatements::NEUTRAL,
        ),
        (
            "sqlite::GroupSqliteStatements",
            crate::effect_replay::GroupSqliteStatements::NEUTRAL,
        ),
        (
            "sqlite::ScopeRetirementSqliteStatements",
            crate::scope_fence::ScopeRetirementSqliteStatements::NEUTRAL,
        ),
        (
            "sqlite::WaitSqliteStatements",
            crate::await_event::WaitSqliteStatements::NEUTRAL,
        ),
        (
            "sqlite::MetaSqliteStatements",
            crate::await_event::MetaSqliteStatements::NEUTRAL,
        ),
    ]
}

fn rendered_pin() -> String {
    let mut out = String::new();
    for schema in Schema::ALL {
        let dialect = schema.dialect();
        for (set, statements) in statement_sets() {
            for statement in statements {
                let rendered = statement.render(dialect).unwrap_or_else(|error| {
                    panic!("`{}` does not render: {error}", statement.name())
                });
                out.push_str("=== ");
                out.push_str(schema.qualifier());
                out.push_str(" | ");
                out.push_str(set);
                out.push_str(" | ");
                out.push_str(statement.name());
                out.push('\n');
                out.push_str(rendered.sql());
                out.push('\n');
            }
        }
    }
    out
}

const PIN: &str = include_str!("rendered_sql_pin.txt");

#[test]
fn every_converted_statement_keeps_its_rendered_bytes() {
    let rendered = rendered_pin();
    if std::env::var_os("LASH_UPDATE_RENDERED_SQL_PIN").is_some() {
        let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY");
        assert!(
            workspace.is_some() || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).is_absolute(),
            "Bazel regeneration requires BUILD_WORKSPACE_DIRECTORY"
        );
        let manifest_dir = workspace.map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            |root| std::path::PathBuf::from(root).join("crates/lash-sqlite-store"),
        );
        std::fs::write(manifest_dir.join("src/rendered_sql_pin.txt"), &rendered)
            .expect("write the rendered-SQL pin");
        return;
    }
    assert_eq!(
        rendered, PIN,
        "a converted statement's rendered bytes moved. If that is intended, \
         regenerate with LASH_UPDATE_RENDERED_SQL_PIN=1 and read the diff; a \
         statement whose text a planner was measured against (an INDEXED BY \
         plan, a partial index predicate) may not move by accident."
    );
}
