//! Backend certification laws shared by store implementations.

use lash_core::*;
mod conformance;
pub use conformance::*;
use lash_core::attachments::*;
use lash_core::facade_support::*;
use lash_core::runtime::*;
use lash_core::store::*;
mod macros;
/// The laws derive tool-intent keys, and read rendered
/// keys back, exactly as lash's own start paths do.
const DERIVED_START_KEYS: lash_core::core_internal::StartKeyDerivation =
    lash_core::core_internal::StartKeyDerivation::LASH_START_PATHS;
#[cfg(test)]
mod backend_assembly_tests;
#[cfg(feature = "lash-vm")]
pub mod fused_artifact_store;
#[cfg(test)]
mod live_replay_store_tests;
#[cfg(feature = "lash-vm")]
pub mod module_artifact_store;
#[cfg(test)]
mod process_replay_store_tests;

#[cfg(test)]
mod host_admission_tests;
