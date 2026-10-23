//! The build image is an immutable library, with no package publication work.
use super::*;
#[path = "helper_classes.rs"]
mod helper_classes;

/// Release metadata opens without decoding helper bodies or recomputing identity.
#[test]
fn release_metadata_opens_without_helper_body_work() {
    let _ = lash_kernel_doc::document_work::helper_work();
    let release = helper_release().expect("build helper library");
    assert!(!release.roots.is_empty());
    for root in &release.roots {
        assert!(release.helpers.contains(root));
    }
    assert_eq!(
        lash_kernel_doc::document_work::helper_work(),
        lash_kernel_doc::document_work::HelperWork::default()
    );
    assert_eq!(
        release.helpers.identity(),
        lash_vocabulary::HelperLibraryId::of_members(release.helpers.members().copied())
    );
}
