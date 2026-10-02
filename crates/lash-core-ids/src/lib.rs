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

impl From<std::num::NonZeroU32> for FormatVersion {
    fn from(value: std::num::NonZeroU32) -> Self {
        Self(value)
    }
}

impl From<FormatVersion> for std::num::NonZeroU32 {
    fn from(value: FormatVersion) -> Self {
        value.0
    }
}

/// The declared, nonzero revision of what a plugin does. Any behaviour
/// change moves it, and a moved revision is a new build generation.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct BehaviorRevision(std::num::NonZeroU32);

impl BehaviorRevision {
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

impl std::fmt::Display for BehaviorRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

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
