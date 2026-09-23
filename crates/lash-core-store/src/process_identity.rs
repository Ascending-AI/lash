//! Durable process identity and lifecycle vocabulary.

use crate::{ProcessId, SessionId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// durable references pin this value so they cannot silently rebind after the
/// name is pruned and registered again.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ProcessIncarnation(u64);
impl ProcessIncarnation {
    pub fn from_registration_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    pub fn registration_sequence(self) -> u64 {
        self.0
    }
}
impl fmt::Display for ProcessIncarnation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
/// Structural identity of one process lifetime.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
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
    ///
    /// The record itself is runtime process machinery and stays in
    /// `lash-core`; this takes only the identity pair it projects.
    pub fn from_record(record: &impl ProcessRecordIdentity) -> Self {
        Self::new(record.process_id().clone(), record.process_incarnation())
    }

    /// Delegates to the one handle parser (FIG-2996 part 1) rather than reading
    /// the encoding again: tools that take a process handle as an argument
    /// parse it here, so a handle argument and a handle the runtime minted are
    /// read by the same code.
    pub fn from_handle_json(handle: &serde_json::Value) -> Result<Self, String> {
        let target = lash_sansio::handle::parse_handle_json(handle)
            .as_ref()
            .and_then(lash_sansio::handle::HandleId::target)
            .ok_or_else(|| "Invalid process handle".to_string())?;
        let lash_sansio::handle::HandleTarget::Process {
            process_id,
            incarnation,
        } = target
        else {
            return Err("Invalid process handle: not a process".to_string());
        };
        if incarnation == 0 {
            return Err("Invalid process handle: missing `incarnation`".to_string());
        }
        Ok(Self::new(
            process_id,
            ProcessIncarnation::from_registration_sequence(incarnation),
        ))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDeliveryState {
    Pending,
    Enqueuing,
    Enqueued,
    Discarded,
}
impl WakeDeliveryState {
    /// Whether the delivery still owes the target a wake.
    ///
    /// The Rust twin of the `state IN (...)` predicate prune and preflight
    /// queries carry. Exhaustive on purpose: a new state must declare whether
    /// it is still owed before any query can compile.
    pub fn is_undelivered(self) -> bool {
        match self {
            Self::Pending | Self::Enqueuing => true,
            Self::Enqueued | Self::Discarded => false,
        }
    }
}

/// Version 3 carries full admitted effect addresses and complete trigger causes
/// in the invocation delivered with a process wake.
pub const PROCESS_WAKE_DELIVERY_FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessWakeDelivery {
    pub version: u32,
    pub wake_id: String,
    pub target_session_id: SessionId,
    pub process_id: ProcessId,
    pub process_incarnation: ProcessIncarnation,
    pub sequence: u64,
    pub event_type: String,
    pub event_invocation: crate::RuntimeInvocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_caused_by: Option<crate::CausalRef>,
    /// Authority captured from the durable process originator at event append.
    /// The delivery driver must forward this unchanged into queued work.
    #[serde(default, skip_serializing_if = "process_wake_authority_is_empty")]
    pub authority: crate::QueuedWorkAuthority,
    pub input: String,
    pub created_at_ms: u64,
}
fn process_wake_authority_is_empty(authority: &crate::QueuedWorkAuthority) -> bool {
    authority.principal.is_none() && authority.elevation.is_none()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessExecutionEnvRef(String);
impl ProcessExecutionEnvRef {
    /// Constructs a `ProcessExecutionEnvRef` for store and durable-substrate implementors suspending or resuming durable process execution.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes the opaque stable reference for continuation-store implementors; its contents carry no ordering or backend-independent structure.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Verify that this reference addresses exactly one valid encoded
    /// process-execution environment.
    pub fn matches_store_bytes(&self, bytes: &[u8]) -> bool {
        ProcessExecutionEnvSpec::from_store_bytes(bytes).is_ok()
            && process_execution_env_ref_for_bytes(bytes) == *self
    }
}
impl fmt::Display for ProcessExecutionEnvRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
pub fn process_execution_env_ref_for_bytes(bytes: &[u8]) -> ProcessExecutionEnvRef {
    ProcessExecutionEnvRef::new(format!(
        "process-env:v6:blake3:{}",
        crate::stable_hash::blake3_hex("lash-process-env/v6", bytes)
    ))
}

pub fn process_wake_turn_cause(wake: &ProcessWakeDelivery) -> crate::TurnCause {
    crate::TurnCause {
        id: wake.wake_id.clone(),
        event_type: wake.event_type.clone(),
        origin: crate::MessageOrigin::Process {
            process_id: wake.process_id.clone(),
            event_type: wake.event_type.clone(),
            sequence: wake.sequence,
            wake_id: Some(wake.wake_id.clone()),
            caused_by: wake.process_caused_by.clone(),
        },
        text: process_wake_turn_text(wake),
    }
}

/// Renders a durable process wake as model-visible chronological context.
pub fn process_wake_turn_text(wake: &ProcessWakeDelivery) -> String {
    // Sender-floor allocation keeps sequences small ordered identifiers, so
    // the model-facing `#<sequence>` remains a useful event label.
    format!(
        "Background process wake\nProcess: {}\nEvent: {} #{}\nWake input:\n{}",
        wake.process_id, wake.event_type, wake.sequence, wake.input
    )
}
pub fn wake_payload_value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// The identity pair a durable process record projects.
///
/// `lash-core`'s `ProcessRecord` is the sole implementor; the seam exists so
/// `ProcessRef` can be pinned from a record without the record's runtime
/// lifecycle vocabulary moving with it.
pub trait ProcessRecordIdentity {
    fn process_id(&self) -> &ProcessId;
    fn process_incarnation(&self) -> ProcessIncarnation;
}

impl<T> ProcessRecordIdentity for &T
where
    T: ProcessRecordIdentity + ?Sized,
{
    fn process_id(&self) -> &ProcessId {
        T::process_id(self)
    }

    fn process_incarnation(&self) -> ProcessIncarnation {
        T::process_incarnation(self)
    }
}

impl<T> ProcessRecordIdentity for Box<T>
where
    T: ProcessRecordIdentity + ?Sized,
{
    fn process_id(&self) -> &ProcessId {
        T::process_id(self)
    }

    fn process_incarnation(&self) -> ProcessIncarnation {
        T::process_incarnation(self)
    }
}

/// Generates a durable lifecycle vocabulary and its complete variant list from
/// one declaration.
///
/// The generated encoder match is exhaustive, so a new variant requires its
/// persisted spelling here and thereby necessarily extends `ALL`. Backends fold
/// `ALL` into SQL predicates
/// (`crate::store_backend_support::live_process_status_predicate_sql` and
/// friends), so a hand-maintained list would be exactly the drift those
/// predicates exist to prevent.
macro_rules! lifecycle_vocabulary {
    ($type:ident, $encoder:ident, by_ref { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $type {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn $encoder(&self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
    ($type:ident, $encoder:ident, by_value { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $type {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn $encoder(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

lifecycle_vocabulary!(ProcessStatus, label, by_ref {
    Running => "running",
    Waiting => "waiting",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Abandoned => "abandoned",
    CallerDeparted => "caller_departed",
});

lifecycle_vocabulary!(WakeDeliveryState, as_str, by_value {
    Pending => "pending",
    Enqueuing => "enqueuing",
    Enqueued => "enqueued",
    Discarded => "discarded",
});
