//! Leaf utilities of the Lash runtime kernel.
//!
//! These modules sit at the bottom of `lash-core`'s dependency graph: durable
//! identity framing, stable hashing, canonical JSON leaves, and a handful of
//! process-local helpers. They depend on nothing else in the kernel, so they
//! live in their own crate and `lash-core` re-exports every one of them at its
//! original path.

pub mod clock;
pub mod execution_permit;
pub mod identity_json;
/// `tokio::sync::Notify` semantics on loom primitives for the `cfg(loom)` seam
/// tests (FIG-1161). Public only because the `changed`/`dispatcher_changed`
/// notifier type crosses crate boundaries into `lash-core` and
/// `lash-core-worker` signatures.
#[cfg(loom)]
pub mod loom_notify;
pub mod operational_metrics;
pub mod panic_containment;
#[cfg(feature = "perf-witness")]
pub mod perf_witness;
pub mod stable_hash;
pub mod stable_identity;
pub mod task;
#[cfg(any(test, feature = "testing"))]
pub mod test_clock;
pub mod test_watchdog;
#[cfg(feature = "testing")]
pub mod trace_capture;
pub mod worker_capacity;
