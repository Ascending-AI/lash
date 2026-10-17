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
