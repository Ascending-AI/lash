//! Durable process identity and lifecycle vocabulary.

/// version_surface = "coexist"
/// version_guard(items(LASH_STABLE_IDENTITY_DOMAIN_VERSION, derive))
const LASH_STABLE_IDENTITY_DOMAIN_VERSION: &str = "lash-stable-identity/v2";

/// version_surface = "coexist"
/// version_guard(items(PROCESS_ENV_PREFIX_VERSION, process_execution_env_ref_for_bytes))
const PROCESS_ENV_PREFIX_VERSION: &str = "process-env:v6:blake3:";

/// version_surface = "coexist"
/// version_guard(items(LASH_PROCESS_ENV_DOMAIN_VERSION, process_execution_env_ref_for_bytes))
const LASH_PROCESS_ENV_DOMAIN_VERSION: &str = "lash-process-env/v6";

use crate::ProcessId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Mint the id of one newly registered process.
///
/// Only the process registrar calls this, inside the transaction that
/// registers the process. The id is a UUIDv7 — time-ordered with 74 random
/// bits — and is never reused, so it is the one identity a process has
/// (ADR 0107). Minting reads the clock and the random source; the durable
/// shift never calls it, it reads the id back off the start's recorded result.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "Uuid::now_v7 always returns a version-7 UUID with the RFC variant"
)]
pub fn mint_process_id() -> ProcessId {
    ProcessId::minted(
        lash_sansio::identity::ProcessIdRegistrar::REGISTRAR,
        uuid::Uuid::now_v7().as_u128(),
    )
    .expect("UUIDv7 generator supplies valid process-id bits")
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
    #[expect(
        clippy::expect_used,
        reason = "the fixture pins the version and RFC variant bits"
    )]
    pub fn sequential_id_for_testing(ordinal: u64) -> ProcessId {
        // Version 7 in the version nibble and the RFC 4122 variant, around a
        // counter where UUIDv7 carries its random bits.
        ProcessId::minted(
            lash_sansio::identity::ProcessIdRegistrar::REGISTRAR,
            (u128::from(ordinal) & ((1_u128 << 62) - 1))
                | ((u128::from(ordinal) >> 62) << 64)
                | (0x7_u128 << 76)
                | (0b10_u128 << 62),
        )
        .expect("sequential fixture supplies valid process-id bits")
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
/// A key lash derives from an admitted operation (a tool intent, an isolated
/// call) is trusted: a retry under it returns the retained process
/// whatever it submitted. A host's key (one it supplied, or its keyless
/// start's derived key) fences its start: a retry under it returns the
/// retained process only if it presents the same start, and is otherwise a
/// [`StartKeyConflict`](crate::runtime_error::RuntimeErrorCode::ProcessStartKeyConflict)
/// that names nothing but the key.
///
/// Every key is a framed digest in one family, with each start path in its own
/// namespace, so a host-supplied key can never collide with one lash derives
/// for a tool intent or isolated call. The digest is over admitted
/// operation identity only — never over submitted content, source or compiler
/// identity, or the minted result. Only [`StartKey::for_host`] is open to a
/// host; every other family is derived under a [`StartKeyDerivation`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct StartKey(String);

const START_KEY_DOMAIN: &str = "lash.process-start-key";
const START_KEY_PREFIX: &str = "process-start-key";
/// Version 1 of the start-key preimage grammar and rendering. A rendered key
/// is `process-start-key:v1:<namespace>:blake3:<64 hex>`; the namespace is
/// also the first tagged field of the preimage. Retired namespaces stay
/// burned.
///
/// version_guard(
///     items(
///         START_KEY_DOMAIN, START_KEY_PREFIX, derive, for_tool_intent, for_host,
///         for_keyless_host, for_isolated_call, write_scope,
///     ),
/// )
/// version_surface = "coexist"
pub const START_KEY_FAMILY_VERSION: u8 = 1;

/// The start path a key was derived for. Each has its own tag in the preimage
/// and its own name in the rendered key, so no key derived on one path can
/// equal one derived on another.
///
/// Tags 2 (ADR 0116) and 3 (the trigger delivery, FIG-5415) are retired; no
/// namespace may reuse them, nor the name `trigger`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartKeyNamespace {
    /// A start declared by a recorded tool intent.
    ToolIntent,
    /// A key a host or remote caller supplied: the same bytes are one key
    /// across the store set, whoever presents them.
    Host,
    /// The nth keyless host start of one admitted scope.
    KeylessHost,
    /// The one process start of an isolated tool call.
    IsolatedCall,
}

impl StartKeyNamespace {
    const ALL: [Self; 4] = [
        Self::ToolIntent,
        Self::Host,
        Self::KeylessHost,
        Self::IsolatedCall,
    ];

    fn tag(self) -> u8 {
        match self {
            Self::ToolIntent => 1,
            Self::Host => 4,
            Self::KeylessHost => 5,
            Self::IsolatedCall => 6,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ToolIntent => "intent",
            Self::Host => "host",
            Self::KeylessHost => "keyless",
            Self::IsolatedCall => "isolated",
        }
    }
}

/// The authority to derive a key in one of lash's own start families, or to
/// read a rendered key back from text.
///
/// Only lash's own start paths hold one: the tool-intent and isolated-call
/// realizations and the host rails' keyless ordinal, in the execution crate,
/// and host decoding of a record's key. Neither the `lash`
/// facade nor the runtime crate's root re-exports it, so host and plugin code
/// cannot name it: a host mints only [`StartKey::for_host`] keys, and a host
/// rail refuses any other family.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct StartKeyDerivation(());

impl StartKeyDerivation {
    /// The one authority lash's own start paths derive under.
    #[doc(hidden)]
    pub const LASH_START_PATHS: Self = Self(());

    /// The key of a start declared by a recorded tool intent: the model's
    /// `processes.start`, and every leaf that declares a start. The intent's
    /// replay key is its admitted operation identity.
    pub fn for_tool_intent(self, identity: &crate::ToolIntentIdentity) -> StartKey {
        StartKey::derive(StartKeyNamespace::ToolIntent, |encoder| {
            encoder.string(&identity.replay_key);
        })
    }

    /// The key of the `ordinal`th keyless host start issued under `scope`.
    ///
    /// A keyless start is always new, yet a start effect is addressed by its
    /// key and a durable handler replays it: the key is derived from the
    /// admitted scope and the start's ordinal within the run, never drawn at
    /// random, so a replay re-issues the same key.
    pub fn for_keyless_host(self, scope: &crate::ExecutionScope, ordinal: u32) -> StartKey {
        StartKey::derive(StartKeyNamespace::KeylessHost, |encoder| {
            write_scope(encoder, scope);
            encoder.u32(ordinal);
        })
    }

    /// The key of an isolated tool call's one process start (D04): the
    /// call's logical Run owner and its lash-minted call id, so every
    /// redelivery and replay of the call starts the same process.
    pub fn for_isolated_call(
        self,
        owner: &crate::effect_opener::EffectOpener,
        call_id: &lash_sansio::ToolCallId,
    ) -> StartKey {
        StartKey::derive(StartKeyNamespace::IsolatedCall, |encoder| {
            encoder.string(&owner.identity_encoding());
            encoder.string(call_id.as_str());
        })
    }

    /// Parse a stored or wire start key.
    ///
    /// # Errors
    ///
    /// [`InvalidStartKey`] for anything that is not a rendered key of this
    /// family and version.
    pub fn parse(self, value: &str) -> Result<StartKey, InvalidStartKey> {
        StartKey::parse_rendered(value)
    }
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
            crate::stable_hash::blake3_hex(LASH_STABLE_IDENTITY_DOMAIN_VERSION, &digest)
        ))
    }

    /// The key a host or remote caller supplies for an idempotent start:
    /// arbitrary bytes in a namespace no lash-derived key shares. Lash mixes
    /// nothing into it, so the same bytes are one key across the store set,
    /// whoever presents them (ADR 0107): the start's originator and lifetime
    /// are content the key fences, never part of the key.
    pub fn for_host(key: impl AsRef<[u8]>) -> Self {
        Self::derive(StartKeyNamespace::Host, |encoder| {
            encoder.bytes(key.as_ref());
        })
    }

    /// Whether a start under this key must present the retained process's
    /// content. A host's key (supplied or keyless) is a claim the host makes,
    /// so a retry under it with a different start is a conflict; a key lash
    /// derives from an admitted operation is trusted and returns the retained
    /// process whatever the retry submitted.
    pub fn fences_content(&self) -> bool {
        matches!(
            self.namespace(),
            Some(StartKeyNamespace::Host | StartKeyNamespace::KeylessHost)
        )
    }

    /// Whether this is a key a host supplied ([`Self::for_host`]): the only
    /// family a host rail accepts on a request.
    pub fn is_host_supplied(&self) -> bool {
        self.namespace() == Some(StartKeyNamespace::Host)
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

    pub(crate) fn parse_rendered(value: &str) -> Result<Self, InvalidStartKey> {
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
        crate::ExecutionScope::SessionOperation { session_id, .. } => (3, Some(session_id)),
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
        Self::parse_rendered(&value).map_err(serde::de::Error::custom)
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
    /// The plugin configuration the process's creator ran under, at its
    /// config revision: every runtime built for the process reads it
    /// (FIG-4379).
    pub plugin_config: crate::AdmittedPluginConfig,
    pub policy: crate::SessionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<crate::run_spec::RecordedRender>,
}
impl ProcessExecutionEnvSpec {
    /// Constructs a `ProcessExecutionEnvSpec` for protocol and process-engine implementors running a durable process.
    pub fn new(plugin_config: crate::AdmittedPluginConfig, policy: crate::SessionPolicy) -> Self {
        Self {
            plugin_config,
            policy,
            render: None,
        }
    }

    /// Content-addresses the exact bytes persisted by [`Self::to_store_bytes`].
    ///
    /// The policy contains execution settings, with the owning session named
    /// only by its runtime state. Identical captured settings therefore share
    /// an environment reference across sessions.
    /// These bytes follow the final binary's serde-json feature set. Shapes
    /// and golden vectors change in place until the 1.0 version freeze ends.
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
/// This enum has no `#[serde(other)]` fallback arm, by design: a store that
/// silently folded an unknown status into a known one would corrupt the very
/// fold the registry exists to keep honest.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}
impl ProcessStatus {
    /// Whether the row belongs to the non-terminal process partition.
    ///
    /// This is the Rust twin of the `status IN (...)` predicate every backend
    /// non-terminal page query carries, and the complement of
    /// [`ProcessStatus::is_retired`]. The match is exhaustive on purpose: a new
    /// variant must declare which side of the live/retired partition it falls
    /// on before any query can compile.
    pub fn is_live(&self) -> bool {
        match self {
            Self::Running | Self::Waiting => true,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned => false,
        }
    }

    /// Lets process-store implementors apply retention only to completed, failed, cancelled, or
    /// abandoned rows; running and waiting rows are never terminal.
    pub fn is_terminal(&self) -> bool {
        self.terminal().is_some()
    }

    /// The terminal status this is, or `None` for a row with no recorded
    /// outcome. Exhaustive on purpose: a new variant must declare whether it
    /// is terminal before anything can compile.
    pub fn terminal(&self) -> Option<TerminalProcessStatus> {
        match self {
            Self::Completed => Some(TerminalProcessStatus::Completed),
            Self::Failed => Some(TerminalProcessStatus::Failed),
            Self::Cancelled => Some(TerminalProcessStatus::Cancelled),
            Self::Abandoned => Some(TerminalProcessStatus::Abandoned),
            Self::Running | Self::Waiting => None,
        }
    }

    /// Lets process-store implementors select the rows retention may reclaim.
    ///
    /// Retention reclaims a row; it never asserts an outcome. Terminal rows
    /// qualify because their outcome is recorded.
    pub fn is_retired(&self) -> bool {
        self.retired().is_some()
    }

    /// The retired status this is, or `None` for a live row. Exhaustive on
    /// purpose: a new variant must declare which side of the live/retired
    /// partition it falls on before anything can compile.
    pub fn retired(&self) -> Option<RetiredProcessStatus> {
        match self {
            Self::Completed => Some(RetiredProcessStatus::Completed),
            Self::Failed => Some(RetiredProcessStatus::Failed),
            Self::Cancelled => Some(RetiredProcessStatus::Cancelled),
            Self::Abandoned => Some(RetiredProcessStatus::Abandoned),
            Self::Running | Self::Waiting => None,
        }
    }
}

/// The status of a process whose outcome is recorded: the terminal subset of
/// [`ProcessStatus`].
///
/// A terminal outcome derives one, so none can name a status no outcome
/// ends a process in.
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
#[serde(rename_all = "snake_case")]
pub enum TerminalProcessStatus {
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl TerminalProcessStatus {
    /// Every variant, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Abandoned,
    ];

    /// This status in the whole lifecycle vocabulary.
    pub fn status(self) -> ProcessStatus {
        match self {
            Self::Completed => ProcessStatus::Completed,
            Self::Failed => ProcessStatus::Failed,
            Self::Cancelled => ProcessStatus::Cancelled,
            Self::Abandoned => ProcessStatus::Abandoned,
        }
    }

    /// The durable spelling, shared with [`ProcessStatus::label`].
    pub fn label(self) -> &'static str {
        self.status().label()
    }
}

impl From<TerminalProcessStatus> for ProcessStatus {
    fn from(status: TerminalProcessStatus) -> Self {
        status.status()
    }
}

impl fmt::Display for TerminalProcessStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The status a process row had when retention reclaimed it: the retired
/// subset of [`ProcessStatus`].
///
/// A tombstone and every "no longer retained" answer carry one, so a pruned
/// process still says how it ended: a `failed` one is never reported as
/// completed.
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
#[serde(rename_all = "snake_case")]
pub enum RetiredProcessStatus {
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl RetiredProcessStatus {
    /// Every variant, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Abandoned,
    ];

    /// This status in the whole lifecycle vocabulary.
    pub fn status(self) -> ProcessStatus {
        match self {
            Self::Completed => ProcessStatus::Completed,
            Self::Failed => ProcessStatus::Failed,
            Self::Cancelled => ProcessStatus::Cancelled,
            Self::Abandoned => ProcessStatus::Abandoned,
        }
    }

    /// The durable spelling, shared with [`ProcessStatus::label`].
    pub fn label(self) -> &'static str {
        self.status().label()
    }

    /// The single reader of a tombstone's stored label: an unrecognised or
    /// live label is `None`, never a default.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.label() == label)
    }
}

impl From<RetiredProcessStatus> for ProcessStatus {
    fn from(status: RetiredProcessStatus) -> Self {
        status.status()
    }
}

impl From<TerminalProcessStatus> for RetiredProcessStatus {
    fn from(status: TerminalProcessStatus) -> Self {
        match status {
            TerminalProcessStatus::Completed => Self::Completed,
            TerminalProcessStatus::Failed => Self::Failed,
            TerminalProcessStatus::Cancelled => Self::Cancelled,
            TerminalProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl fmt::Display for RetiredProcessStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
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
        "{PROCESS_ENV_PREFIX_VERSION}{}",
        crate::stable_hash::blake3_hex(LASH_PROCESS_ENV_DOMAIN_VERSION, bytes)
    ))
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
});

impl crate::store::DurableRecord for StartKey {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::artifact_referrer::ARTIFACT_REFERRER_KINDS_VERSION);
}

impl crate::store::DurableRecord for StartKeyNamespace {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::process_identity::START_KEY_FAMILY_VERSION);
}

#[cfg(test)]
#[path = "process_identity_tests.rs"]
mod tests;
