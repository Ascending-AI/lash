//! Each built dialect's current helper release, defined by the build.
//! A checkpoint pins its exact manifest and refuses a different release.
//! Parents read definitions as data without linking dialect compilers.

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
    /// The dialect's helpers as the build defined them against the library
    /// it ships with, validated in a registry holding exactly that.
    pub helpers: &'static [u8],
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
            helpers: include_bytes!(concat!(env!("OUT_DIR"), "/", $dialect, "_helpers.bin")),
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

/// The closed set of TypeScript helpers defined from the package's kernel sources.
#[cfg(feature = "dialect-typescript")]
mod typescript_helpers {
    include!(concat!(env!("OUT_DIR"), "/typescript_helpers.rs"));
}
#[cfg(feature = "dialect-typescript")]
pub use typescript_helpers::TypeScriptHelper;
/// The closed set of Python helpers defined from the package's kernel sources.
#[cfg(feature = "dialect-python")]
mod python_helpers {
    include!(concat!(env!("OUT_DIR"), "/python_helpers.rs"));
}
#[cfg(feature = "dialect-python")]
pub use python_helpers::PythonHelper;

/// The kernel's native base: its number, text and JSON functions and the
/// machine's own, each a definition its embedder adds the native of.
pub const KERNEL_BASE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kernel_base.bin"));

/// Kernel collection definitions, validated against the kernel's native base
/// and encoded in the same token form as dialect helpers.
pub const KERNEL_COLLECTIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/kernel_collections.bin"));

/// TypeScript regular-expression, date and URL definitions, validated
/// against its kernel.
#[cfg(feature = "dialect-typescript")]
pub const TYPESCRIPT_EXTENSIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/typescript_extensions.bin"));
