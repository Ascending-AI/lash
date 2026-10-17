//! The checked-in plans and manifest are what the helper compiler gives
//! for the current helper bodies: a body edited without regenerating them
//! fails here, as drifted generated BUCK files fail the build.

use lash_ext_helper_compiler::library::Dialect;

#[test]
fn plans_are_what_the_compiler_gives_for_the_current_bodies() {
    let generated =
        lash_ext_helper_compiler::generate(Dialect::TypeScript).expect("the helpers compile");
    let stale = "is stale: run `kiln run //crates/lash-ext-helper-compiler:regenerate__bin`";
    assert!(
        generated.rust == include_str!("../src/generated/plans.rs"),
        "src/generated/plans.rs {stale}"
    );
    assert!(
        generated.manifest == include_str!("../src/generated/plans.tsv"),
        "src/generated/plans.tsv {stale}"
    );
}

/// FIG-6715: closed try bodies, declining callees and closure factories
/// are compiled, rather than silently left interpreted.
#[test]
fn closed_helpers_have_generated_plans() {
    let generated =
        lash_ext_helper_compiler::generate(Dialect::TypeScript).expect("the helpers compile");
    for name in [
        "ts.number.parse_int",
        "ts.number.parse_decimal",
        "ts.number.parse",
        "ts.primitive_to_number",
        "ts.string.slice_units",
        "ts.regexp.native_exec",
        "ts.regexp.native_match_all",
        "ts.regexp.native_replace",
        "ts.regexp.native_split",
        "ts.string.substitute",
        "ts.uri.flush",
        "ts.uri.decode",
        "ts.array.keys",
        "ts.array.values",
        "ts.array.entries",
    ] {
        let row = generated
            .manifest
            .lines()
            .find(|line| line.starts_with(&format!("{name}\t")))
            .expect("the helper is admitted");
        assert!(row.ends_with("\tgenerated"), "{row}");
    }
    for row in generated.manifest.lines().skip(2) {
        if row.split('\t').nth(2) == Some("closed") {
            assert!(row.ends_with("\tgenerated"), "{row}");
        }
    }
}
