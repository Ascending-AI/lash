//! Each dialect's helper releases, as data (FIG-5799, FIG-5990).
//!
//! A host names the dialects it builds with (`dialect-typescript`,
//! `dialect-python`; none is a default), and the build holds a helper
//! release line for each of them and for no other: a TypeScript-only host
//! carries none of Python's helpers. A release's name and ordinal are
//! lash's, one line for every dialect ([`RETAINED_HELPER_RELEASES`]); what
//! it holds is each dialect's own, sealed on its own.
//!
//! A sealed release has shipped: the repository keeps it as it was sealed
//! (`src/sealed.rs`), and a build that cannot hold it exactly fails. The
//! release the tree builds is the build's own until the cut that ships it
//! seals it: the build script defines it from the current helper sources,
//! so changing a helper changes no checked-in file (FIG-5839). Sealing is
//! one generator run at the cut ([`SEAL`]), which keeps each dialect's
//! release the tree builds as it stands and so makes it sealed.
//!
//! This crate holds data alone. A parent reads the releases from it
//! (`lash-vm-library`) and a worker its helpers and retained functions
//! (`lash-vm-worker`), both from the one build of it; the dialects and the
//! extensions are dependencies of its build script, so a host that only
//! edits workflows links neither (FIG-5812).

mod declared;
mod manifest;
mod releases;

pub use declared::{
    PYTHON, RETAINED_HELPER_RELEASES, RETIRING_HELPER_RELEASES, SHIPPED_DIALECTS, TYPESCRIPT,
};
pub use manifest::{
    HELPER_DEFINITION_KIND, HeldManifests, ManifestFunction, RELEASE_MANIFEST_KIND,
    ReleaseManifest, ReleaseRefusal, stage_release,
};
pub use releases::{HeldReleases, HelperRelease, HelperReleaseIndex, ReleasedFunction};

/// The generator that seals each built dialect's release the tree builds,
/// and keeps every sealed release, in `src/sealed.rs`. The 1.0 cut runs it
/// once, in a build with every shipped dialect.
pub const SEAL: &str = "kiln test //crates/lash-vm-releases:lash-vm-releases__unit_test --local-test-execution --no-test-cache --test_arg=--ignored --test_arg=--exact --test_arg=tests::seal_helper_release --test_env=LASH_REGENERATE=1 --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD";

/// One dialect's helpers and helper releases, as the build defined and
/// resolved them (`build.rs`).
#[derive(Clone, Copy, Debug)]
pub struct DialectReleases {
    /// The dialect's name: what a session records and a run names.
    pub dialect: &'static str,
    /// The dialect's helpers as the build defined them against the library
    /// it ships with, validated in a registry holding exactly that.
    pub helpers: &'static [u8],
    /// The functions of the helper releases the build retains for the
    /// dialect that it does not define itself, validated in a registry
    /// holding exactly the dialect's library and helpers.
    pub retained_functions: &'static [u8],
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
    /// The helper releases the build holds for the dialect: the ones it
    /// retains, oldest first, the last being the release the tree builds,
    /// and the ones it retires.
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
            retained_functions: include_bytes!(concat!(
                env!("OUT_DIR"),
                "/",
                $dialect,
                "_retained_functions.bin"
            )),
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

/// Kernel collection definitions, validated against the kernel's native base
/// and encoded in the same token form as dialect helpers.
pub const KERNEL_COLLECTIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/kernel_collections.bin"));

/// TypeScript date and URL definitions, validated against its kernel and regex library.
#[cfg(feature = "dialect-typescript")]
pub const TYPESCRIPT_EXTENSIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/typescript_extensions.bin"));
