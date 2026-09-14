//! Leaf utilities of the Lash runtime kernel.
//!
//! These modules sit at the bottom of `lash-core`'s dependency graph: durable
//! identity framing, stable hashing, canonical JSON leaves, and a handful of
//! process-local helpers. They depend on nothing else in the kernel, so they
//! live in their own crate and `lash-core` re-exports every one of them at its
//! original path.

pub mod identity_json;
pub mod operational_metrics;
pub mod panic_containment;
#[cfg(feature = "perf-witness")]
#[doc(hidden)]
pub mod perf_witness;
pub mod stable_hash;
pub mod stable_identity;
pub mod task;
pub mod test_watchdog;
