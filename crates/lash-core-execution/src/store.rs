//! Durable session-store seams.
//!
//! The durable layer lives in `lash-core-store`; this module re-exports it at
//! its original path and adds the seams that need a runtime-side supertrait.
pub use lash_core_store::store::*;

#[cfg(any(test, feature = "testing"))]
mod conformance_deployment;
#[cfg(any(test, feature = "testing"))]
pub use conformance_deployment::ConformanceDeployment;
