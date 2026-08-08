//! The durable AwaitEvent promise coordinator shared by every SQL backend.
//!
//! [`promise_semantics`] owns the pure transition table; this module owns the
//! *orchestration* around it that durable adapters were duplicating: HMAC key
//! minting and authentication, the persisted-identity projection, resolution
//! encoding, the poll/notifier waiter loop, and revoke/cancel semantics.
//!
//! Backends implement only [`AwaitEventBackend`]: the storage atoms
//! (`ensure_pending`, compare-and-store, inspect, session revocation, session
//! cancel sweep, tombstone read) plus whatever transaction and locking
//! mechanics that substrate needs to make each atom atomic. SQL, transaction
//! shape, and lock choice stay in the store crate; nothing that decides a
//! durable promise outcome does.
//!
//! The rows are the source of truth. The notifier map is a latency hint only:
//! it lives in one process, so a waiter attached to a different host object
//! must still observe a peer's resolution, and every waiter therefore polls
//! persisted state with bounded backoff regardless of notifications.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::executor::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome,
};
use super::promise_semantics::{self, PromiseState};

type HmacSha256 = Hmac<sha2::Sha256>;

/// First poll delay, and the delay a notification resets the waiter to.
const INITIAL_POLL: Duration = Duration::from_millis(25);
/// Ceiling for the waiter's exponential poll backoff.
const MAX_POLL: Duration = Duration::from_secs(1);

/// Backend-specific error vocabulary for coordinator-owned failures.
///
/// Hosts match on `RuntimeError::code`, so each backend supplies typed codes
/// for the four coordinator failures it can emit. Bundled backends use built-in
/// variants; external backends can deliberately use namespaced values from
/// [`RuntimeErrorCode::from_wire_code`]. Storage failures stay in the backend,
/// which owns its own `_store` mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitEventVocabulary {
    /// Signer initialization failure.
    pub sign: RuntimeErrorCode,
    /// State encoding failure.
    pub encode: RuntimeErrorCode,
    /// Persisted state decoding failure.
    pub decode: RuntimeErrorCode,
    /// In-process notifier failure.
    pub notify: RuntimeErrorCode,
    /// Human-readable backend name used in error messages, e.g. `"SQLite"`.
    pub display_name: &'static str,
}

/// The persisted projection of an [`AwaitEventKey`]'s identity.
///
/// Backends store these columns beside the promise and compare all four before
/// honoring any operation: a row whose identity disagrees with the presented
/// key is not that key's promise, so the operation takes the non-oracular
/// unknown/revoked result rather than reading or overwriting someone else's
/// terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitEventRowIdentity {
    /// Canonical JSON of the key's [`ExecutionScope`].
    pub scope_json: String,
    /// Canonical JSON of the key's [`AwaitEventWaitIdentity`].
    pub wait_json: String,
    /// Owning session, when the scope has one. `NULL` rows are session-free.
    pub session_id: Option<String>,
    /// Whether this promise is turn-control (gate or terminal) machinery.
    pub turn_control: bool,
}

/// A terminal payload rendered as its byte length only.
///
/// Encoded terminals carry resolution payloads — tool results and await-event
/// values — so they are redacted wherever they would otherwise reach a log, a
/// panic message, or an assertion failure.
struct RedactedTerminal<'a>(&'a str);

impl std::fmt::Debug for RedactedTerminal<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "<redacted, {} bytes>", self.0.len())
    }
}

/// Persisted promise state observed by [`AwaitEventBackend::inspect`].
///
/// The terminal stays encoded: decoding is the coordinator's job so both
/// backends report the same decode-failure vocabulary.
#[derive(Clone, PartialEq, Eq)]
pub enum PersistedPromise {
    /// No row exists for this key yet.
    Missing,
    /// A row exists with a matching identity and no terminal.
    Pending,
    /// A row exists with a matching identity and a winning terminal.
    Resolved {
        /// The encoded winning terminal.
        terminal_json: String,
    },
    /// The session is tombstoned, or a row with a different identity owns the
    /// key id.
    UnknownOrRevoked,
}

impl std::fmt::Debug for PersistedPromise {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("Missing"),
            Self::Pending => formatter.write_str("Pending"),
            Self::Resolved { terminal_json } => formatter
                .debug_struct("Resolved")
                .field("terminal_json", &RedactedTerminal(terminal_json))
                .finish(),
            Self::UnknownOrRevoked => formatter.write_str("UnknownOrRevoked"),
        }
    }
}

/// Outcome of a backend's fenced first-writer-wins terminal write.
///
/// The write predicate is exactly [`promise_semantics::resolve`]'s `Store`
/// arm — write when the promise is missing or pending — and the coordinator
/// maps the reported observation back through the same transition table to
/// produce the public [`ResolveOutcome`].
#[derive(Clone, PartialEq, Eq)]
pub enum TerminalCas {
    /// The proposed terminal won and is now durable.
    Stored,
    /// A terminal already won; nothing was written.
    AlreadyResolved {
        /// The encoded terminal that remains authoritative.
        terminal_json: String,
    },
    /// The session is tombstoned, or a row with a different identity owns the
    /// key id.
    UnknownOrRevoked,
}

impl std::fmt::Debug for TerminalCas {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stored => formatter.write_str("Stored"),
            Self::AlreadyResolved { terminal_json } => formatter
                .debug_struct("AlreadyResolved")
                .field("terminal_json", &RedactedTerminal(terminal_json))
                .finish(),
            Self::UnknownOrRevoked => formatter.write_str("UnknownOrRevoked"),
        }
    }
}

/// Storage atoms a durable substrate must provide to run keyed promises.
///
/// Every method is one atomic unit on the substrate: the backend takes
/// whatever transaction and lock it needs (SQLite's `BEGIN IMMEDIATE` write
/// lock, PostgreSQL's per-session advisory transaction lock) so that the
/// tombstone check, the identity comparison, and the write it guards cannot
/// interleave. No method decides a promise outcome, mints or authenticates a
/// key, or notifies a waiter.
#[async_trait]
pub trait AwaitEventBackend: Send + Sync {
    /// The error vocabulary hosts already match on for this backend.
    fn vocabulary(&self) -> AwaitEventVocabulary;

    /// Whether `session_id` carries a durable revocation tombstone.
    ///
    /// Read outside any promise fence: it guards key minting only, where there
    /// is no row to race with.
    async fn session_is_revoked(&self, session_id: &str) -> Result<bool, RuntimeError>;

    /// Materialize the promise row so a waiter is durably registered.
    ///
    /// Atomically: reject (`false`) when the owning session is tombstoned or a
    /// row with a different identity already owns `key_id`; otherwise insert a
    /// terminal-free row if none exists and accept (`true`). An existing
    /// matching row — pending or resolved — is accepted unchanged.
    async fn ensure_pending(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        now_ms: u64,
    ) -> Result<bool, RuntimeError>;

    /// Compare-and-store `terminal_json` as the promise's first terminal.
    ///
    /// Atomically: reject when the owning session is tombstoned or a row with a
    /// different identity owns `key_id`; write and report
    /// [`TerminalCas::Stored`] when no row exists or the matching row has no
    /// terminal; otherwise report the existing terminal without writing.
    async fn store_terminal(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<TerminalCas, RuntimeError>;

    /// Read the persisted promise state for `key_id` under the same fence, so
    /// the tombstone check and the row read agree with each other.
    async fn inspect(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
    ) -> Result<PersistedPromise, RuntimeError>;

    /// Tombstone `session_id` and drop its promise rows in one atom.
    ///
    /// The tombstone is idempotent and must outlive the rows: a host that
    /// reopens the substrate has to keep rejecting keys minted for a deleted
    /// session.
    async fn revoke_session(&self, session_id: &str, now_ms: u64) -> Result<(), RuntimeError>;

    /// Sweep every unresolved non-turn-control promise of `session_id` to
    /// `terminal_json`.
    ///
    /// This is `promise_semantics::cancel_sweep` applied set-wise: existing
    /// terminals are immutable and turn-control promises are never swept, so
    /// cancelling observation cannot manufacture a turn cancellation or
    /// terminal publication. The session stays usable afterwards.
    async fn cancel_session_promises(
        &self,
        session_id: &str,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<(), RuntimeError>;
}

/// The durable keyed-promise state machine, shared by every SQL backend.
///
/// One coordinator instance is one host object: it owns that host's signing
/// secret, clock, and local notifier map, and delegates all persistence to its
/// [`AwaitEventBackend`]. Cloning shares the notifier map, so clones are the
/// same host for wake purposes; separately constructed coordinators over one
/// substrate are independent hosts and reach each other only through the rows.
#[derive(Clone)]
pub struct AwaitEventCoordinator<B> {
    backend: B,
    signing_secret: Arc<[u8]>,
    clock: Arc<dyn crate::Clock>,
    notifiers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl<B: AwaitEventBackend> AwaitEventCoordinator<B> {
    /// Build a coordinator over `backend`.
    ///
    /// `signing_secret` is the substrate's persisted await-event secret: two
    /// hosts reading the same secret mint byte-identical keys, which is what
    /// makes a key usable after a reopen. `clock` stamps durable rows and
    /// drives the waiter's poll backoff.
    pub fn new(backend: B, signing_secret: Arc<[u8]>, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            backend,
            signing_secret,
            clock,
            notifiers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Mint the authenticated key for `scope`/`wait`.
    ///
    /// Minting is a pure read: it never registers a promise row. A tombstoned
    /// session refuses to mint at all, so a key created after session deletion
    /// cannot be smuggled past the resolve path.
    pub async fn key_for(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        let key_id = promise_semantics::derive_key_id(scope, &wait)?;
        if let Some(session_id) = scope.session_id()
            && self.backend.session_is_revoked(session_id).await?
        {
            return Err(unknown_or_revoked());
        }
        let signature = self.signature(scope, &wait, &key_id)?;
        Ok(AwaitEventKey {
            scope: scope.clone(),
            wait,
            key_id,
            signature,
        })
    }

    /// Publish `resolution` as the promise's terminal, first writer wins.
    ///
    /// A missing promise accepts the terminal so a signal that arrives before
    /// its waiter is buffered rather than lost.
    pub async fn resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !self.authenticates(key)? {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        let identity = self.row_identity(key)?;
        let terminal_json = self.encode_resolution(&resolution)?;
        let cas = self
            .backend
            .store_terminal(
                &key.key_id,
                &identity,
                &terminal_json,
                self.clock.timestamp_ms(),
            )
            .await?;
        let observed = match cas {
            TerminalCas::Stored => {
                self.notify_key(&key.key_id)?;
                return Ok(ResolveOutcome::Accepted);
            }
            TerminalCas::AlreadyResolved { terminal_json } => {
                PromiseState::Resolved(self.decode_resolution(&terminal_json)?)
            }
            TerminalCas::UnknownOrRevoked => PromiseState::Revoked,
        };
        Ok(promise_semantics::resolve(observed, resolution)
            .resolve_outcome()
            .expect("resolve never returns the unchanged transition"))
    }

    /// Read the terminal without registering a waiter.
    ///
    /// `None` covers both "no row yet" and "pending": neither is oracular
    /// about whether a promise was ever created.
    pub async fn peek(&self, key: &AwaitEventKey) -> Result<Option<Resolution>, RuntimeError> {
        match self.inspect(key).await? {
            PromiseState::Missing | PromiseState::Pending => Ok(None),
            PromiseState::Resolved(terminal) => Ok(Some(terminal)),
            PromiseState::Revoked => Err(unknown_or_revoked()),
        }
    }

    /// Wait for the terminal using this host's clock.
    pub async fn await_resolution(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        let clock = Arc::clone(&self.clock);
        self.await_resolution_with_clock(key, cancel, deadline, clock.as_ref())
            .await
    }

    /// Wait for the terminal, polling on a caller-supplied clock.
    ///
    /// The waiter registers durably first, then loops on persisted state:
    /// notifications only shorten the wait. Cancellation and deadline expiry
    /// publish `Cancelled`/`Timeout` for ordinary promises, but turn-control
    /// promises refuse to — an observer stopping must not manufacture a turn
    /// cancellation or terminal — and report the `turn_control_wait_*` error
    /// instead.
    pub async fn await_resolution_with_clock(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        clock: &dyn crate::Clock,
    ) -> Result<Resolution, RuntimeError> {
        let notify = self.notifier_for(&key.key_id)?;
        self.ensure_pending(key).await?;
        let mut backoff = INITIAL_POLL;
        loop {
            match self.inspect(key).await? {
                PromiseState::Resolved(terminal) => return Ok(terminal),
                PromiseState::Revoked => return Err(unknown_or_revoked()),
                PromiseState::Missing | PromiseState::Pending => {}
            }

            let poll = clock.sleep(backoff);
            tokio::pin!(poll);
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitCancelled,
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled).await?;
                    }
                    _ = clock.sleep_until(deadline) => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitTimeout,
                                "turn-control waiter timed out without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Timeout).await?;
                    }
                    _ = notify.notified() => {
                        backoff = INITIAL_POLL;
                        continue;
                    }
                    _ = &mut poll => {}
                }
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitCancelled,
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled).await?;
                    }
                    _ = notify.notified() => {
                        backoff = INITIAL_POLL;
                        continue;
                    }
                    _ = &mut poll => {}
                }
            }
            backoff = (backoff * 2).min(MAX_POLL);
        }
    }

    /// Tombstone `session_id`: in-flight waiters fail with
    /// `await_event_unknown_or_revoked`, its promise rows are dropped, and keys
    /// minted for it stay unusable across reopen. This is session deletion, in
    /// contrast to the recoverable [`cancel_session`](Self::cancel_session).
    pub async fn revoke_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        validate_session_id(session_id)?;
        self.backend
            .revoke_session(session_id, self.clock.timestamp_ms())
            .await?;
        self.notify_all()
    }

    /// Resolve every *outstanding* ordinary wait of `session_id` with
    /// [`Resolution::Cancelled`], leaving the session usable: already-terminal
    /// promises keep their terminal, turn-control promises are untouched, and
    /// promises registered afterwards behave normally.
    pub async fn cancel_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        validate_session_id(session_id)?;
        let terminal_json = self.encode_resolution(&Resolution::Cancelled)?;
        self.backend
            .cancel_session_promises(session_id, &terminal_json, self.clock.timestamp_ms())
            .await?;
        self.notify_all()
    }

    /// Durably register the caller as a waiter before it starts polling, so a
    /// resolver that arrives while the waiter sleeps has a row to write.
    async fn ensure_pending(&self, key: &AwaitEventKey) -> Result<(), RuntimeError> {
        if !self.authenticates(key)? {
            return Err(unknown_or_revoked());
        }
        let identity = self.row_identity(key)?;
        if self
            .backend
            .ensure_pending(&key.key_id, &identity, self.clock.timestamp_ms())
            .await?
        {
            Ok(())
        } else {
            Err(unknown_or_revoked())
        }
    }

    /// Project persisted state onto the transition table's [`PromiseState`].
    ///
    /// An unauthenticated key reports `Revoked` without touching the substrate:
    /// a forged key must not be able to learn whether a promise exists.
    async fn inspect(&self, key: &AwaitEventKey) -> Result<PromiseState, RuntimeError> {
        if !self.authenticates(key)? {
            return Ok(PromiseState::Revoked);
        }
        let identity = self.row_identity(key)?;
        match self.backend.inspect(&key.key_id, &identity).await? {
            PersistedPromise::Missing => Ok(PromiseState::Missing),
            PersistedPromise::Pending => Ok(PromiseState::Pending),
            PersistedPromise::Resolved { terminal_json } => Ok(PromiseState::Resolved(
                self.decode_resolution(&terminal_json)?,
            )),
            PersistedPromise::UnknownOrRevoked => Ok(PromiseState::Revoked),
        }
    }

    fn row_identity(&self, key: &AwaitEventKey) -> Result<AwaitEventRowIdentity, RuntimeError> {
        Ok(AwaitEventRowIdentity {
            scope_json: serde_json::to_string(&key.scope).map_err(|err| self.encode_error(&err))?,
            wait_json: serde_json::to_string(&key.wait).map_err(|err| self.encode_error(&err))?,
            session_id: key.scope.session_id().map(ToOwned::to_owned),
            turn_control: key.wait.is_turn_control(),
        })
    }

    fn encode_resolution(&self, resolution: &Resolution) -> Result<String, RuntimeError> {
        serde_json::to_string(resolution).map_err(|err| self.encode_error(&err))
    }

    fn decode_resolution(&self, encoded: &str) -> Result<Resolution, RuntimeError> {
        let vocabulary = self.backend.vocabulary();
        serde_json::from_str(encoded).map_err(|err| {
            RuntimeError::new(
                vocabulary.decode,
                format!(
                    "failed to decode {} await-event terminal: {err}",
                    vocabulary.display_name
                ),
            )
        })
    }

    fn encode_error(&self, err: &serde_json::Error) -> RuntimeError {
        let vocabulary = self.backend.vocabulary();
        RuntimeError::new(
            vocabulary.encode,
            format!(
                "failed to encode {} await-event state: {err}",
                vocabulary.display_name
            ),
        )
    }

    fn signature(
        &self,
        scope: &ExecutionScope,
        wait: &AwaitEventWaitIdentity,
        key_id: &str,
    ) -> Result<String, RuntimeError> {
        let mut mac = HmacSha256::new_from_slice(&self.signing_secret).map_err(|err| {
            let vocabulary = self.backend.vocabulary();
            RuntimeError::new(
                vocabulary.sign,
                format!(
                    "failed to initialize {} await-event signer: {err}",
                    vocabulary.display_name
                ),
            )
        })?;
        mac.update(&promise_semantics::sign_material(scope, wait, key_id));
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    /// Authenticate a presented key against this substrate's secret.
    ///
    /// Both the derived key id and the signature are compared, and both
    /// comparisons run in constant time: a key whose visible identity and
    /// routing identity disagree must not be honored, and neither comparison
    /// may leak where it diverged.
    fn authenticates(&self, key: &AwaitEventKey) -> Result<bool, RuntimeError> {
        let Ok(derived_key_id) = promise_semantics::derive_key_id(&key.scope, &key.wait) else {
            return Ok(false);
        };
        let expected_signature = self.signature(&key.scope, &key.wait, &key.key_id)?;
        let key_id_matches =
            promise_semantics::constant_time_eq(derived_key_id.as_bytes(), key.key_id.as_bytes());
        let signature_matches = promise_semantics::constant_time_eq(
            expected_signature.as_bytes(),
            key.signature.as_bytes(),
        );
        Ok(key_id_matches & signature_matches)
    }

    fn notifier_for(&self, key_id: &str) -> Result<Arc<Notify>, RuntimeError> {
        let mut notifiers = self.notifiers.lock().map_err(|_| self.notifier_error())?;
        Ok(Arc::clone(
            notifiers
                .entry(key_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new())),
        ))
    }

    fn notify_key(&self, key_id: &str) -> Result<(), RuntimeError> {
        if let Some(notify) = self
            .notifiers
            .lock()
            .map_err(|_| self.notifier_error())?
            .get(key_id)
        {
            notify.notify_waiters();
        }
        Ok(())
    }

    fn notify_all(&self) -> Result<(), RuntimeError> {
        for notify in self
            .notifiers
            .lock()
            .map_err(|_| self.notifier_error())?
            .values()
        {
            notify.notify_waiters();
        }
        Ok(())
    }

    fn notifier_error(&self) -> RuntimeError {
        let vocabulary = self.backend.vocabulary();
        RuntimeError::new(
            vocabulary.notify,
            format!(
                "{} await-event notifier lock poisoned",
                vocabulary.display_name
            ),
        )
    }
}

/// The shared non-oracular failure for an invalid, unknown, or revoked key.
///
/// Backend-independent by design: hosts match this one code, and it must not
/// reveal which of the three conditions applied.
fn unknown_or_revoked() -> RuntimeError {
    RuntimeError::new(
        crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        "await-event key is invalid or revoked",
    )
}

/// Session-scoped levers take a session id, not a key, so they validate it
/// themselves rather than inheriting an authenticated key's guarantees.
fn validate_session_id(session_id: &str) -> Result<(), RuntimeError> {
    if session_id.trim().is_empty() {
        return Err(RuntimeError::new(
            crate::RuntimeErrorCode::InvalidAwaitEventSessionId,
            "await-event session id must be non-empty",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One in-memory promise row.
    struct MemoryRow {
        identity: AwaitEventRowIdentity,
        terminal_json: Option<String>,
    }

    /// Minimal in-memory backend: enough substrate to pin the coordinator's own
    /// decisions without a database. Durable behavior is pinned by the SQLite
    /// and PostgreSQL conformance suites, not here.
    #[derive(Clone, Default)]
    struct MemoryBackend {
        rows: Arc<Mutex<HashMap<String, MemoryRow>>>,
        revoked: Arc<Mutex<Vec<String>>>,
        inspections: Arc<AtomicUsize>,
    }

    impl MemoryBackend {
        fn revoked_session(&self, session_id: Option<&str>) -> bool {
            session_id.is_some_and(|session_id| {
                self.revoked
                    .lock()
                    .expect("revocation list")
                    .iter()
                    .any(|revoked| revoked == session_id)
            })
        }
    }

    #[async_trait]
    impl AwaitEventBackend for MemoryBackend {
        fn vocabulary(&self) -> AwaitEventVocabulary {
            AwaitEventVocabulary {
                sign: RuntimeErrorCode::from_wire_code("memory_await_event_sign"),
                encode: RuntimeErrorCode::from_wire_code("memory_await_event_encode"),
                decode: RuntimeErrorCode::from_wire_code("memory_await_event_decode"),
                notify: RuntimeErrorCode::from_wire_code("memory_await_event_notify"),
                display_name: "in-memory",
            }
        }

        async fn session_is_revoked(&self, session_id: &str) -> Result<bool, RuntimeError> {
            Ok(self.revoked_session(Some(session_id)))
        }

        async fn ensure_pending(
            &self,
            key_id: &str,
            identity: &AwaitEventRowIdentity,
            _now_ms: u64,
        ) -> Result<bool, RuntimeError> {
            if self.revoked_session(identity.session_id.as_deref()) {
                return Ok(false);
            }
            let mut rows = self.rows.lock().expect("rows");
            match rows.get(key_id) {
                Some(row) => Ok(&row.identity == identity),
                None => {
                    rows.insert(
                        key_id.to_string(),
                        MemoryRow {
                            identity: identity.clone(),
                            terminal_json: None,
                        },
                    );
                    Ok(true)
                }
            }
        }

        async fn store_terminal(
            &self,
            key_id: &str,
            identity: &AwaitEventRowIdentity,
            terminal_json: &str,
            _now_ms: u64,
        ) -> Result<TerminalCas, RuntimeError> {
            if self.revoked_session(identity.session_id.as_deref()) {
                return Ok(TerminalCas::UnknownOrRevoked);
            }
            let mut rows = self.rows.lock().expect("rows");
            match rows.get_mut(key_id) {
                Some(row) if &row.identity != identity => Ok(TerminalCas::UnknownOrRevoked),
                Some(MemoryRow {
                    terminal_json: Some(terminal),
                    ..
                }) => Ok(TerminalCas::AlreadyResolved {
                    terminal_json: terminal.clone(),
                }),
                Some(row) => {
                    row.terminal_json = Some(terminal_json.to_string());
                    Ok(TerminalCas::Stored)
                }
                None => {
                    rows.insert(
                        key_id.to_string(),
                        MemoryRow {
                            identity: identity.clone(),
                            terminal_json: Some(terminal_json.to_string()),
                        },
                    );
                    Ok(TerminalCas::Stored)
                }
            }
        }

        async fn inspect(
            &self,
            key_id: &str,
            identity: &AwaitEventRowIdentity,
        ) -> Result<PersistedPromise, RuntimeError> {
            self.inspections.fetch_add(1, Ordering::Relaxed);
            if self.revoked_session(identity.session_id.as_deref()) {
                return Ok(PersistedPromise::UnknownOrRevoked);
            }
            let rows = self.rows.lock().expect("rows");
            match rows.get(key_id) {
                None => Ok(PersistedPromise::Missing),
                Some(row) if &row.identity != identity => Ok(PersistedPromise::UnknownOrRevoked),
                Some(row) => Ok(row
                    .terminal_json
                    .clone()
                    .map_or(PersistedPromise::Pending, |terminal_json| {
                        PersistedPromise::Resolved { terminal_json }
                    })),
            }
        }

        async fn revoke_session(&self, session_id: &str, _now_ms: u64) -> Result<(), RuntimeError> {
            self.revoked
                .lock()
                .expect("revocation list")
                .push(session_id.to_string());
            self.rows
                .lock()
                .expect("rows")
                .retain(|_, row| row.identity.session_id.as_deref() != Some(session_id));
            Ok(())
        }

        async fn cancel_session_promises(
            &self,
            session_id: &str,
            terminal_json: &str,
            _now_ms: u64,
        ) -> Result<(), RuntimeError> {
            for row in self.rows.lock().expect("rows").values_mut() {
                if row.identity.session_id.as_deref() == Some(session_id)
                    && !row.identity.turn_control
                    && row.terminal_json.is_none()
                {
                    row.terminal_json = Some(terminal_json.to_string());
                }
            }
            Ok(())
        }
    }

    fn coordinator() -> AwaitEventCoordinator<MemoryBackend> {
        AwaitEventCoordinator::new(
            MemoryBackend::default(),
            Arc::from(b"coordinator-test-secret".to_vec()),
            Arc::new(crate::SystemClock),
        )
    }

    #[tokio::test]
    async fn resolve_buffers_before_wait_and_keeps_the_first_terminal() {
        let coordinator = coordinator();
        let key = coordinator
            .key_for(
                &ExecutionScope::turn("buffer-session", "turn"),
                AwaitEventWaitIdentity::tool_completion("call"),
            )
            .await
            .expect("mint key");
        let first = Resolution::Ok(serde_json::json!("first"));

        assert_eq!(
            coordinator
                .resolve(&key, first.clone())
                .await
                .expect("first resolve"),
            ResolveOutcome::Accepted
        );
        assert_eq!(
            coordinator
                .resolve(&key, Resolution::Cancelled)
                .await
                .expect("second resolve"),
            ResolveOutcome::AlreadyResolved {
                terminal: first.clone()
            }
        );
        assert_eq!(
            coordinator.peek(&key).await.expect("peek"),
            Some(first.clone())
        );
        assert_eq!(
            coordinator
                .await_resolution(&key, CancellationToken::new(), None)
                .await
                .expect("await resolved promise"),
            first
        );
    }

    #[tokio::test]
    async fn a_tampered_key_is_rejected_without_reading_the_substrate() {
        let coordinator = coordinator();
        let key = coordinator
            .key_for(
                &ExecutionScope::turn("tamper-session", "turn"),
                AwaitEventWaitIdentity::tool_completion("call"),
            )
            .await
            .expect("mint key");
        let inspections = Arc::clone(&coordinator.backend.inspections);
        let baseline = inspections.load(Ordering::Relaxed);

        for tampered in [
            {
                let mut variant = key.clone();
                variant.signature.push('0');
                variant
            },
            {
                let mut variant = key.clone();
                variant.key_id.push('0');
                variant
            },
            {
                let mut variant = key.clone();
                variant.wait = AwaitEventWaitIdentity::tool_completion("other-call");
                variant
            },
        ] {
            assert_eq!(
                coordinator
                    .resolve(&tampered, Resolution::Cancelled)
                    .await
                    .expect("tampered resolve"),
                ResolveOutcome::UnknownOrRevoked
            );
            assert_eq!(
                coordinator
                    .peek(&tampered)
                    .await
                    .expect_err("tampered peek")
                    .code
                    .as_str(),
                "await_event_unknown_or_revoked"
            );
        }
        assert_eq!(
            inspections.load(Ordering::Relaxed),
            baseline,
            "authentication must fail before the substrate is read"
        );
        assert_eq!(
            coordinator.peek(&key).await.expect("valid key"),
            None,
            "failed authentication must not materialize a terminal"
        );
    }

    #[tokio::test]
    async fn revocation_is_a_tombstone_and_cancellation_leaves_the_session_usable() {
        let coordinator = coordinator();
        let cancel_scope = ExecutionScope::turn("cancel-session", "turn");
        let ordinary = coordinator
            .key_for(
                &cancel_scope,
                AwaitEventWaitIdentity::tool_completion("call"),
            )
            .await
            .expect("ordinary key");
        let gate = coordinator
            .key_for(&cancel_scope, AwaitEventWaitIdentity::TurnCancelGate)
            .await
            .expect("gate key");
        coordinator
            .backend
            .ensure_pending(
                &ordinary.key_id,
                &coordinator.row_identity(&ordinary).expect("row identity"),
                0,
            )
            .await
            .expect("register ordinary waiter");
        coordinator
            .backend
            .ensure_pending(
                &gate.key_id,
                &coordinator.row_identity(&gate).expect("row identity"),
                0,
            )
            .await
            .expect("register gate waiter");

        coordinator
            .cancel_session("cancel-session")
            .await
            .expect("cancel sweep");
        assert_eq!(
            coordinator.peek(&ordinary).await.expect("swept peek"),
            Some(Resolution::Cancelled)
        );
        assert_eq!(
            coordinator.peek(&gate).await.expect("gate peek"),
            None,
            "turn-control promises are never swept"
        );
        assert_eq!(
            coordinator
                .resolve(&gate, Resolution::Ok(serde_json::json!("still-open")))
                .await
                .expect("gate remains writable"),
            ResolveOutcome::Accepted
        );

        coordinator
            .revoke_session("cancel-session")
            .await
            .expect("revoke session");
        assert_eq!(
            coordinator
                .resolve(&ordinary, Resolution::Cancelled)
                .await
                .expect("resolve after revocation"),
            ResolveOutcome::UnknownOrRevoked
        );
        assert_eq!(
            coordinator
                .peek(&ordinary)
                .await
                .expect_err("peek after revocation")
                .code
                .as_str(),
            "await_event_unknown_or_revoked"
        );
        assert_eq!(
            coordinator
                .key_for(
                    &cancel_scope,
                    AwaitEventWaitIdentity::tool_completion("post-revoke"),
                )
                .await
                .expect_err("mint after revocation")
                .code
                .as_str(),
            "await_event_unknown_or_revoked"
        );
    }

    #[tokio::test]
    async fn session_levers_reject_a_blank_session_id() {
        let coordinator = coordinator();
        for error in [
            coordinator
                .revoke_session("  ")
                .await
                .expect_err("blank revoke"),
            coordinator
                .cancel_session("")
                .await
                .expect_err("blank cancel"),
        ] {
            assert_eq!(error.code.as_str(), "invalid_await_event_session_id");
        }
    }

    #[tokio::test]
    async fn coordinator_failures_carry_the_backend_vocabulary() {
        let coordinator = coordinator();
        let key = coordinator
            .key_for(
                &ExecutionScope::turn("vocabulary-session", "turn"),
                AwaitEventWaitIdentity::tool_completion("call"),
            )
            .await
            .expect("mint key");
        coordinator.backend.rows.lock().expect("rows").insert(
            key.key_id.clone(),
            MemoryRow {
                identity: coordinator.row_identity(&key).expect("row identity"),
                terminal_json: Some("not-json".to_string()),
            },
        );

        let error = coordinator.peek(&key).await.expect_err("corrupt terminal");
        assert_eq!(error.code.as_str(), "memory_await_event_decode");
        assert!(
            error
                .message
                .contains("failed to decode in-memory await-event terminal"),
            "unexpected decode message: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_turn_control_waiter_never_publishes_a_cancellation() {
        let coordinator = coordinator();
        let gate = coordinator
            .key_for(
                &ExecutionScope::turn("turn-control-session", "turn"),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("gate key");
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = coordinator
            .await_resolution(&gate, cancel, None)
            .await
            .expect_err("cancelled turn-control waiter");
        assert_eq!(error.code.as_str(), "turn_control_wait_cancelled");
        assert_eq!(
            coordinator.peek(&gate).await.expect("gate peek"),
            None,
            "a stopped observer must not manufacture a turn cancellation"
        );
    }
}
