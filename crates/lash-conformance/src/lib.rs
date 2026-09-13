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

/// Locate a dev-only recovery helper for a conformance test.
///
/// Cargo leaves the override unset, so `name` resolves below the current
/// profile's `examples` directory. Bazel sets `LASH_CONFORMANCE_HELPER_EXE` to
/// one exact runfile path. That override is intentionally returned verbatim:
/// a Bazel target's output basename is not required to equal its Cargo example
/// name, so joining or validating it against `name` would reject a valid
/// hermetic runfile.
pub fn helper_executable(name: &str) -> std::path::PathBuf {
    resolve_helper_executable(
        name,
        std::env::var_os("LASH_CONFORMANCE_HELPER_EXE").map(Into::into),
        std::env::current_exe,
    )
}

fn resolve_helper_executable(
    name: &str,
    exact_override: Option<std::path::PathBuf>,
    current_exe: impl FnOnce() -> std::io::Result<std::path::PathBuf>,
) -> std::path::PathBuf {
    if let Some(path) = exact_override {
        return path;
    }
    current_exe()
        .expect("locate current test executable")
        .parent()
        .expect("test executable has a deps directory")
        .parent()
        .expect("deps directory has a profile directory")
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

#[cfg(test)]
mod helper_executable_tests {
    use super::resolve_helper_executable;
    use std::path::PathBuf;

    #[test]
    fn exact_override_is_not_rewritten_with_the_cargo_name() {
        let runfile = PathBuf::from("runfiles/store/sqlite-helper__example");
        let resolved = resolve_helper_executable("sqlite-helper", Some(runfile.clone()), || {
            panic!("the Cargo executable path must stay lazy when Bazel supplies a runfile")
        });

        assert_eq!(resolved, runfile);
    }

    #[test]
    fn unset_override_preserves_the_cargo_profile_layout() {
        let resolved = resolve_helper_executable("sqlite-helper", None, || {
            Ok(PathBuf::from("target/debug/deps/conformance-test"))
        });

        assert_eq!(
            resolved,
            PathBuf::from(format!(
                "target/debug/examples/sqlite-helper{}",
                std::env::consts::EXE_SUFFIX
            ))
        );
    }
}
