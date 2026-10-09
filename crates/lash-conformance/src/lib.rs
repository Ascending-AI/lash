//! Backend certification laws shared by store implementations.

use lash_core::*;
mod conformance;
pub use conformance::*;
#[expect(
    dead_code,
    reason = "the explorer's one law, send versus redrive, was deleted with the shift (L3s, FIG-5196); L9c re-proves it on the session actor"
)]
mod interleave;
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
#[cfg(feature = "lashlang")]
pub mod fused_artifact_store;
#[cfg(test)]
mod live_replay_store_tests;
#[cfg(test)]
mod process_replay_store_tests;

#[cfg(test)]
mod host_admission_tests;
