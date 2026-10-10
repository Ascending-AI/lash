//! The laws of the helper releases a build holds, and the generator that
//! seals the release the tree builds.

// FIG-2971: this file is test/tooling code; the generator's ambient fs/env
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::fmt::Write as _;

use super::*;

#[path = "build/release.rs"]
mod release;

fn helper(result: i64) -> lash_kernel_doc::FunctionDefinition {
    lash_kernel_doc::parse_definition(&format!(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
    ))
    .expect("a helper definition")
}

fn helper_registry(result: i64) -> lash_kernel_doc::FunctionRegistry {
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    registry
        .register(helper(result), None)
        .expect("register the helper");
    registry
}

/// FIG-5821: the unsealed 1.0 baseline holds only today's functions;
/// superseded pre-release helpers must not grow every worker's catalog.
#[test]
fn unsealed_helper_baseline_contains_only_current_functions() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let previous = release::freeze("1.0", 1, None, &old, &fingerprints);
    let frozen = release::freeze("1.0", 1, Some(&previous), &current, &fingerprints);
    assert_eq!(frozen.functions.len(), 1, "no superseded unsealed helper");
    assert_eq!(frozen.index().functions, frozen.writes);
    assert!(
        release::keep(&[frozen], &current, &fingerprints)
            .expect("the baseline matches the build")
            .is_empty()
    );
}

/// FIG-5799: a sealed release still protects runnable functions a run pins.
#[test]
fn sealed_helper_freezes_keep_runnable_functions() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let mut previous = release::freeze("1.0", 1, None, &old, &fingerprints);
    previous.seal().expect("seal the shipped release");
    let building = release::freeze("2.0", 2, Some(&previous), &current, &fingerprints);
    assert_eq!(
        building.functions.len(),
        2,
        "the sealed helper remains held"
    );
    assert_eq!(building.writes.len(), 1, "only the current helper is named");
    let kept = release::keep(&[previous, building], &current, &fingerprints)
        .expect("the sealed helper remains runnable");
    assert_eq!(kept.len(), 1);
}

/// V17: a sealed building release cannot silently write changed helpers.
#[test]
fn a_sealed_building_release_rejects_changed_writes() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let mut sealed = release::freeze("1.0", 1, None, &old, &fingerprints);
    sealed.seal().expect("seal the baseline");
    assert!(
        sealed.seal().is_err(),
        "sealing is explicit and happens once"
    );
    assert!(release::keep(&[sealed], &current, &fingerprints).is_err());
}

/// FIG-5839: the build defines the release the tree builds only while no
/// sealed release is kept for it; once sealed, the build holds it as it
/// shipped, never defined again (V17). Every other declared release is a
/// sealed one the repository keeps, and every sealed one is declared, so a
/// declaration and its artifact cannot drift apart (V18).
#[test]
fn only_the_release_the_tree_builds_is_defined_by_the_build() {
    let fingerprints = Default::default();
    let mut shipped = release::freeze("1.0", 1, None, &helper_registry(1), &fingerprints);
    shipped.seal().expect("seal 1.0");
    let shipped_text = serde_json::to_string(&shipped).expect("1.0's JSON");
    let unsealed_text = serde_json::to_string(&release::freeze(
        "1.0",
        1,
        None,
        &helper_registry(1),
        &fingerprints,
    ))
    .expect("an unsealed 1.0's JSON");
    let define = |name: &str, ordinal: u32, previous: Option<&HelperRelease>| {
        release::freeze(name, ordinal, previous, &helper_registry(2), &fingerprints)
    };

    let unsealed = release::resolve(&[("1.0", 1)], &[], &[], define).expect("the build's 1.0");
    assert!(!unsealed.retained[0].sealed, "the build defines 1.0");

    let sealed = release::resolve(
        &[("1.0", 1)],
        &[],
        &[&shipped_text],
        |_, _, _| -> HelperRelease { panic!("a sealed release is never defined again") },
    )
    .expect("the sealed 1.0");
    assert_eq!(sealed.retained, vec![shipped.clone()]);

    let next = release::resolve(&[("1.0", 1), ("2.0", 2)], &[], &[&shipped_text], define)
        .expect("the build's 2.0 after the sealed 1.0");
    assert_eq!(
        next.retained[1].functions.len(),
        2,
        "2.0 keeps 1.0's helper"
    );

    let retiring = release::resolve(&[("2.0", 2)], &[("1.0", 1)], &[&shipped_text], define)
        .expect("1.0 retiring");
    assert_eq!(retiring.retiring, vec![shipped]);

    for (retained, retiring, kept, refusal) in [
        (
            &[("1.0", 1), ("2.0", 2)][..],
            &[][..],
            &[][..],
            "an earlier release the repository does not keep",
        ),
        (
            &[("2.0", 2)],
            &[],
            &[&shipped_text[..]][..],
            "an undeclared sealed release",
        ),
        (
            &[("2.0", 2)],
            &[("1.0", 1)],
            &[],
            "a retiring release the repository does not keep",
        ),
        (
            &[("1.0", 2)],
            &[],
            &[&shipped_text],
            "a sealed release kept under another ordinal",
        ),
        (
            &[("1.0", 1)],
            &[],
            &[&unsealed_text],
            "an unsealed release kept as shipped",
        ),
        (&[], &[], &[], "no retained release"),
    ] {
        assert!(
            release::resolve(retained, retiring, kept, define).is_err(),
            "{refusal} is refused"
        );
    }
}

/// The releases this build holds are the ones it declares, and the release
/// the tree builds writes exactly what the build defines.
#[test]
fn the_build_holds_the_releases_it_declares() {
    let held = held_releases().expect("the build's releases");
    let declared = |releases: &[HelperRelease]| -> Vec<(String, u32)> {
        releases
            .iter()
            .map(|release| (release.release.clone(), release.ordinal))
            .collect()
    };
    let named = |declarations: &[(&str, u32)]| -> Vec<(String, u32)> {
        declarations
            .iter()
            .map(|(name, ordinal)| ((*name).to_owned(), *ordinal))
            .collect()
    };
    assert_eq!(declared(&held.retained), named(RETAINED_HELPER_RELEASES));
    assert_eq!(declared(&held.retiring), named(RETIRING_HELPER_RELEASES));
    let index: Vec<HelperReleaseIndex> =
        serde_json::from_slice(HELPER_RELEASE_INDEX).expect("the index decodes");
    assert_eq!(
        index,
        held.retained
            .iter()
            .map(HelperRelease::index)
            .collect::<Vec<_>>()
    );
    let helpers = lash_kernel_doc::ValidatedFunctions::from_json(TYPESCRIPT_HELPERS)
        .expect("the helpers decode");
    let building = held.retained.last().expect("the release the tree builds");
    for (function, _) in helpers.iter() {
        assert!(
            building.writes.contains(function),
            "the release the tree builds writes the build's helper {function}"
        );
    }
}

/// Seals the release the tree builds, and keeps every sealed release in
/// `src/sealed.rs`: the one act of the cut that ships it (`SEAL`). A build
/// after it holds the release as it shipped, and changes a helper by
/// declaring the next release. Run again, it keeps what is sealed and seals
/// the release the tree builds if it is new; a release no longer declared
/// is dropped.
#[test]
#[ignore = "regenerates crates/lash-vm-releases/src/sealed.rs"]
fn seal_helper_release() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let held = held_releases().expect("the build's releases");
    let mut sealed: Vec<HelperRelease> = held.retiring.into_iter().chain(held.retained).collect();
    sealed.sort_by_key(|release| release.ordinal);
    for release in &mut sealed {
        if !release.sealed {
            release.seal().expect("seal the release the tree builds");
        }
    }
    let workspace =
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the regeneration workspace");
    std::fs::write(
        std::path::Path::new(&workspace).join("crates/lash-vm-releases/src/sealed.rs"),
        sealed_source(&sealed).expect("the sealed releases' source"),
    )
    .expect("the sealed releases are written");
}

/// The source `src/sealed.rs` keeps `releases` in, oldest first: each
/// release's JSON, one function to a line, as a string constant.
fn sealed_source(releases: &[HelperRelease]) -> Result<String, String> {
    let mut text = format!(
        "// @generated by `{SEAL}`; do not edit.\n\
         //! The helper releases that have shipped, oldest first, each as it was\n\
         //! sealed: every function a build of it holds (FIG-5799). Until the 1.0 cut\n\
         //! none has, and the build defines the release the tree builds (FIG-5839).\n\n\
         pub(crate) const SEALED: &[&str] = &["
    );
    for release in releases {
        let mut body = format!(
            "{{\"release\":{},\"ordinal\":{},",
            json(&release.release)?,
            release.ordinal
        );
        if release.sealed {
            body.push_str("\"sealed\":true,");
        }
        let _ = write!(
            body,
            "\"writes\":{},\"functions\":[",
            json(&release.writes)?
        );
        for (index, function) in release.functions.iter().enumerate() {
            body.push_str(if index == 0 { "\n" } else { ",\n" });
            body.push_str(&json(function)?);
        }
        body.push_str("\n]}\n");
        let mut hashes = String::from("#");
        while body.contains(&format!("\"{hashes}")) {
            hashes.push('#');
        }
        let _ = write!(text, "\n    r{hashes}\"{body}\"{hashes},");
    }
    if !releases.is_empty() {
        text.push('\n');
    }
    text.push_str("];\n");
    Ok(text)
}

/// The source the sealed releases are kept in decodes to them, and the
/// source of none is the one the repository keeps before the 1.0 cut.
#[test]
fn the_sealed_source_keeps_the_releases_it_is_given() {
    assert_eq!(
        sealed_source(&[]).expect("no release"),
        include_str!("sealed.rs")
    );
    let mut shipped = release::freeze("1.0", 1, None, &helper_registry(1), &Default::default());
    shipped.seal().expect("seal 1.0");
    let source = sealed_source(std::slice::from_ref(&shipped)).expect("1.0's source");
    let start = source.find("r#\"").expect("a raw string") + 3;
    let end = source.rfind("\"#").expect("its end");
    assert_eq!(
        HelperRelease::decode(&source[start..end]).expect("1.0 decodes"),
        shipped
    );
}

fn json(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}
