//! Durable process identity and lifecycle vocabulary.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

#[serde(transparent)]
pub struct ProcessIncarnation(u64);
impl ProcessIncarnation {
    /// Wrap the registration change sequence allocated by a process store.
    pub fn from_registration_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Expose the registration change sequence to process-store implementors.
    pub fn registration_sequence(self) -> u64 {
        self.0
    }
}
impl fmt::Display for ProcessIncarnation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
pub struct ProcessRef {
    pub process_id: ProcessId,
    pub incarnation: ProcessIncarnation,
}
impl ProcessRef {
    /// Pin a reusable process name to one store-minted incarnation.
    pub fn new(process_id: impl Into<ProcessId>, incarnation: ProcessIncarnation) -> Self {
        Self {
            process_id: process_id.into(),
            incarnation,
        }
    }

    /// Pin the identity carried by a retained process record.
    pub fn from_record(record: &ProcessRecord) -> Self {
        Self::new(record.id.clone(), record.incarnation)
    }
}
impl fmt::Display for ProcessRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.process_id, self.incarnation)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExecutionEnvSpec {
    #[serde(default)]
    pub plugin_options: crate::PluginOptions,
    pub policy: crate::SessionPolicy,
}
impl ProcessExecutionEnvSpec {
    /// Constructs a `ProcessExecutionEnvSpec` for protocol and process-engine implementors running a durable process.
    pub fn new(plugin_options: crate::PluginOptions, policy: crate::SessionPolicy) -> Self {
        Self {
            plugin_options,
            policy,
        }
    }

    /// Content-addresses the exact bytes persisted by [`Self::to_store_bytes`].
    ///
    /// Version 6 adds reasoning-retention capability and selection to the
    /// policy's semantic identity.
    /// Older environment references are refused at load and must be recreated; a
    /// future byte-format change requires a new textual family version and the
    /// same explicit old-row policy. These bytes follow the final binary's
    /// serde-json feature set; enabling order-preserving maps is therefore an
    /// identity-format change that requires a new family version.
    pub fn stable_ref(&self) -> Result<ProcessExecutionEnvRef, serde_json::Error> {
        self.to_store_bytes()
            .map(|bytes| process_execution_env_ref_for_bytes(&bytes))
    }

    /// Serializes a process execution environment for continuation-store implementors, preserving the stable reference alongside plugin and protocol state.
    pub fn to_store_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserializes a stored execution environment for process-engine implementors and returns malformed payloads as plugin errors.
    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}
/// Durable lifecycle status of a process row.
///
/// # Rollout: adding a variant is a one-way door for readers
///
/// This enum has no `#[serde(other)]` fallback arm, by design — a store that
/// silently folded an unknown status into a known one would corrupt the very
/// fold the registry exists to keep honest. The consequence is that a variant
/// is only readable by binaries that know it: an older binary sharing a
/// registry with a newer one hard-errors with `unknown variant
/// caller_departed` on any read that touches such a row. That includes
/// [`ProcessRegistry::processes_changed_since`](crate::ProcessRegistry::processes_changed_since),
/// where the failure is not "skip one row" but a stalled feed — the projector
/// stops advancing its cursor at all.
///
/// [`ProcessStatus::CallerDeparted`] therefore ships without a store schema
/// bump, deliberately: no column shape changed, and every backend already
/// filters the `status` column it is written to. For SQLite that is also the
/// only tenable choice — its stores have no migration chain and refuse any
/// database whose `user_version` does not match exactly, so a bump would make
/// every existing process database unopenable to buy nothing. Postgres *does*
/// have a migration ladder, so a bump was possible there; it was skipped for
/// rollout simplicity and vocabulary parity across backends, not because it
/// could not be done.
///
/// Operationally: upgrade readers before any writer can emit a new status.
/// A mixed-version fleet sharing one registry must roll all binaries forward
/// first; rolling a writer out ahead of its readers stalls their feeds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    /// The caller that registered an Externally-Owned row departed after the
    /// row committed and before any outcome was recorded.
    ///
    /// Deliberately **not** terminal: lash cannot observe whether the external
    /// work it was recording ever happened, and writing `Cancelled` or
    /// `Failed` would assert an outcome lash never saw. The row is instead
    /// durably distinguishable from an Externally-Owned row whose caller is
    /// still present, so external reconciliation can close it with the truth,
    /// awaits can refuse instead of parking forever, and retention can reclaim
    /// it (see [`ProcessStatus::is_retired`]).
    CallerDeparted,
}
impl ProcessStatus {
    /// Maps a terminal process outcome to its durable status for process-store implementors;
    /// non-terminal variants remain running.
    pub fn from_terminal(terminal: ProcessTerminalSemantics) -> Self {
        terminal.status
    }

    /// Whether the row is still on the live worklist.
    ///
    /// This is the Rust twin of the `status IN (...)` predicate every backend
    /// worklist query carries, and the complement of
    /// [`ProcessStatus::is_retired`]. The match is exhaustive on purpose: a new
    /// variant must declare which side of the live/retired partition it falls
    /// on before any query can compile.
    pub fn is_live(&self) -> bool {
        match self {
            Self::Running | Self::Waiting => true,
            Self::Completed
            | Self::Failed
            | Self::Cancelled
            | Self::Abandoned
            | Self::CallerDeparted => false,
        }
    }

    /// Lets process-store implementors apply retention only to completed, failed, cancelled, or
    /// abandoned rows; running, waiting, and caller-departed rows are never terminal.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned => true,
            Self::Running | Self::Waiting | Self::CallerDeparted => false,
        }
    }

    /// Lets process-store implementors select the rows retention may reclaim.
    ///
    /// Retention reclaims a row; it never asserts an outcome. Terminal rows
    /// qualify because their outcome is recorded, and
    /// [`ProcessStatus::CallerDeparted`] qualifies because lash can never
    /// record one: leaving those rows out would let a host accumulate them
    /// without bound, since nothing may honestly terminalize them.
    pub fn is_retired(&self) -> bool {
        match self {
            Self::Completed
            | Self::Failed
            | Self::Cancelled
            | Self::Abandoned
            | Self::CallerDeparted => true,
            Self::Running | Self::Waiting => false,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObserverInheritance {
    #[default]
    All,
    None,
    Only(Vec<ProcessId>),
}
