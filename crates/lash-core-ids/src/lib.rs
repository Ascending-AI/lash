//! Leaf utilities of the Lash runtime kernel.
//!
//! These modules sit at the bottom of `lash-core`'s dependency graph: durable
//! identity framing, stable hashing, canonical JSON leaves, and a handful of
//! process-local helpers. They depend on nothing else in the kernel, so they
//! live in their own crate and `lash-core` re-exports every one of them at its
//! original path.

#[cfg(any(test, feature = "testing"))]
pub mod cancel_sweep;
pub mod clock;
pub mod identity_json;
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

/// The id a plugin registers under: the name of its state and config
/// namespaces and of its place in the plugin composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct PluginId(&'static str);

impl PluginId {
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

pub use lash_vocabulary::providers::FormatVersion;
