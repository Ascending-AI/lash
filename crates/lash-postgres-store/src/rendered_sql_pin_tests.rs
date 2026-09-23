//! The bytes every converted PostgreSQL statement renders to, pinned.
//!
//! The SQLite half of this pin (`crates/lash-sqlite-store/src/rendered_sql_pin_tests.rs`)
//! is the one FIG-3406's per-table schema layout could move. This half exists
//! because a render-axis change is a change to one tokenizer both backends
//! share: PostgreSQL carries no schema qualifier at all, so "nothing moved"
//! here is exactly the claim worth pinning cheaply.
//!
//! ```text
//! LASH_UPDATE_RENDERED_SQL_PIN=1 kiln run //crates/lash-postgres-store:lash-postgres-store__unit_test -- \
//!     every_converted_statement_keeps_its_rendered_bytes
//! ```

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_store_sql::{Dialect, Statement};

/// Every statement set the effect and wait families declare on this backend,
/// shared and PostgreSQL-only, walked through their `NEUTRAL` inventories so a
/// new statement joins the pin rather than escaping it.
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
            "postgres::EffectJournalPostgresStatements",
            crate::effect_replay::EffectJournalPostgresStatements::NEUTRAL,
        ),
        (
            "postgres::ReplayPostgresStatements",
            crate::effect_replay::ReplayPostgresStatements::NEUTRAL,
        ),
        (
            "postgres::GroupPostgresStatements",
            crate::effect_replay::GroupPostgresStatements::NEUTRAL,
        ),
        (
            "postgres::ScopeRetirementPostgresStatements",
            crate::effect_replay::ScopeRetirementPostgresStatements::NEUTRAL,
        ),
        (
            "postgres::WaitPostgresStatements",
            crate::await_event::WaitPostgresStatements::NEUTRAL,
        ),
        (
            "postgres::MetaPostgresStatements",
            crate::await_event::MetaPostgresStatements::NEUTRAL,
        ),
    ]
}

fn rendered_pin() -> String {
    let mut out = String::new();
    for (set, statements) in statement_sets() {
        for statement in statements {
            let rendered = statement
                .render(Dialect::postgres())
                .unwrap_or_else(|error| panic!("`{}` does not render: {error}", statement.name()));
            out.push_str("=== ");
            out.push_str(set);
            out.push_str(" | ");
            out.push_str(statement.name());
            out.push('\n');
            out.push_str(rendered.sql());
            out.push('\n');
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
            |root| std::path::PathBuf::from(root).join("crates/lash-postgres-store"),
        );
        std::fs::write(manifest_dir.join("src/rendered_sql_pin.txt"), &rendered)
            .expect("write the rendered-SQL pin");
        return;
    }
    assert_eq!(
        rendered, PIN,
        "a converted statement's rendered bytes moved. If that is intended, \
         regenerate with LASH_UPDATE_RENDERED_SQL_PIN=1 and read the diff."
    );
}
