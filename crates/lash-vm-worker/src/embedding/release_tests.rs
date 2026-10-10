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

/// The helper release the tree builds, frozen for the tree as it stands
/// (`build.rs`): the source `lash-vm-library`'s `src/generated/` keeps it in.
const FROZEN: &str = include_str!(concat!(env!("OUT_DIR"), "/helper_release.rs"));

/// Where the release the tree builds and the build part (`build.rs`).
const DIVERGENCE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/helper_release_divergence.json"));

/// Freezes the helper release the tree builds for the tree as it stands:
/// it writes the build's functions and keeps every function of its earlier
/// freeze the build can still run (FIG-5799).
#[test]
#[ignore = "regenerates crates/lash-vm-library/src/generated/helpers_1_0.rs"]
fn regenerate_helper_release() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let workspace =
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the regeneration workspace");
    std::fs::write(
        std::path::Path::new(&workspace)
            .join("crates/lash-vm-library/src/generated/helpers_1_0.rs"),
        FROZEN,
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
