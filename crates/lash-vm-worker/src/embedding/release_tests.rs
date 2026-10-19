//! The helper releases an embedding holds for the dialects its host named
//! (FIG-5799, FIG-5990): the releases themselves are `lash-vm-releases`',
//! which the build defines and checks.

use std::collections::BTreeSet;

use lash_kernel_doc::FunctionId;
use lash_vm_client::WorkerTuning;

use super::tests::{every_dialect, typescript_only};
use super::{EmbedError, HELPER_RELEASE};

/// An embedding holds every function of every helper release its dialects
/// retain, by identity, and resolves names against its own release's alone:
/// release 1.0's functions, which a run of a build of it may pin, are all
/// held, and a writer held to release 1.0 resolves each name to the
/// function that release wrote (FIG-5799), with one dialect and with two.
#[test]
fn every_function_of_a_retained_helper_release_is_held() {
    for embed in [typescript_only, every_dialect] {
        every_function_of_release_one_is_held(
            &embed(&WorkerTuning::standard()).expect("an embedding"),
        );
    }
}

fn every_function_of_release_one_is_held(embedding: &super::Embedding) {
    let written = embedding
        .registry_for(lash_kernel_doc::KERNEL_VERSION)
        .expect("the functions as registered");
    let first = embedding
        .helper_releases()
        .next()
        .expect("the embedding retains a helper release");
    assert_eq!((first.release.as_str(), first.ordinal), ("1.0", 1));
    for function in &first.functions {
        assert!(
            written.get(function).is_some(),
            "release 1.0 holds {function}"
        );
    }
    let library = embedding.library_for(1).expect("release 1.0's names");
    let resolved: BTreeSet<FunctionId> = library.iter().map(|(_, id)| id).collect();
    assert_eq!(
        resolved, first.writes,
        "release 1.0 resolves names as it wrote them"
    );
    assert!(matches!(
        embedding.library_for(HELPER_RELEASE + 1),
        Err(EmbedError::HelperReleaseNotHeld { .. })
    ));
}
