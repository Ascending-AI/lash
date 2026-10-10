//! The helper release the tree builds, as the repository keeps it
//! (FIG-5799): its generator, and the laws that hold the tree to it.

// FIG-2971: this file is test/tooling code; the generator's ambient fs/env
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeSet;

use lash_kernel_doc::FunctionId;
use lash_vm_client::WorkerTuning;

use super::{EmbedError, HELPER_RELEASE, standard};

#[path = "../build/release.rs"]
mod release;

fn helper_registry(result: i64) -> lash_kernel_doc::FunctionRegistry {
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
    ))
    .expect("a helper definition");
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    registry
        .register(definition, None)
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
            .retained
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
    let building = release::freeze("2.0", 2, None, &current, &fingerprints);
    let frozen = release::refreeze(&building, Some(&previous), &current, &fingerprints)
        .expect("the new release");
    assert_eq!(
        frozen.file_name().expect("the destination"),
        "helpers_2_0.rs"
    );
    assert_eq!(frozen.functions.len(), 2, "the sealed helper remains held");
    assert_eq!(frozen.writes.len(), 1, "only the current helper is named");
    let kept = release::keep(&[previous, frozen], &current, &fingerprints)
        .expect("the sealed helper remains runnable");
    assert_eq!(kept.retained.len(), 1);
    assert!(kept.divergence.is_empty());
}

/// V17: regeneration cannot turn a shipped tail back into an unsealed baseline.
#[test]
fn a_sealed_tail_requires_a_new_release_before_regeneration() {
    let registry = helper_registry(1);
    let fingerprints = Default::default();
    let mut sealed = release::freeze("1.0", 1, None, &registry, &fingerprints);
    sealed.seal().expect("seal the baseline");
    assert!(
        sealed.seal().is_err(),
        "sealing is explicit and happens once"
    );
    assert!(release::refreeze(&sealed, None, &registry, &fingerprints).is_err());
}

/// V17: a sealed building release cannot silently write changed helpers.
#[test]
fn a_sealed_building_release_rejects_changed_writes() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let mut sealed = release::freeze("1.0", 1, None, &old, &fingerprints);
    sealed.sealed = true;
    assert!(release::keep(&[sealed], &current, &fingerprints).is_err());
}

/// The helper release the tree builds, frozen for the tree as it stands
/// (`build.rs`): the source `lash-vm-library`'s `src/generated/` keeps it in.
const FROZEN: &str = include_str!(concat!(env!("OUT_DIR"), "/helper_release.json"));

/// Where the release the tree builds and the build part (`build.rs`).
const DIVERGENCE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/helper_release_divergence.json"));

/// Regenerates an unsealed baseline; LASH_SEAL=1 explicitly seals it at the cut.
#[test]
#[ignore = "regenerates the building helper release artifact"]
fn regenerate_helper_release() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let mut frozen = lash_vm_library::helper_releases()
        .expect("releases")
        .pop()
        .expect("building release");
    assert!(
        !frozen.sealed,
        "a sealed release cannot be regenerated; declare a new release"
    );
    frozen = lash_vm_library::HelperRelease::decode(FROZEN).expect("the frozen release");
    if std::env::var("LASH_SEAL").as_deref() == Ok("1") {
        frozen.seal().expect("seal once");
    }
    let workspace =
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the regeneration workspace");
    std::fs::write(
        std::path::Path::new(&workspace)
            .join("crates/lash-vm-library/src/generated")
            .join(frozen.file_name().expect("a release file name")),
        frozen.source().expect("the release source"),
    )
    .expect("the release is written");
}

/// The helper release the tree builds is the tree's: it writes exactly the
/// functions the build defines, under their identities, and every native it
/// holds answers as it did when the release was frozen (FIG-5799). A change
/// to a helper or a native is frozen again before it lands, keeping what
/// runs written against the earlier freeze pin.
#[test]
fn the_tree_builds_the_helper_release_it_froze() {
    let divergence: Vec<String> =
        serde_json::from_slice(DIVERGENCE).expect("the divergence decodes");
    assert!(divergence.is_empty(), "{}", divergence.join("\n"));
}

/// The standard embedding holds every function of every helper release it
/// retains, by identity, and resolves names against its own release's
/// alone: release 1.0's functions, which a run of a build of it may pin,
/// are all held, and a writer held to release 1.0 resolves each name to the
/// function that release wrote (FIG-5799).
#[test]
fn every_function_of_a_retained_helper_release_is_held() {
    let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
    let written = embedding
        .registry_for(lash_kernel_doc::KERNEL_VERSION)
        .expect("the functions as registered");
    let first = embedding
        .helper_releases()
        .next()
        .expect("the standard embedding retains a helper release");
    assert_eq!((first.release.as_str(), first.ordinal), ("1.0", 1));
    for function in &first.functions {
        assert!(
            written.get(function).is_some(),
            "release 1.0 holds {function}"
        );
    }
    let library = embedding.library_for(1).expect("release 1.0's names");
    let resolved: BTreeSet<FunctionId> = library.iter().map(|(_, id)| *id).collect();
    assert_eq!(
        resolved, first.writes,
        "release 1.0 resolves names as it wrote them"
    );
    assert!(matches!(
        embedding.library_for(HELPER_RELEASE + 1),
        Err(EmbedError::HelperReleaseNotHeld { .. })
    ));
}
