//! lash's own packages and each built dialect's current helper release,
//! made by the build. A checkpoint pins its exact manifest and refuses a
//! different release. Parents read definitions as data without linking
//! dialect compilers.

mod manifest;
mod releases;

/// The TypeScript dialect name recorded by sessions and runs.
pub const TYPESCRIPT: &str = "typescript";
/// The Python dialect name recorded by sessions and runs.
pub const PYTHON: &str = "python";
/// Dialects whose helpers the build may include.
pub const SHIPPED_DIALECTS: &[&str] = &[TYPESCRIPT, PYTHON];

pub use manifest::{
    HELPER_DEFINITION_KIND, HeldManifests, ManifestFunction, RELEASE_MANIFEST_KIND,
    ReleaseManifest, ReleaseRefusal, stage_release,
};
pub use releases::{HeldReleases, HelperRelease, HelperReleaseIndex, ReleasedFunction};

/// One dialect's helpers and helper releases, as the build defined and
/// resolved them (`build.rs`).
#[derive(Clone, Copy, Debug)]
pub struct DialectReleases {
    /// The dialect's name: what a session records and a run names.
    pub dialect: &'static str,
    /// The index of each helper release the build retains for the dialect,
    /// oldest first.
    pub helper_release_index: &'static [u8],
    /// Every helper release the build holds for the dialect.
    held: &'static str,
    /// Canonical checkpoint nodes of the dialect's immutable helper
    /// releases, encoded by the build as a sequence of
    /// `(digest, envelope bytes)`.
    pub checkpoint_nodes: &'static [u8],
    /// Exact release manifests and root refs, in retained-release order,
    /// encoded by the build beside `checkpoint_nodes`.
    pub checkpoint_manifests: &'static [u8],
}

impl DialectReleases {
    /// The single current helper release for this dialect.
    ///
    /// # Errors
    ///
    /// The decoder's message: a defect of the build.
    pub fn held_releases(&self) -> Result<HeldReleases, String> {
        HeldReleases::decode(self.held)
    }
}

#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
macro_rules! built {
    ($dialect:literal) => {
        DialectReleases {
            dialect: $dialect,
            helper_release_index: include_bytes!(concat!(
                env!("OUT_DIR"),
                "/",
                $dialect,
                "_helper_releases.json"
            )),
            held: include_str!(concat!(
                env!("OUT_DIR"),
                "/",
                $dialect,
                "_held_releases.json"
            )),
            checkpoint_nodes: include_bytes!(concat!(
                env!("OUT_DIR"),
                "/",
                $dialect,
                "_checkpoint_nodes.bin"
            )),
            checkpoint_manifests: include_bytes!(concat!(
                env!("OUT_DIR"),
                "/",
                $dialect,
                "_checkpoint_manifests.bin"
            )),
        }
    };
}

/// The dialects this build holds helper releases for: the ones its
/// features name, in [`SHIPPED_DIALECTS`]' order.
pub const DIALECTS: &[DialectReleases] = &[
    #[cfg(feature = "dialect-typescript")]
    built!("typescript"),
    #[cfg(feature = "dialect-python")]
    built!("python"),
];

/// What the build holds for `dialect`, when it was built with it.
#[must_use]
pub fn dialect(dialect: &str) -> Option<&'static DialectReleases> {
    DIALECTS.iter().find(|built| built.dialect == dialect)
}

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
    /// The artifact digest stamped by this package's native constructors.
    pub built: lash_kernel_doc::ArtifactDigest,
    /// This build's native implementations, sorted by function identity.
    pub natives: &'static [(lash_kernel_doc::FunctionId, &'static str)],
}

impl BuiltPackage {
    /// The identities and local names declared by this function's body.
    /// Constructor tables project these build-owned facts; workers never
    /// resolve an implementation's dependencies through a global name index.
    pub fn native_callees(
        &self,
        function: &lash_kernel_doc::FunctionId,
    ) -> Result<Vec<(lash_kernel_doc::FunctionId, &'static str)>, String> {
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
            built: include!(concat!(env!("OUT_DIR"), "/", $stem, ".digest.rs")),
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
