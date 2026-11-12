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

/// [`installed_functions`] with every native that stands beside a kernel
/// body removed, and no generated plans: each library function a body
/// defines answers from that body. A native without a body stays, for
/// nothing else answers it.
///
/// # Errors
/// [`installed_functions`]'s.
pub fn installed_bodies() -> Result<FunctionRegistry, LibraryError> {
    let installed = installed_functions()?;
    let mut registry = lash_kernel_releases::helper_release()?.functions.clone();
    for (function, registered) in installed.iter() {
        if let Some(native) = registered.native()
            && !registered.has_body()
        {
            registry
                .implement(*function, std::sync::Arc::clone(native))
                .map_err(LibraryError::new)?;
        }
    }
    Ok(registry)
}

/// Runs every alternative body through Decline while retaining which functions
/// have natives in this image (`K-MACH-002`, ruling 7bcba3681b). Plans and native
/// specializations are disabled; body-only functions keep their own steps.
///
/// # Panics
/// When the registry refuses to install a native its own definition states.
pub fn declining_bodies(
    library: &lash_kernel_vm::PreparedLibrary,
) -> lash_kernel_vm::PreparedLibrary {
    struct Decline;
    impl lash_kernel_doc::NativeFunction for Decline {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Err(lash_kernel_doc::NativeError::Decline)
        }
    }
    let mut registry = library.registry().bodies();
    for (function, registered) in library.registry().iter() {
        if registered.has_body() && registered.native().is_some() {
            registry
                .implement(*function, std::sync::Arc::new(Decline))
                .unwrap_or_else(|error| panic!("{function} retains its native: {error}"));
        }
    }
    lash_kernel_vm::PreparedLibrary::with_helpers(
        std::sync::Arc::new(registry),
        std::sync::Arc::clone(library.helpers()),
    )
}
