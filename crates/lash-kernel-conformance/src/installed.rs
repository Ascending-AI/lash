//! The shipped helper library fixture, bound by each source owner's installer.
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_releases::LibraryError;

#[path = "../../lash-kernel-releases/src/library.rs"]
mod library;

/// Installs the selected shipped release's native implementations and helper
/// plans for a dialect conformance run, without a worker or host protocol.
///
/// # Errors
/// A native or generated plan that disagrees with its release declaration.
pub fn installed_functions() -> Result<FunctionRegistry, LibraryError> {
    let mut registry = lash_kernel_releases::helper_release()?.functions.clone();
    for package in lash_kernel_releases::PACKAGES {
        let image =
            lash_kernel_doc::ReleasedFunctions::read(package.image).map_err(LibraryError::new)?;
        library::implement(
            package.path,
            &mut registry,
            &image.ids().collect::<Vec<_>>(),
        )
        .map_err(LibraryError::new)?;
    }
    Ok(registry)
}
