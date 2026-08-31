//! A cell reads what an earlier cell bound.
//!
//! The RLM session model is that top-level bindings persist across cells: the
//! prompt lists them under `=== BOUND VARIABLES ===` with their values, and the
//! Lashlang dialect resolves them at *link*, where the live session globals are
//! known. The TypeScript lowerer resolved every name against source-local
//! scopes at parse instead, so `finish(findings)` in a second cell rejected
//! with `TS_UNKNOWN_BINDING` while the same turn's prompt showed `findings` and
//! its value. Every crate test missed it because they pre-supply bindings in
//! the same source they compile.
//!
//! The rule this pins: a name the *session* has is known; a name nobody has is
//! still `TS_UNKNOWN_BINDING`, and it must still be reported at parse rather
//! than degrading into a link error nobody can read.

use std::collections::BTreeSet;

fn environment(globals: [&str; 1]) -> lashlang::LashlangHostEnvironment {
    lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::default(),
    )
    .with_globals(globals)
}

#[test]
fn a_live_session_global_is_readable_from_a_later_cell() {
    let environment = environment(["findings"]);
    lash_typescript::link("finish(findings);", &environment)
        .expect("a session global must be readable");
}

#[test]
fn a_live_session_global_can_be_read_shaped_and_rebound() {
    let environment = environment(["findings"]);
    // The three shapes a second cell actually uses: read a field, pass it to a
    // call, and shadow it with a fresh root binding of the same name (which is
    // how a cell rebinds a session global).
    for source in [
        "const summary = findings.summary;\nfinish(summary);",
        "finish(JSON.stringify(findings));",
        "const findings = { summary: \"new\" };\nfinish(findings);",
    ] {
        lash_typescript::link(source, &environment)
            .unwrap_or_else(|error| panic!("`{source}` must link: {error}"));
    }
}

#[test]
fn a_restored_process_handle_is_awaitable_from_a_later_cell() {
    let process_environment = environment(["handle"]).with_process_handles(["handle"]);
    lash_typescript::link("finish(await handle);", &process_environment)
        .expect("a restored live process handle must remain awaitable");

    let ordinary_environment = environment(["handle"]);
    let error = lash_typescript::link("finish(await handle);", &ordinary_environment)
        .expect_err("an ordinary ambient value must not become awaitable");
    assert!(
        error.to_string().contains("TS_AWAIT_UNSUPPORTED"),
        "{error}"
    );
}

#[test]
fn a_name_no_one_has_is_still_rejected_at_parse() {
    let environment = environment(["findings"]);
    let error = lash_typescript::link("finish(nowhere);", &environment)
        .expect_err("an unknown name must still reject");
    let rendered = error.to_string();
    assert!(
        rendered.contains("TS_UNKNOWN_BINDING"),
        "the diagnostic must stay the parse-stage unknown-binding one: {rendered}"
    );
    assert!(rendered.contains("nowhere"), "{rendered}");
}

#[test]
fn a_session_global_does_not_disable_the_scope_rules_around_it() {
    let environment = environment(["findings"]);
    // Ambient names are const: assigning to one without declaring it is still
    // refused, and the temporal-dead-zone and duplicate-binding analyses are
    // unchanged for source-local names.
    assert!(lash_typescript::link("findings = 1;\nfinish(1);", &environment).is_err());
    assert!(lash_typescript::link("const a = b;\nconst b = 1;\nfinish(a);", &environment).is_err());
    assert!(lash_typescript::link("const c = 1;\nconst c = 2;\nfinish(c);", &environment).is_err());
}

#[test]
fn parse_alone_still_knows_nothing_about_a_session() {
    // The narrow entry point keeps its meaning: a standalone program is
    // self-contained, which is what every non-RLM caller compiles.
    let mut globals = BTreeSet::new();
    globals.insert("findings".to_string());
    assert!(lash_typescript::parse("finish(findings);").is_err());
    assert!(lash_typescript::parse_with_globals("finish(findings);", &globals).is_ok());
}
