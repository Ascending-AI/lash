//! Leaf utilities of the Lash runtime kernel.
//!
//! These modules sit at the bottom of `lash-core`'s dependency graph: durable
//! identity framing, stable hashing, canonical JSON leaves, and a handful of
//! process-local helpers. They depend on nothing else in the kernel, so they
//! live in their own crate and `lash-core` re-exports every one of them at its
//! original path.

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

/// A plugin's declared, nonzero state and config format version.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct FormatVersion(std::num::NonZeroU32);

impl FormatVersion {
    pub const ONE: Self = Self(std::num::NonZeroU32::MIN);

    pub const fn new(value: u32) -> Option<Self> {
        match std::num::NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
