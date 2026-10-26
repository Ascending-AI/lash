//! Format and component identities in immutable grants.
use serde::{Deserialize, Serialize};
use std::fmt;

/// A plugin's declared, nonzero state and config format version.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
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

/// The id a process component registers under. A `:` is refused: the fleet
/// writer record keys a component's config format as `component:<id>`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessComponentId(String);

/// Why a process component id is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProcessComponentIdError {
    #[error("a process component id is empty")]
    Empty,
    #[error("process component id `{id}` contains `:`, which the fleet writer key reserves")]
    Colon { id: String },
}

impl ProcessComponentId {
    /// # Errors
    ///
    /// An empty id, or one containing `:`.
    pub fn new(id: impl Into<String>) -> Result<Self, ProcessComponentIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ProcessComponentIdError::Empty);
        }
        if id.contains(':') {
            return Err(ProcessComponentIdError::Colon { id });
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fleet writer-record key this component's state/config format is
    /// admitted under: `component:<id>`, never a session plugin's key.
    pub fn writer_key(&self) -> String {
        format!("component:{}", self.0)
    }
}

impl TryFrom<String> for ProcessComponentId {
    type Error = ProcessComponentIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProcessComponentId> for String {
    fn from(id: ProcessComponentId) -> Self {
        id.0
    }
}

impl fmt::Display for ProcessComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The host's execution and retry contract for a dispatch grant's
/// discovery, recorded with the grant in the process environment before its
/// first resolution. Resolution and the matched tool have independent
/// bounds. As with ordinary tool bounds,
/// there is no runtime-selected ceiling; enclosing deadlines still clip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeferredResolutionPolicy {
    pub execution: std::time::Duration,
    pub execution_policy: crate::ExecutionPolicy,
}

impl Default for DeferredResolutionPolicy {
    /// Sixty seconds, with three repeatable attempts and exponential backoff
    /// from 100 ms up to 1,000 ms.
    fn default() -> Self {
        Self {
            execution: std::time::Duration::from_secs(60),
            execution_policy: crate::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::MIN.saturating_add(2),
                100,
                1_000,
            ),
        }
    }
}
