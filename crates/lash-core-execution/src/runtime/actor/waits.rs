//! Waits, completion keys, `await_external` and `await_process`
//! (ADR 0132 §6, §11; S5 of I0, FIG-5194). Owned by L5 (FIG-5173).
//!
//! # Contracts
//!
//! - A wait's deadline is written once, at minting, and never refreshed.
//! - The first resolution wins; a repeat with the same digest answers
//!   `AlreadyResolved`, with another `Conflict`.
//! - A host resolve refuses every kind but `tool_completion` and `custom`
//!   with `ReservedKind` and writes nothing.
//! - Lock order is the wait row, then the actor row.
//! - A durable backend built without completion secrets is refused
//!   (`DurableBuildError::MissingCompletionSecrets`).
//! - Every await races the awaiter's own cancel mail; there is no
//!   `HandedOver`: failover keeps the same row, key and deadline.

use std::collections::BTreeMap;
use std::time::Duration;

use lash_durable::domain::ScopeKey;
use lash_durable::{ActorTx, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::ActorContext;
use crate::{Backend, ProcessId, ProcessOutcome};

pub use lash_core_effect::Resolution;
pub use lash_durable::domain::{KeyVersion, ResolveAnswer, WaitId, WaitKind};

/// A host-resolvable wait's key: `wk1.<wait_id>.<mac>`, where the MAC is
/// HMAC-SHA256 under the deployment's completion secret of the row's key
/// version over `"wk1" ‖ wait_id ‖ kind`. It carries no scope or kind in
/// plaintext.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedKey(String);

impl PinnedKey {
    /// A key as minted.
    #[must_use]
    pub fn new(key: String) -> Self {
        Self(key)
    }

    /// The key a host is handed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PinnedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PinnedKey(..)")
    }
}

/// One completion secret's bytes. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Wrap secret bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The bytes, for the MAC.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretBytes(..)")
    }
}

/// The deployment's completion secrets: one or more versions, one current.
/// New keys are minted under the current version; a live wait verifies under
/// its stored version for as long as that version is configured.
#[derive(Clone, PartialEq, Eq)]
pub struct CompletionKeySecrets {
    current: KeyVersion,
    secrets: BTreeMap<KeyVersion, SecretBytes>,
}

impl std::fmt::Debug for CompletionKeySecrets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompletionKeySecrets")
            .field("current", &self.current)
            .field("versions", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Refused completion secrets.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretsRefusal {
    /// The current version has no secret.
    #[error("the current key version {0:?} has no secret")]
    CurrentMissing(KeyVersion),
    /// A version is configured twice.
    #[error("key version {0:?} is configured twice")]
    DuplicateVersion(KeyVersion),
    /// A secret is too short to key an HMAC-SHA256 safely.
    #[error("the secret of key version {0:?} is too short")]
    TooShort(KeyVersion),
}

impl CompletionKeySecrets {
    /// Validate `secrets`, with `current` the version new keys mint under.
    ///
    /// # Errors
    ///
    /// [`SecretsRefusal`].
    pub fn new(
        _current: KeyVersion,
        _secrets: Vec<(KeyVersion, SecretBytes)>,
    ) -> Result<Self, SecretsRefusal> {
        todo!("L5 (FIG-5173): validate versioned completion secrets")
    }

    /// One fixed 32-byte secret under version 1, for tests: never a
    /// deployment's.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing() -> Self {
        Self {
            current: KeyVersion(1),
            secrets: BTreeMap::from([(KeyVersion(1), SecretBytes::new(vec![7; 32]))]),
        }
    }

    /// The version new keys mint under.
    #[must_use]
    pub fn current(&self) -> KeyVersion {
        self.current
    }

    /// The secret of `version`, if it is configured.
    #[must_use]
    pub fn secret(&self, version: KeyVersion) -> Option<&SecretBytes> {
        self.secrets.get(&version)
    }
}

/// A wait's deadline: written once, at minting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WaitDeadline(DurableInstant);

/// A refused wait deadline.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitDeadlineRefusal {
    /// The request is above the ceiling.
    #[error("a wait of {requested:?} is above the ceiling {ceiling:?}")]
    AboveCeiling {
        /// The requested wait.
        requested: Duration,
        /// The ceiling.
        ceiling: Duration,
    },
    /// The deadline does not fit a durable instant.
    #[error("a wait of {0:?} does not fit a durable instant")]
    Unrepresentable(Duration),
}

impl WaitDeadline {
    /// The deadline a wait of `requested` (or `default`) minted at `now`
    /// gets, refused above `ceiling`. Callers pass `ExecutionBudgets`'
    /// `wait_default` and `wait_ceiling`; a nested wait keeps its own
    /// deadline.
    ///
    /// # Errors
    ///
    /// [`WaitDeadlineRefusal`].
    pub fn resolve(
        _requested: Option<Duration>,
        _default: Duration,
        _ceiling: Duration,
        _now: DurableInstant,
    ) -> Result<Self, WaitDeadlineRefusal> {
        todo!("L5 (FIG-5173): resolve a wait deadline under its default and ceiling")
    }

    /// The deadline.
    #[must_use]
    pub fn at(self) -> DurableInstant {
        self.0
    }
}

/// What to pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    /// What it waits for.
    pub kind: WaitKind,
    /// The scope that revokes it.
    pub scope: ScopeKey,
    /// For a process-terminal wait, the process.
    pub target_process: Option<ProcessId>,
    /// Its deadline.
    pub deadline: Option<WaitDeadline>,
}

/// A pinned wait, as its owner holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WaitRef {
    id: WaitId,
    kind: WaitKind,
}

impl WaitRef {
    /// The wait `id` of `kind`.
    #[must_use]
    pub fn new(id: WaitId, kind: WaitKind) -> Self {
        Self { id, kind }
    }

    /// Its identity.
    #[must_use]
    pub fn id(&self) -> WaitId {
        self.id
    }

    /// What it waits for.
    #[must_use]
    pub fn kind(&self) -> WaitKind {
        self.kind
    }
}

/// A refused pin; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PinRefusal {
    /// A process-terminal wait names no process, or another kind names one.
    #[error("a {0:?} wait's target process does not match its kind")]
    TargetMismatch(WaitKind),
}

/// Which of a race's waits won.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaceWinner {
    /// The wait resolved.
    Resolved {
        /// The wait.
        wait: WaitRef,
        /// Its resolution.
        resolution: Resolution,
    },
    /// The wait's deadline passed first.
    TimedOut(WaitRef),
    /// The awaiter's own cancel mail came first.
    Cancelled,
}

/// How an [`await_external`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalWaitOutcome {
    /// The host resolved the key.
    Resolved(Resolution),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// How an [`await_process`] ended.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessWaitOutcome {
    /// The process ended.
    Resolved(ProcessOutcome),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// Mint a wait on `tx`: insert its row pending, and for a host-resolvable
/// kind mint its key under `secrets`' current version. The key exists only
/// once `tx` commits, before any step that submits it.
///
/// # Errors
///
/// [`PinRefusal`]; nothing is recorded.
pub fn pin(
    _tx: &mut ActorTx,
    _secrets: &CompletionKeySecrets,
    _spec: WaitSpec,
) -> Result<(WaitRef, Option<PinnedKey>), PinRefusal> {
    todo!("L5 (FIG-5173): mint a wait row and its HMAC key in the owner's transaction")
}

/// A host's resolve of `key`: parse it, verify its MAC in constant time under
/// its row's key version, refuse a reserved kind, then resolve from
/// `pending` and wake the owner, in one mailbox transaction.
///
/// # Errors
///
/// A store failure.
pub async fn resolve_host(
    _backend: &Backend,
    _key: &str,
    _resolution: Resolution,
) -> Result<ResolveAnswer, DurableError> {
    todo!("L5 (FIG-5173): verify a host key and resolve its wait, first winner")
}

/// Wait until one of `waits` resolves or times out, raced against the
/// awaiter's own cancel mail. Suspension is a state: with nothing runnable
/// the activation commits and releases as `waiting`.
///
/// # Errors
///
/// A store failure; [`DurableError::OwnershipLost`].
pub async fn race(_cx: &ActorContext, _waits: &[WaitRef]) -> Result<RaceWinner, DurableError> {
    todo!("L5 (FIG-5173): race pinned waits against the awaiter's cancel mail")
}

/// Wait on a pinned host-resolvable wait.
pub async fn await_external(_cx: &ActorContext, _wait: &WaitRef) -> ExternalWaitOutcome {
    todo!("L5 (FIG-5173): await a host-resolvable wait, bounded by its deadline")
}

/// Wait for `process` to end, bounded by `deadline`, raced against the
/// awaiter's cancel mail, so a cycle of waits is cancellable.
pub async fn await_process(
    _cx: &ActorContext,
    _process: &ProcessId,
    _deadline: WaitDeadline,
) -> ProcessWaitOutcome {
    todo!("L5 (FIG-5173): await a process terminal, bounded and cancellable")
}

/// Resolve every pending process-terminal wait on `process` with `outcome`
/// in its terminal transaction, and wake each owner. Called by L6.
pub fn resolve_process_terminal_waits(
    _tx: &mut ActorTx,
    _process: &ProcessId,
    _outcome: &ProcessOutcome,
) {
    todo!("L5 (FIG-5173): resolve a process's terminal waits in its terminal transaction")
}

/// Revoke every pending wait of `scope`. Called by L6 and L6b.
pub fn revoke_scope(_tx: &mut ActorTx, _scope: &ScopeKey) {
    todo!("L5 (FIG-5173): revoke a scope's pending waits")
}

/// The wait methods of the context: what the deleted await-event resolver
/// and the effect host's wait index became. L5 (FIG-5173) replaces each by
/// `pin`, `race`, `resolve_host` or `revoke_scope`, or deletes it where an
/// await-event identity hash keyed it.
impl ActorContext {
    /// The wait effects: `Sleep` (a timer wait), `AwaitEvent` and
    /// `PeekAwaitEvent` (a pinned wait raced against cancel). Any other
    /// command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn wait_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L5 (FIG-5173): run a wait effect as a pinned wait or timer, raced against cancel")
    }

    /// The identity of the authority that mints this context's keys.
    #[must_use]
    pub fn await_event_authority_binding_id(&self) -> Option<String> {
        todo!("L5 (FIG-5173): delete with await-event keys; wait keys are HMAC-minted")
    }

    /// Prepare the completion key of a wait.
    ///
    /// # Errors
    ///
    /// The key's refusal.
    pub async fn prepare_completion_key(
        &self,
        _scope: &crate::ExecutionScope,
        _wait: crate::AwaitEventWaitIdentity,
        _may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        todo!("L5 (FIG-5173): mint a completion key by pin")
    }

    /// The await-event key of a wait.
    ///
    /// # Errors
    ///
    /// The key's refusal.
    pub async fn await_event_key(
        &self,
        _scope: &crate::ExecutionScope,
        _wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        todo!("L5 (FIG-5173): mint a completion key by pin")
    }

    /// Resolve a wait by its key.
    ///
    /// # Errors
    ///
    /// The resolution's refusal.
    pub async fn resolve_await_event(
        &self,
        _key: &crate::AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        todo!("L5 (FIG-5173): resolve a wait through resolve_host")
    }

    /// Publish a resolution of a wait by its key.
    ///
    /// # Errors
    ///
    /// The resolution's refusal.
    pub async fn publish_await_event(
        &self,
        _key: &crate::AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<Option<crate::ResolveOutcome>, crate::RuntimeError> {
        todo!("L5 (FIG-5173): resolve a wait through resolve_host")
    }

    /// Read a wait's resolution without waiting.
    ///
    /// # Errors
    ///
    /// The read's refusal.
    pub async fn peek_await_event(
        &self,
        _key: &crate::AwaitEventKey,
    ) -> Result<Option<Resolution>, crate::RuntimeError> {
        todo!("L5 (FIG-5173): read a wait row")
    }

    /// Wait for a wait's resolution.
    ///
    /// # Errors
    ///
    /// The wait's refusal.
    pub async fn await_await_event(
        &self,
        _key: &crate::AwaitEventKey,
        _cancel: CancellationToken,
    ) -> Result<Resolution, crate::RuntimeError> {
        todo!("L5 (FIG-5173): await a wait through race")
    }

    /// Revoke a session's waits.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn revoke_await_events_for_session(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L5 (FIG-5173): revoke a session's waits through revoke_scope")
    }

    /// Cancel a session's waits.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn cancel_await_events_for_session(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L5 (FIG-5173): revoke a session's waits through revoke_scope")
    }

    /// Retire a scope's waits.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn retire_await_events_for_scope(
        &self,
        _scope: &crate::ExecutionScope,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L5 (FIG-5173): revoke a scope's waits through revoke_scope")
    }

    /// Retire a scope's waits if none is pending.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn retire_await_events_for_scope_if_quiescent(
        &self,
        _scope: &crate::ExecutionScope,
    ) -> Result<bool, crate::RuntimeError> {
        todo!("L5 (FIG-5173): revoke a scope's waits through revoke_scope")
    }

    /// Lift a scope's retirement fence for a re-registered owner.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn reinstate_await_event_scope(
        &self,
        _scope: &crate::ExecutionScope,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L5 (FIG-5173): delete with scope retirement fences; revocation is per wait row")
    }

    /// Whether a scope's waits are retired.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn await_event_scope_is_retired(
        &self,
        _scope: &crate::ExecutionScope,
    ) -> Result<bool, crate::RuntimeError> {
        todo!("L5 (FIG-5173): delete with scope retirement fences; revocation is per wait row")
    }

    /// Release a terminal run's waits.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn retire_closed_run_waits(
        &self,
        _session_id: &crate::SessionId,
        _run: &crate::TurnId,
        _committed_turn: Option<&crate::TurnId>,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L5 (FIG-5173): revoke a closed run's waits through revoke_scope")
    }

    /// The unresolved host-resolvable waits of a session.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn list_outstanding_await_event_keys(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Vec<crate::AwaitEventKey>, crate::RuntimeError> {
        todo!("L5 (FIG-5173): list a session's pending host-resolvable waits")
    }
}
