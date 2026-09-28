//! Backend certification laws shared by store implementations.

use lash_core::*;
mod conformance;
pub use conformance::*;
mod effect_host_macros;
use lash_core::attachments::*;
use lash_core::facade_support::*;
use lash_core::runtime::*;
use lash_core::store::*;
mod macros;
mod response_derivation_macros;
use lash_core::testing::conformance_support::default_queued_drain_policy;
#[cfg(test)]
mod file_attachment_store_tests;
#[cfg(feature = "lashlang")]
pub mod fused_artifact_store;
#[cfg(test)]
mod live_replay_store_tests;
