//! lash's own packages, made by the build (FIG-6917), and what a parent
//! holds of them (FIG-5812). A parent links, admits and migrates documents
//! against exactly the functions its workers hold, and runs none of them
//! (ADR 0123): it decodes the packages as data, without linking a dialect
//! or an extension, and registers them once as a [`HeldPackages`], from
//! which its admission and every worker's registry are derived (V1). A
//! build with no dialect feature holds `kernel` alone.

use std::sync::{Arc, OnceLock};

use lash_kernel_package::{HeldPackages, Package, PackageHeader};

/// The TypeScript dialect name recorded by sessions and runs.
pub const TYPESCRIPT: &str = "typescript";
/// The Python dialect name recorded by sessions and runs.
pub const PYTHON: &str = "python";
/// Dialects whose packages the build may include.
pub const SHIPPED_DIALECTS: &[&str] = &[TYPESCRIPT, PYTHON];

#[cfg(test)]
mod tests;

/// One of lash's packages as the build made it: its header and the
/// release image of its code (`lash_kernel_package::Package::from_image`).
#[derive(Clone, Copy, Debug)]
pub struct BuiltPackage {
    /// The package's path.
    pub path: &'static str,
    /// Its `PackageHeader`, as JSON.
    pub header: &'static str,
    /// Its code, validated against the packages it requires.
    pub image: &'static [u8],
    /// This build's native implementations, sorted by function identity.
    pub natives: &'static [(lash_vocabulary::legacy_names::FunctionId, &'static str)],
}

impl BuiltPackage {
    /// The identities and local names declared by this function's body.
    /// Constructor tables project these build-owned facts; workers never
    /// resolve an implementation's dependencies through a global name index.
    pub fn native_callees(
        &self,
        function: &lash_vocabulary::legacy_names::FunctionId,
    ) -> Result<Vec<(lash_vocabulary::legacy_names::FunctionId, &'static str)>, String> {
        let image = lash_kernel_doc::ReleasedFunctions::read(self.image)
            .map_err(|error| error.to_string())?;
        let at = image
            .ids()
            .position(|id| id == *function)
            .ok_or_else(|| format!("{} holds no native {function}", self.path))?;
        Ok(image.listed(at).collect())
    }
}

macro_rules! package {
    ($path:literal, $stem:literal) => {
        BuiltPackage {
            path: $path,
            header: include_str!(concat!(env!("OUT_DIR"), "/", $stem, ".package.json")),
            image: include_bytes!(concat!(env!("OUT_DIR"), "/", $stem, ".bin")),
            natives: include!(concat!(env!("OUT_DIR"), "/", $stem, ".natives.rs")),
        }
    };
}

/// lash's packages this build holds, each after the packages it requires:
/// `kernel` always, and those of the dialects its features name.
pub const PACKAGES: &[BuiltPackage] = &[
    package!("kernel", "kernel"),
    #[cfg(feature = "dialect-typescript")]
    package!("regex.ecma", "regex_ecma"),
    #[cfg(feature = "dialect-typescript")]
    package!("date.ecma", "date_ecma"),
    #[cfg(feature = "dialect-typescript")]
    package!("url.whatwg", "url_whatwg"),
    #[cfg(feature = "dialect-typescript")]
    package!("typescript", "typescript"),
    #[cfg(feature = "dialect-python")]
    package!("python", "python"),
];

/// The package at `path`, when this build holds it.
#[must_use]
pub fn package(path: &str) -> Option<&'static BuiltPackage> {
    PACKAGES.iter().find(|built| built.path == path)
}

/// Why lash's packages could not be assembled: a defect of the build,
/// never of a document.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("lash's packages do not assemble: {message}")]
pub struct LibraryError {
    message: String,
}

impl LibraryError {
    fn new(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

fn standard_in(
    slot: &OnceLock<Result<Arc<HeldPackages>, LibraryError>>,
    assemble: impl FnOnce() -> Result<Arc<HeldPackages>, LibraryError>,
) -> Result<Arc<HeldPackages>, LibraryError> {
    // Assembly decodes and registers every package. Cold readers must
    // wait for that work, rather than each allocating their own copy. A
    // failure is a fixed defect of this build, so retain it as well.
    slot.get_or_init(assemble).clone()
}

/// lash's packages, decoded from the build's images in dependency order.
///
/// # Errors
///
/// The decoder's message: a defect of the build.
pub fn built() -> Result<Vec<Package>, String> {
    PACKAGES
        .iter()
        .map(|built| {
            let header: PackageHeader =
                serde_json::from_str(built.header).map_err(|error| error.to_string())?;
            let image = lash_kernel_doc::ReleasedFunctions::read(built.image)
                .map_err(|error| error.to_string())?;
            let package = Package::from_image(header, image).map_err(|error| error.to_string())?;
            let display = lash_kernel_package::shipped_display::of(&package)
                .map_err(|error| error.to_string())?;
            Ok(package.with_display(display))
        })
        .collect()
}

/// lash's own packages as this build holds them, registered once in a
/// process: the set a host that installs no package of its own runs on,
/// and the requirements a host's package is built against.
///
/// # Errors
///
/// [`LibraryError`].
pub fn lash_packages() -> Result<Arc<HeldPackages>, LibraryError> {
    static STANDARD: OnceLock<Result<Arc<HeldPackages>, LibraryError>> = OnceLock::new();
    standard_in(&STANDARD, || {
        Ok(Arc::new(
            HeldPackages::install(built().map_err(LibraryError::new)?)
                .map_err(LibraryError::new)?,
        ))
    })
}
