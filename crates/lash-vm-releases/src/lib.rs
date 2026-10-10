//! The helper releases lash's shipped worker holds, as data (FIG-5799).
//!
//! A sealed release has shipped: the repository keeps it as it was sealed
//! (`src/sealed.rs`), and a build that cannot hold it exactly fails. The
//! release the tree builds is the build's own until the cut that ships it
//! seals it: the build script defines it from the current helper sources,
//! so changing a helper changes no checked-in file (FIG-5839). Sealing is
//! one generator run at the cut ([`SEAL`]), which keeps the release the
//! tree builds as it stands and so makes it sealed.
//!
//! This crate holds data alone. A parent reads the releases from it
//! (`lash-vm-library`) and a worker its helpers and retained functions
//! (`lash-vm-worker`), both from the one build of it; the dialect and the
//! extensions are dependencies of its build script, so a host that only
//! edits workflows links neither (FIG-5812).

mod declared;
mod releases;

pub use declared::{RETAINED_HELPER_RELEASES, RETIRING_HELPER_RELEASES};
pub use releases::{HeldReleases, HelperRelease, HelperReleaseIndex, ReleasedFunction};

/// The generator that seals the release the tree builds, and keeps every
/// sealed release, in `src/sealed.rs`. The 1.0 cut runs it once.
pub const SEAL: &str = "kiln test //crates/lash-vm-releases:lash-vm-releases__unit_test --local-test-execution --no-test-cache --test_arg=--ignored --test_arg=--exact --test_arg=tests::seal_helper_release --test_env=LASH_REGENERATE=1 --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD";

/// The TypeScript helpers as the build defined them against the kernel
/// library and lash's extensions, validated in a registry holding exactly
/// those (`build.rs`).
pub const TYPESCRIPT_HELPERS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/typescript_helpers.json"));

/// The functions of the helper releases the build retains that it does not
/// define itself, validated in a registry holding exactly the kernel
/// library, lash's extensions and the TypeScript helpers (`build.rs`).
pub const RETAINED_FUNCTIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/retained_functions.json"));

/// The index of each helper release the build retains, oldest first
/// (`build.rs`).
pub const HELPER_RELEASE_INDEX: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/helper_releases.json"));

/// Every helper release the build holds, as it resolved them (`build.rs`).
const HELD: &str = include_str!(concat!(env!("OUT_DIR"), "/held_releases.json"));

/// The helper releases the build holds: the ones it retains, oldest first,
/// the last being the release the tree builds, and the ones it retires.
///
/// # Errors
///
/// The decoder's message: a defect of the build.
pub fn held_releases() -> Result<HeldReleases, String> {
    HeldReleases::decode(HELD)
}

#[cfg(test)]
mod tests;
