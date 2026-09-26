//! Durable process identity and lifecycle vocabulary.

use crate::{ProcessId, SessionId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Mint the id of one newly registered process.
///
/// Only the process registrar calls this, inside the transaction that
/// registers the process. The id is a UUIDv7 — time-ordered with 74 random
/// bits — and is never reused, so it is the one identity a process has
/// (ADR 0107). Minting reads the clock and the random source; the durable
/// drive never calls it, it reads the id back off the start's recorded result.
#[must_use]
pub fn mint_process_id() -> ProcessId {
    ProcessId::from_minted(uuid::Uuid::now_v7().as_u128())
}

/// Where a process registrar's minted ids come from.
///
/// Production registrars mint a fresh UUIDv7 per registration
/// ([`ProcessIdMint::Random`]). A fixture generator whose artifacts must
/// regenerate byte-identically installs [`ProcessIdMint::sequential_for_testing`],
/// which mints UUIDv7-shaped ids from a counter, in registration order.
#[derive(Clone, Debug, Default)]
pub enum ProcessIdMint {
    /// A fresh UUIDv7 per registration.
    #[default]
    Random,
    /// Deterministic ids from a shared counter. Testing only.
    #[doc(hidden)]
    Sequential(std::sync::Arc<std::sync::atomic::AtomicU64>),
}

impl ProcessIdMint {
    /// A deterministic mint for fixture generation: the `n`th id it mints is
    /// the UUIDv7-shaped value `n`, so ids sort in registration order.
    #[doc(hidden)]
    #[must_use]
    pub fn sequential_for_testing() -> Self {
        Self::Sequential(std::sync::Arc::default())
    }

    /// The id a [`sequential_for_testing`](Self::sequential_for_testing)
    /// mint gives its `ordinal`th registration, counting from 1.
    #[doc(hidden)]
    #[must_use]
    pub fn sequential_id_for_testing(ordinal: u64) -> ProcessId {
        // Version 7 in the version nibble and the RFC 4122 variant, around a
        // counter where UUIDv7 carries its random bits.
        ProcessId::from_minted(u128::from(ordinal) | (0x7_u128 << 76) | (0b10_u128 << 62))
    }

    /// Mint the id of one newly registered process.
    #[must_use]
    pub fn mint(&self) -> ProcessId {
        match self {
            Self::Random => mint_process_id(),
            Self::Sequential(counter) => Self::sequential_id_for_testing(
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
            ),
        }
    }
}

/// The idempotency key of one process start (ADR 0107).
///
/// A start key is never a reference: nothing resolves a key to a process. The
/// registrar maps a key to the process it minted for it while that process is
/// retained, so a retried start returns the retained process instead of
/// starting a second one, and after the process is pruned the same key starts
/// a new process with a new id.
///
/// A key is trusted: a retry under a retained key returns the retained process
/// whatever it submitted, without comparing or staging the retry's content.
///
/// Every key is a framed digest in one family, with each start path in its own
/// namespace, so a host-supplied key can never collide with one lash derives
/// for a model, orchestration or trigger start. The digest is over admitted
/// operation identity only — never over submitted content, source or compiler
/// identity, or the minted result.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct StartKey(String);

const START_KEY_DOMAIN: &str = "lash.process-start-key";
const START_KEY_PREFIX: &str = "process-start-key";
/// Version 1 of the start-key preimage grammar and rendering. A rendered key
/// is `process-start-key:v1:<namespace>:blake3:<64 hex>`; the namespace is
/// also the first tagged field of the preimage. Retired namespaces stay
/// burned.
pub const START_KEY_FAMILY_VERSION: u8 = 1;

/// The start path a key was derived for. Each has its own tag in the preimage
/// and its own name in the rendered key, so no key derived on one path can
/// equal one derived on another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartKeyNamespace {
    /// A start declared by a recorded tool intent.
    ToolIntent,
    /// The nth start of an orchestrating tool call.
    OrchestrationCall,
    /// The one start of a trigger delivery.
    TriggerDelivery,
    /// A key a host or remote caller supplied, scoped to its owner.
    Host,
    /// The nth keyless host start of one admitted scope.
    KeylessHost,
}

impl StartKeyNamespace {
    const ALL: [Self; 5] = [
        Self::ToolIntent,
        Self::OrchestrationCall,
        Self::TriggerDelivery,
        Self::Host,
        Self::KeylessHost,
    ];

    fn tag(self) -> u8 {
        match self {
            Self::ToolIntent => 1,
            Self::OrchestrationCall => 2,
            Self::TriggerDelivery => 3,
            Self::Host => 4,
            Self::KeylessHost => 5,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ToolIntent => "intent",
            Self::OrchestrationCall => "call",
            Self::TriggerDelivery => "trigger",
            Self::Host => "host",
            Self::KeylessHost => "keyless",
        }
    }
}

/// Who owns a host-supplied start key: the key is scoped to its owner, so two
/// sessions (or two host scopes) that pick the same key start two processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartKeyOwner<'a> {
    /// A host start, optionally under a host scope.
    Host { scope: Option<&'a str> },
    /// A start a session originated.
    Session { session_id: &'a crate::SessionId },
}

impl StartKeyOwner<'static> {
    /// An unscoped host start.
    pub const HOST: Self = Self::Host { scope: None };
}

/// A string that is not a start key this build derives.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{value}` is not a process start key")]
pub struct InvalidStartKey {
    value: String,
}

impl StartKey {
    fn derive(
        namespace: StartKeyNamespace,
        write: impl FnOnce(&mut crate::stable_identity::IdentityEncoder),
    ) -> Self {
        let mut identity = crate::stable_identity::IdentityEncoder::new(
            START_KEY_DOMAIN,
            START_KEY_FAMILY_VERSION,
        );
        identity.tag(namespace.tag());
        write(&mut identity);
        let digest = identity.finish();
        Self(format!(
            "{START_KEY_PREFIX}:v{START_KEY_FAMILY_VERSION}:{}:blake3:{}",
            namespace.name(),
            crate::stable_hash::blake3_hex("lash-stable-identity/v2", &digest)
        ))
    }

    /// The key of a start declared by a recorded tool intent: the model's
    /// `processes.start`, and every leaf that declares a start. The intent's
    /// replay key is its admitted operation identity.
    pub fn for_tool_intent(identity: &crate::ToolIntentIdentity) -> Self {
        Self::derive(StartKeyNamespace::ToolIntent, |encoder| {
            encoder.string(&identity.replay_key);
        })
    }

    /// The key of the `ordinal`th start an orchestrating tool call makes, such
    /// as `spawn_agent`: the call's admitted scope and id, and its ordinal.
    pub fn for_orchestration_call(
        scope: &crate::ExecutionScope,
        call_id: &str,
        ordinal: u32,
    ) -> Self {
        Self::derive(StartKeyNamespace::OrchestrationCall, |encoder| {
            write_scope(encoder, scope);
            encoder.string(call_id);
            encoder.u32(ordinal);
        })
    }

    /// The key of the one start a trigger delivery makes: its occurrence and
    /// the exact subscription revision the delivery was reserved against.
    pub fn for_trigger_delivery(
        occurrence_id: &str,
        subscription_id: &str,
        subscription_incarnation: &str,
        subscription_revision: u64,
    ) -> Self {
        Self::derive(StartKeyNamespace::TriggerDelivery, |encoder| {
            encoder.string(occurrence_id);
            encoder.string(subscription_id);
            encoder.string(subscription_incarnation);
            encoder.u64(subscription_revision);
        })
    }

    /// The key a host or remote caller supplies for an idempotent start:
    /// arbitrary bytes, scoped to the start's owner, in a namespace no
    /// lash-derived key shares. The same bytes under two owners are two keys.
    pub fn for_host(owner: StartKeyOwner<'_>, key: impl AsRef<[u8]>) -> Self {
        Self::derive(StartKeyNamespace::Host, |encoder| {
            match owner {
                StartKeyOwner::Host { scope } => {
                    encoder.tag(1);
                    encoder.optional(scope, |encoder, scope| encoder.string(scope));
                }
                StartKeyOwner::Session { session_id } => {
                    encoder.tag(2);
                    encoder.string(session_id);
                }
            }
            encoder.bytes(key.as_ref());
        })
    }

    /// The key of the `ordinal`th keyless host start issued under `scope`.
    ///
    /// A keyless start is always new, yet a start effect is addressed by its
    /// key and a durable handler replays it: the key is derived from the
    /// admitted scope and the start's ordinal within the run, never drawn at
    /// random, so a replay re-issues the same key.
    pub fn for_keyless_host(scope: &crate::ExecutionScope, ordinal: u32) -> Self {
        Self::derive(StartKeyNamespace::KeylessHost, |encoder| {
            write_scope(encoder, scope);
            encoder.u32(ordinal);
        })
    }

    /// Whether a start under this key must present the retained process's
    /// content. A host's key (supplied or keyless) is a claim the host makes,
    /// so a retry under it with different content is a conflict; a key lash
    /// derives from an admitted operation is trusted and returns the retained
    /// process whatever the retry submitted.
    pub fn fences_content(&self) -> bool {
        matches!(
            self.namespace(),
            Some(StartKeyNamespace::Host | StartKeyNamespace::KeylessHost)
        )
    }

    fn namespace(&self) -> Option<StartKeyNamespace> {
        let rest = self
            .0
            .strip_prefix(START_KEY_PREFIX)?
            .strip_prefix(":v")?
            .strip_prefix(&START_KEY_FAMILY_VERSION.to_string())?
            .strip_prefix(':')?;
        let (name, _) = rest.split_once(':')?;
        StartKeyNamespace::ALL
            .into_iter()
            .find(|namespace| namespace.name() == name)
    }

    /// Parse a stored or wire start key.
    ///
    /// # Errors
    ///
    /// [`InvalidStartKey`] for anything that is not a rendered key of this
    /// family and version.
    pub fn parse(value: &str) -> Result<Self, InvalidStartKey> {
        let candidate = Self(value.to_string());
        let valid = candidate.namespace().is_some_and(|namespace| {
            value
                .strip_prefix(&format!(
                    "{START_KEY_PREFIX}:v{START_KEY_FAMILY_VERSION}:{}:blake3:",
                    namespace.name()
                ))
                .is_some_and(|hex| {
                    hex.len() == 64
                        && hex
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        });
        if valid {
            Ok(candidate)
        } else {
            Err(InvalidStartKey {
                value: value.to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An admitted execution scope in a start-key preimage.
fn write_scope(
    encoder: &mut crate::stable_identity::IdentityEncoder,
    scope: &crate::ExecutionScope,
) {
    let (kind, session_id): (u8, Option<&crate::SessionId>) = match scope {
        crate::ExecutionScope::Turn { session_id, .. } => (1, Some(session_id)),
        crate::ExecutionScope::Process { .. } => (2, None),
        crate::ExecutionScope::QueueDrain { session_id, .. } => (3, Some(session_id)),
        crate::ExecutionScope::SessionDelete { session_id } => (4, Some(session_id)),
        crate::ExecutionScope::RuntimeOperation { .. } => (5, None),
    };
    encoder.tag(kind);
    encoder.optional(session_id, |encoder, session_id| encoder.string(session_id));
    encoder.string(scope.id());
}

impl<'de> Deserialize<'de> for StartKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for StartKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Read the process a handle record names, through the one handle parser
/// (ADR 0095): tools that take a process handle as an argument parse it here,
/// so a handle argument and a handle the runtime minted are read by the same
/// code.
///
/// # Errors
///
/// A message naming why the value is not a live process handle: not a handle
/// record, a handle whose id does not decode (a retired spelling or an id no
/// registrar minted), or a handle of another kind.
pub fn process_id_from_handle_json(handle: &serde_json::Value) -> Result<ProcessId, String> {
    let handle = lash_sansio::handle::parse_handle_json(handle)
        .ok_or_else(|| "Invalid process handle".to_string())?;
    let target = handle.target().ok_or_else(|| {
        "Invalid process handle: unreadable id (a retired spelling, or an id no registrar minted)"
            .to_string()
    })?;
    let lash_sansio::handle::HandleTarget::Process { process_id } = target else {
        return Err("Invalid process handle: not a process".to_string());
    };
    Ok(process_id)
}

/// A process id for a test fixture that names no registered process.
///
/// Deterministic in `label`, and a well-formed minted spelling, so a fixture
/// can refer to an unknown process or build a record by hand. Registration
/// never takes one: the registrar mints every registered id.
#[cfg(any(test, feature = "testing"))]
pub fn process_id_for_test(label: &str) -> ProcessId {
    ProcessId::fixture(label)
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
/// in the invocation delivered with a process wake. Version 4 drops the
/// process incarnation: a minted process id names one process (ADR 0107).
pub const PROCESS_WAKE_DELIVERY_FORMAT_VERSION: u32 = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessWakeDelivery {
    pub version: u32,
    pub wake_id: String,
    pub target_session_id: SessionId,
    pub process_id: ProcessId,
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
