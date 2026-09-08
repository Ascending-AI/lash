//! Backend certification laws shared by store implementations.

use lash_core::*;
mod conformance;
pub use conformance::*;
use lash_core::attachments::*;
use lash_core::facade_support::*;
use lash_core::runtime::*;
use lash_core::store::*;
mod macros;
use lash_core::testing::conformance_support::default_queued_drain_policy;
pub mod fused_artifact_store;
#[cfg(test)]
mod in_memory;

/// Locate a dev-only recovery helper beside the current Cargo test profile.
/// CI archives these example executables alongside the test binaries.
pub fn helper_executable(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .expect("locate current test executable")
        .parent()
        .expect("test executable has a deps directory")
        .parent()
        .expect("deps directory has a profile directory")
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}
