//! In-process await-event (Durable Wait) registry backing the native effect
//! host and controller.

use crate::SessionId;
use lash_sansio::sync::MutexExt;
#[cfg(not(loom))]
use lash_sansio::sync::RwLockExt;
#[cfg(loom)]
use loom::sync::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
/// The session-shard map lock swaps to loom's instrumented `RwLock` under
/// `--cfg loom` (FIG-1161 seam 5).
#[cfg(not(loom))]
use std::sync::RwLock;
use std::time::Instant;

#[cfg(loom)]
use crate::loom_notify::Notify;
use hmac::{Hmac, Mac};
/// The per-entry notifier swaps to the crate-local loom shim under
/// `--cfg loom` so a `notify_waiters` landing between a waiter's pending
/// check and its subscription is explored by the model.
#[cfg(not(loom))]
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::RuntimeError;

use crate::promise_semantics::{
    PromiseState, PromiseTransition, SessionRevocationTransition, cancel_decided_observation,
    cancel_decision_fences, cancel_sweep, constant_time_eq, derive_key_id, resolve, revoke_session,
    session_allows_access, sign_material,
};
// Only the cancellation/deadline arms of the wait loop — which `cfg(loom)`
// replaces with a deterministic direct await — consume these.
#[cfg(not(loom))]
use crate::promise_semantics::{WaitStopReason, turn_control_wait_stop};
use crate::{AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome};

/// `lock_recover`/`read_recover`/`write_recover` for the loom primitives this
/// file swaps to under `--cfg loom` (FIG-1161 seam 5). `MutexExt` still
/// covers the real `std::sync::Mutex` fields (`revoked_session_order`,
/// `retired_scopes`, `after_pending`).
#[cfg(loom)]
mod loom_ext {
    pub trait LoomMutexExt<T> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T>;
    }

    impl<T> LoomMutexExt<T> for loom::sync::Mutex<T> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T> {
            self.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    pub trait LoomRwLockExt<T> {
        fn read_recover(&self) -> loom::sync::RwLockReadGuard<'_, T>;
        fn write_recover(&self) -> loom::sync::RwLockWriteGuard<'_, T>;
    }

    impl<T> LoomRwLockExt<T> for loom::sync::RwLock<T> {
        fn read_recover(&self) -> loom::sync::RwLockReadGuard<'_, T> {
            self.read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn write_recover(&self) -> loom::sync::RwLockWriteGuard<'_, T> {
            self.write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(loom)]
use loom_ext::{LoomMutexExt as _, LoomRwLockExt as _};

/// The per-session shard mutex swaps to loom's instrumented `Mutex` under
/// `--cfg loom` so the register-under-lock ordering is model-checked.
#[cfg(not(loom))]
type ShardMutex<T> = std::sync::Mutex<T>;
/// `cfg(loom)` twin of [`ShardMutex`].
#[cfg(loom)]
type ShardMutex<T> = loom::sync::Mutex<T>;

/// The guard [`AwaitEventRegistry::locked_state`] hands out.
#[cfg(not(loom))]
type ShardGuard<'a> = std::sync::MutexGuard<'a, AwaitEventRegistryState>;
/// `cfg(loom)` twin of [`ShardGuard`].
#[cfg(loom)]
type ShardGuard<'a> = loom::sync::MutexGuard<'a, AwaitEventRegistryState>;

type HmacSha256 = Hmac<sha2::Sha256>;

#[cfg(test)]
type PendingCheckHook = fn(&AwaitEventRegistry, &AwaitEventKey);

const COMPLETED_TURN_CONTROL_KEY_LIMIT: usize = 4_096;
const REVOKED_SESSION_LIMIT: usize = 4_096;

#[derive(Debug)]
struct AwaitEventEntry {
    verified_key: AwaitEventKey,
    terminal: Option<LiveTerminal>,
    notify: Arc<Notify>,
    /// Waiters currently parked on this promise. A scope with a parked waiter
    /// is not quiescent: retiring it would strand a live continuation.
    waiters: usize,
}

impl AwaitEventEntry {
    fn for_key(key: &AwaitEventKey) -> Self {
        Self {
            verified_key: key.clone(),
            terminal: None,
            notify: Arc::new(Notify::new()),
            waiters: 0,
        }
    }
}

/// What closed a live promise: a resolution that won, or the owning group
/// child's cancel decision (ADR 0099 §4), which refuses every later resolve
/// and reads as `Cancelled`.
#[derive(Clone, Debug)]
enum LiveTerminal {
    Resolved(Resolution),
    CancelDecided,
}

impl LiveTerminal {
    fn promise_state(&self) -> PromiseState {
        match self {
            Self::Resolved(resolution) => PromiseState::Resolved(resolution.clone()),
            Self::CancelDecided => PromiseState::CancelDecided,
        }
    }

    fn observed(&self) -> Resolution {
        match self {
            Self::Resolved(resolution) => resolution.clone(),
            Self::CancelDecided => cancel_decided_observation(),
        }
    }
}

fn live_state(terminal: Option<&LiveTerminal>) -> PromiseState {
    terminal.map_or(PromiseState::Pending, LiveTerminal::promise_state)
}

struct ParkedWaiter {
    shard: AwaitEventRegistryShard,
    key_id: String,
}

impl Drop for ParkedWaiter {
    fn drop(&mut self) {
        let mut state = self.shard.lock_recover();
        if let Some(PromiseSlot::Live(entry)) = state.promises.get_mut(&self.key_id) {
            entry.waiters = entry.waiters.saturating_sub(1);
        }
    }
}

/// One promise's slot in the registry: live until it resolves, and — for
/// turn-control keys — archived afterwards so a late resolve still reads its
/// terminal. A key id occupies exactly one slot, so a promise cannot be live
/// and archived at once.
#[derive(Debug)]
enum PromiseSlot {
    Live(AwaitEventEntry),
    ArchivedTurnControl(CompletedTurnControlEntry),
}

impl PromiseSlot {
    fn verified_key(&self) -> &AwaitEventKey {
        match self {
            Self::Live(entry) => &entry.verified_key,
            Self::ArchivedTurnControl(entry) => &entry.verified_key,
        }
    }
}

#[derive(Debug)]
struct AwaitEventRegistryState {
    promises: HashMap<String, PromiseSlot>,
    /// Key ids of the archived turn-control slots, in archive order. It is
    /// also the archived-slot count: every `ArchivedTurnControl` slot has
    /// exactly one entry here.
    completed_turn_control_order: VecDeque<String>,
    revoked: bool,
}

#[derive(Debug)]
struct CompletedTurnControlEntry {
    verified_key: AwaitEventKey,
    terminal: Resolution,
}

/// What a presented key resolves to inside a locked shard: the revoked gate
/// and the stored-key verification every reader performs, run once by
/// [`AwaitEventRegistry::promise_lookup`].
enum PromiseLookup<'a> {
    /// The session is revoked or the scope retired. `resolve` routes this
    /// through its outcome mapping; readers answer unknown-or-revoked.
    Revoked,
    /// A slot carries this key id and the presented key verifies against it.
    Slot(&'a mut PromiseSlot),
    /// A slot carries this key id but the presented key does not verify.
    Mismatched,
    /// No slot carries this key id.
    Missing,
}

impl AwaitEventRegistryState {
    fn new() -> Self {
        Self {
            promises: HashMap::new(),
            completed_turn_control_order: VecDeque::new(),
            revoked: false,
        }
    }
}

type AwaitEventRegistryShard = Arc<ShardMutex<AwaitEventRegistryState>>;

#[derive(Debug)]
pub struct AwaitEventRegistry {
    secret: Vec<u8>,
    session_shards: RwLock<HashMap<SessionId, AwaitEventRegistryShard>>,
    unscoped_shard: AwaitEventRegistryShard,
    revoked_session_order: std::sync::Mutex<VecDeque<SessionId>>,
    /// Journal-identity keys of retired non-session scopes. The in-process
    /// twin of the durable scope-retirement fence, and like it unbounded: a
    /// fence that could be evicted would let a retired scope mint again once
    /// enough later retirements had passed through, which is exactly the
    /// re-admission the fence exists to refuse. Growth is bounded by the
    /// host's own retirement rate (one small string per retired process or
    /// runtime operation for the life of the process), and a process fence
    /// is released again by [`reinstate_scope`](Self::reinstate_scope) when
    /// the host re-registers the id.
    retired_scopes: std::sync::Mutex<HashSet<String>>,
    completed_turn_control_key_limit: usize,
    revoked_session_limit: usize,
    #[cfg(test)]
    verify_uncached_calls: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    after_pending: std::sync::Mutex<Option<PendingCheckHook>>,
}

impl Default for AwaitEventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AwaitEventRegistry {
    pub fn new() -> Self {
        Self::with_limits(COMPLETED_TURN_CONTROL_KEY_LIMIT, REVOKED_SESSION_LIMIT)
    }

    fn with_limits(completed_turn_control_key_limit: usize, revoked_session_limit: usize) -> Self {
        Self {
            secret: uuid::Uuid::new_v4().as_bytes().to_vec(),
            session_shards: RwLock::new(HashMap::new()),
            unscoped_shard: Arc::new(ShardMutex::new(AwaitEventRegistryState::new())),
            revoked_session_order: std::sync::Mutex::new(VecDeque::new()),
            retired_scopes: std::sync::Mutex::new(HashSet::new()),
            completed_turn_control_key_limit,
            revoked_session_limit,
            #[cfg(test)]
            verify_uncached_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            after_pending: std::sync::Mutex::new(None),
        }
    }

    fn shard_for_session(&self, session_id: &SessionId) -> AwaitEventRegistryShard {
        if let Some(shard) = self.session_shards.read_recover().get(session_id).cloned() {
            return shard;
        }
        let mut shards = self.session_shards.write_recover();
        Arc::clone(
            shards
                .entry(SessionId::from(session_id.to_string()))
                .or_insert_with(|| Arc::new(ShardMutex::new(AwaitEventRegistryState::new()))),
        )
    }

    fn existing_session_shard(&self, session_id: &SessionId) -> Option<AwaitEventRegistryShard> {
        self.session_shards.read_recover().get(session_id).cloned()
    }

    /// Snapshot the registered, unresolved keys of one session without
    /// materializing a shard for an unknown session.
    pub fn outstanding_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        if session_id.trim().is_empty() {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidAwaitEventSessionId,
                "await-event session id must be non-empty",
            ));
        }
        let Some(shard) = self.existing_session_shard(session_id) else {
            return Ok(Vec::new());
        };
        let state = Self::locked_state(&shard);
        if state.revoked {
            return Ok(Vec::new());
        }
        let mut keys = state
            .promises
            .values()
            .filter_map(|slot| match slot {
                PromiseSlot::Live(entry) if entry.terminal.is_none() => {
                    Some(entry.verified_key.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        keys.sort_unstable_by(|left, right| left.key_id.cmp(&right.key_id));
        Ok(keys)
    }

    fn shard_for_scope(&self, scope: &ExecutionScope) -> AwaitEventRegistryShard {
        match scope.session_id() {
            Some(session_id) => self.shard_for_session(&SessionId::from(session_id)),
            None => Arc::clone(&self.unscoped_shard),
        }
    }

    fn locked_state(shard: &AwaitEventRegistryShard) -> ShardGuard<'_> {
        shard.lock_recover()
    }

    /// The revoked gate and the stored-key verification every reader shares.
    /// `resolve`, `peek_resolution`, and `await_resolution_inner` used to
    /// spell the revoked → archived → live ladder by hand; a new reader that
    /// took the natural hot-path order would read a stale live slot for an
    /// already-archived turn-control gate — a waiter that hangs after its
    /// terminal was recorded.
    fn promise_lookup<'a>(
        &self,
        state: &'a mut AwaitEventRegistryState,
        key: &AwaitEventKey,
    ) -> Result<PromiseLookup<'a>, RuntimeError> {
        if state.revoked || self.scope_is_retired(&key.scope)? {
            return Ok(PromiseLookup::Revoked);
        }
        Ok(match state.promises.get_mut(&key.key_id) {
            Some(slot) if Self::verified_key_matches(slot.verified_key(), key) => {
                PromiseLookup::Slot(slot)
            }
            Some(_) => PromiseLookup::Mismatched,
            None => PromiseLookup::Missing,
        })
    }

    pub fn key_for(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        wait.validate()?;
        match scope.session_id() {
            Some(session_id) => {
                if let Some(shard) = self.existing_session_shard(&SessionId::from(session_id)) {
                    let state = Self::locked_state(&shard);
                    if !session_allows_access(state.revoked) {
                        return Err(Self::unknown_or_revoked());
                    }
                }
            }
            None => {
                let state = Self::locked_state(&self.unscoped_shard);
                if !session_allows_access(state.revoked) {
                    return Err(Self::unknown_or_revoked());
                }
                drop(state);
                if self.scope_is_retired(scope)? {
                    return Err(Self::unknown_or_revoked());
                }
            }
        }
        self.derive_key(scope, wait)
    }

    /// Session scopes never do: they are fenced per session shard by revocation.
    pub fn scope_is_retired(&self, scope: &ExecutionScope) -> Result<bool, RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(false);
        }
        let scope_id = scope.journal_identity()?;
        Ok(self.retired_scopes.lock_recover().contains(scope_id.key()))
    }

    fn derive_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        let key_id = derive_key_id(scope, &wait)?;
        let signature = self.signature(scope, &wait, &key_id)?;
        Ok(AwaitEventKey {
            scope: scope.clone(),
            wait,
            key_id,
            signature,
        })
    }

    fn signature(
        &self,
        scope: &ExecutionScope,
        wait: &AwaitEventWaitIdentity,
        key_id: &str,
    ) -> Result<String, RuntimeError> {
        let mut mac = HmacSha256::new_from_slice(&self.secret).map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::AwaitEventKeySign,
                format!("failed to initialize await-event key signer: {err}"),
            )
        })?;
        mac.update(&sign_material(scope, wait, key_id));
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    fn verify_uncached(&self, key: &AwaitEventKey) -> Result<bool, RuntimeError> {
        #[cfg(test)]
        self.verify_uncached_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let expected = self.signature(&key.scope, &key.wait, &key.key_id)?;
        Ok(constant_time_eq(
            expected.as_bytes(),
            key.signature.as_bytes(),
        ))
    }

    fn verified_key_matches(stored: &AwaitEventKey, presented: &AwaitEventKey) -> bool {
        stored.scope == presented.scope
            && stored.wait == presented.wait
            && stored.key_id == presented.key_id
            && constant_time_eq(stored.signature.as_bytes(), presented.signature.as_bytes())
    }

    fn unknown_or_revoked() -> RuntimeError {
        RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
            "await-event key is invalid or revoked",
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "a resolve transition always carries a public outcome"
    )]
    pub fn resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !self.verify_uncached(key)? {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        let shard = self.shard_for_scope(&key.scope);
        let mut state = Self::locked_state(&shard);
        match self.promise_lookup(&mut state, key)? {
            PromiseLookup::Revoked => {
                return resolve(PromiseState::Revoked, resolution)
                    .resolve_outcome()
                    .expect("revoked resolve always has a public outcome");
            }
            PromiseLookup::Mismatched => return Ok(ResolveOutcome::UnknownOrRevoked),
            PromiseLookup::Slot(PromiseSlot::ArchivedTurnControl(completed)) => {
                return resolve(
                    PromiseState::Resolved(completed.terminal.clone()),
                    resolution,
                )
                .resolve_outcome()
                .expect("resolved promise always has a public outcome");
            }
            PromiseLookup::Slot(PromiseSlot::Live(entry)) => {
                match resolve(live_state(entry.terminal.as_ref()), resolution) {
                    PromiseTransition::Store(terminal) => {
                        entry.terminal = Some(LiveTerminal::Resolved(terminal));
                        entry.notify.notify_waiters();
                    }
                    transition => {
                        return transition
                            .resolve_outcome()
                            .expect("normal resolve always has a public outcome");
                    }
                }
            }
            PromiseLookup::Missing => {
                let mut entry = AwaitEventEntry::for_key(key);
                let PromiseTransition::Store(terminal) = resolve(PromiseState::Missing, resolution)
                else {
                    unreachable!("missing promise always buffers the proposed terminal")
                };
                entry.terminal = Some(LiveTerminal::Resolved(terminal));
                entry.notify.notify_waiters();
                state
                    .promises
                    .insert(key.key_id.clone(), PromiseSlot::Live(entry));
            }
        }
        if matches!(key.wait, AwaitEventWaitIdentity::TurnTerminal) {
            self.archive_turn_control(&mut state, key)?;
        }
        Ok(ResolveOutcome::Accepted)
    }

    pub fn peek_resolution(&self, key: &AwaitEventKey) -> Result<Option<Resolution>, RuntimeError> {
        let shard = self.shard_for_scope(&key.scope);
        let mut state = Self::locked_state(&shard);
        match self.promise_lookup(&mut state, key)? {
            PromiseLookup::Revoked | PromiseLookup::Mismatched => Err(Self::unknown_or_revoked()),
            PromiseLookup::Slot(PromiseSlot::ArchivedTurnControl(completed)) => {
                Ok(Some(completed.terminal.clone()))
            }
            PromiseLookup::Slot(PromiseSlot::Live(entry)) => {
                Ok(entry.terminal.as_ref().map(LiveTerminal::observed))
            }
            PromiseLookup::Missing => {
                if !self.verify_uncached(key)? {
                    return Err(Self::unknown_or_revoked());
                }
                Ok(None)
            }
        }
    }

    fn archive_turn_control(
        &self,
        state: &mut AwaitEventRegistryState,
        terminal_key: &AwaitEventKey,
    ) -> Result<(), RuntimeError> {
        let gate_key =
            self.derive_key(&terminal_key.scope, AwaitEventWaitIdentity::TurnCancelGate)?;
        let escalation_key = self.derive_key(
            &terminal_key.scope,
            AwaitEventWaitIdentity::TurnCancelEscalation,
        )?;
        for key_id in [
            &gate_key.key_id,
            &escalation_key.key_id,
            &terminal_key.key_id,
        ] {
            // Absent or already archived slots are left alone.
            if !matches!(state.promises.get(key_id), Some(PromiseSlot::Live(_))) {
                continue;
            }
            let Some(PromiseSlot::Live(entry)) = state.promises.remove(key_id) else {
                unreachable!("checked live above");
            };
            let Some(terminal) = entry.terminal.as_ref().map(LiveTerminal::observed) else {
                // An escalation nobody wrote is a bare waiter slot the
                // finished turn no longer needs.
                continue;
            };
            state.promises.insert(
                key_id.clone(),
                PromiseSlot::ArchivedTurnControl(CompletedTurnControlEntry {
                    verified_key: entry.verified_key,
                    terminal,
                }),
            );
            state.completed_turn_control_order.push_back(key_id.clone());
        }
        while state.completed_turn_control_order.len() > self.completed_turn_control_key_limit {
            let Some(key_id) = state.completed_turn_control_order.pop_front() else {
                break;
            };
            state.promises.remove(&key_id);
        }
        Ok(())
    }

    pub async fn await_resolution(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        clock: &dyn crate::Clock,
    ) -> Result<Resolution, RuntimeError> {
        if let Some(resolution) = self.peek_resolution(key)? {
            return Ok(resolution);
        }
        lash_core_ids::execution_permit::release_process_execution_permit_while(
            self.await_resolution_inner(key, cancel, deadline, clock),
        )
        .await
    }

    async fn await_resolution_inner(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        clock: &dyn crate::Clock,
    ) -> Result<Resolution, RuntimeError> {
        let shard = self.shard_for_scope(&key.scope);
        loop {
            let notified = {
                let mut state = Self::locked_state(&shard);
                let entry = match self.promise_lookup(&mut state, key)? {
                    PromiseLookup::Revoked | PromiseLookup::Mismatched => {
                        return Err(Self::unknown_or_revoked());
                    }
                    PromiseLookup::Slot(PromiseSlot::ArchivedTurnControl(completed)) => {
                        return Ok(completed.terminal.clone());
                    }
                    PromiseLookup::Slot(PromiseSlot::Live(entry)) => entry,
                    PromiseLookup::Missing => {
                        if !self.verify_uncached(key)? {
                            return Err(Self::unknown_or_revoked());
                        }
                        state.promises.insert(
                            key.key_id.clone(),
                            PromiseSlot::Live(AwaitEventEntry::for_key(key)),
                        );
                        let Some(PromiseSlot::Live(entry)) = state.promises.get_mut(&key.key_id)
                        else {
                            unreachable!("await-event entry inserted above")
                        };
                        entry
                    }
                };
                if let Some(terminal) = entry.terminal.as_ref().map(LiveTerminal::observed) {
                    return Ok(terminal);
                }
                // Register while the state lock still excludes resolvers and
                // revocation. A notify_waiters between the pending check and
                // subscription would otherwise leave a completed promise parked.
                let mut notified = Box::pin(Arc::clone(&entry.notify).notified_owned());
                notified.as_mut().enable();
                entry.waiters += 1;
                (
                    notified,
                    ParkedWaiter {
                        shard: Arc::clone(&shard),
                        key_id: key.key_id.clone(),
                    },
                )
            };
            let (mut notified, _parked) = notified;
            #[cfg(test)]
            if let Some(after_pending) = self.after_pending.lock_recover().take() {
                after_pending(self, key);
            }
            #[cfg(loom)]
            {
                // `tokio::select!` randomizes arm poll order, which makes the
                // loom model non-deterministic and breaks path replay. The
                // seam under test is the notification arm — the loom models
                // never cancel the token or set a deadline — so awaiting
                // `notified` directly exercises the same ordering.
                let _ = cancel;
                let _ = deadline;
                let _ = clock;
                notified.as_mut().await;
            }
            #[cfg(not(loom))]
            {
                let deadline = async {
                    if let Some(deadline) = deadline {
                        clock.sleep_until(deadline).await;
                    } else {
                        // This arm must never resolve; a resolvable default would silently give a no-deadline waiter a timeout it never had.
                        std::future::pending().await
                    }
                };
                tokio::pin!(deadline);
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if let Some((code, message)) =
                            turn_control_wait_stop(&key.wait, WaitStopReason::Cancelled)
                        {
                            return Err(RuntimeError::new(code, message));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled)?;
                    }
                    _ = &mut deadline => {
                        if let Some((code, message)) =
                            turn_control_wait_stop(&key.wait, WaitStopReason::TimedOut)
                        {
                            return Err(RuntimeError::new(code, message));
                        }
                        let _ = self.resolve(key, Resolution::Timeout)?;
                    }
                    _ = &mut notified => {}
                }
            }
        }
    }

    /// Revoke every await-event for `session_id`: in-flight waiters error with
    /// `await_event_unknown_or_revoked`, matching entries are drained, and a
    /// bounded recent-session tombstone cache rejects keys created after
    /// deletion without growing for the lifetime of the process.
    pub fn revoke_session(&self, session_id: &SessionId) -> Result<(), RuntimeError> {
        let shard = self.shard_for_session(session_id);
        let newly_revoked = {
            let mut state = Self::locked_state(&shard);
            let transition = revoke_session(state.revoked);
            let newly_revoked = transition == SessionRevocationTransition::MarkRevoked;
            state.revoked = true;
            for slot in state.promises.values() {
                if let PromiseSlot::Live(entry) = slot {
                    entry.notify.notify_waiters();
                }
            }
            state.promises.clear();
            state.completed_turn_control_order.clear();
            newly_revoked
        };
        if !newly_revoked {
            return Ok(());
        }
        let expired = {
            let mut order = self.revoked_session_order.lock_recover();
            order.push_back(session_id.clone());
            let mut expired = Vec::new();
            while order.len() > self.revoked_session_limit {
                if let Some(session_id) = order.pop_front() {
                    expired.push(session_id);
                }
            }
            expired
        };
        if !expired.is_empty() {
            let mut shards = self.session_shards.write_recover();
            for session_id in expired {
                shards.remove(&session_id);
            }
        }
        Ok(())
    }

    /// Retire a terminal non-session `scope`: in-flight waiters under it error
    /// with `await_event_unknown_or_revoked`, its entries are dropped, and a
    /// permanent fence rejects later mints, resolves, peeks, and waits under
    /// the scope. The in-process twin of the durable scope-retirement fence.
    pub fn retire_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.retire_scope_gated(scope, false).map(|_| ())
    }

    /// [`retire_scope`](Self::retire_scope) only if no waiter is parked on a
    /// promise under `scope`. The proof and the fence happen under one lock,
    /// so no waiter can park between them. Answers whether the scope retired.
    pub fn retire_scope_if_quiescent(&self, scope: &ExecutionScope) -> Result<bool, RuntimeError> {
        self.retire_scope_gated(scope, true)
    }

    fn retire_scope_gated(
        &self,
        scope: &ExecutionScope,
        only_if_quiescent: bool,
    ) -> Result<bool, RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(crate::await_event_support::await_event_scope_not_retirable(
                scope,
            ));
        }
        let scope_id = scope.journal_identity()?.key().to_string();
        {
            let mut state = Self::locked_state(&self.unscoped_shard);
            if only_if_quiescent
                && state.promises.values().any(|slot| {
                    matches!(slot, PromiseSlot::Live(entry)
                        if entry.verified_key.scope == *scope && entry.waiters > 0)
                })
            {
                return Ok(false);
            }
            state.promises.retain(|_, slot| {
                if slot.verified_key().scope != *scope {
                    return true;
                }
                if let PromiseSlot::Live(entry) = slot {
                    entry.notify.notify_waiters();
                }
                false
            });
            let AwaitEventRegistryState {
                promises,
                completed_turn_control_order,
                ..
            } = &mut *state;
            completed_turn_control_order.retain(|key_id| promises.contains_key(key_id));
            // Fence while the shard lock still excludes new waiters: the
            // quiescence proof above and the fence land together.
            self.retired_scopes.lock_recover().insert(scope_id);
        }
        Ok(true)
    }

    /// Lift the fence on a non-session `scope` whose owner is registered
    /// again. Only the fence goes; the scope starts with no promises, which is
    /// what a re-registered process id expects.
    pub fn reinstate_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(crate::await_event_support::await_event_scope_not_retirable(
                scope,
            ));
        }
        let scope_id = scope.journal_identity()?;
        self.retired_scopes.lock_recover().remove(scope_id.key());
        Ok(())
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize, usize) {
        let mut shards = self
            .session_shards
            .read_recover()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        shards.push(Arc::clone(&self.unscoped_shard));
        let mut counts = (0, 0, 0);
        for shard in shards {
            let state = Self::locked_state(&shard);
            for slot in state.promises.values() {
                match slot {
                    PromiseSlot::Live(_) => counts.0 += 1,
                    PromiseSlot::ArchivedTurnControl(_) => counts.1 += 1,
                }
            }
            counts.2 += usize::from(state.revoked);
        }
        counts
    }

    #[cfg(test)]
    fn has_entry(&self, key: &AwaitEventKey) -> bool {
        let shard = self.shard_for_scope(&key.scope);
        let state = Self::locked_state(&shard);
        matches!(state.promises.get(&key.key_id), Some(PromiseSlot::Live(_)))
    }

    #[cfg(test)]
    fn is_completed_turn_control(&self, key: &AwaitEventKey) -> bool {
        let shard = self.shard_for_scope(&key.scope);
        let state = Self::locked_state(&shard);
        matches!(
            state.promises.get(&key.key_id),
            Some(PromiseSlot::ArchivedTurnControl(_))
        )
    }

    /// Close the completion key `scope`/`wait` names because the group child
    /// that owns it is cancel-decided (ADR 0099 §4, W17).
    ///
    /// Called by the owner of the child's cancel fence while it holds that
    /// fence, so the decision and the closed key are one step. A promise
    /// nobody resolved yet — including one no waiter registered — becomes
    /// cancel-decided: every later resolve is refused, typed, and a waiter
    /// reads `Cancelled`. A terminal that won first stays authoritative, and a
    /// revoked session or retired scope is left as it is.
    pub fn fence_cancel_decided(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<(), RuntimeError> {
        let key = self.derive_key(scope, wait)?;
        let shard = self.shard_for_scope(&key.scope);
        let mut state = Self::locked_state(&shard);
        let observed = match self.promise_lookup(&mut state, &key)? {
            PromiseLookup::Revoked | PromiseLookup::Mismatched => return Ok(()),
            PromiseLookup::Slot(PromiseSlot::ArchivedTurnControl(_)) => return Ok(()),
            PromiseLookup::Slot(PromiseSlot::Live(entry)) => live_state(entry.terminal.as_ref()),
            PromiseLookup::Missing => PromiseState::Missing,
        };
        if !cancel_decision_fences(&observed) {
            return Ok(());
        }
        let entry = match state.promises.entry(key.key_id.clone()) {
            std::collections::hash_map::Entry::Occupied(slot) => match slot.into_mut() {
                PromiseSlot::Live(entry) => entry,
                PromiseSlot::ArchivedTurnControl(_) => {
                    unreachable!("an archived slot is never fenced")
                }
            },
            std::collections::hash_map::Entry::Vacant(slot) => {
                let PromiseSlot::Live(entry) =
                    slot.insert(PromiseSlot::Live(AwaitEventEntry::for_key(&key)))
                else {
                    unreachable!("a live slot was inserted")
                };
                entry
            }
        };
        entry.terminal = Some(LiveTerminal::CancelDecided);
        entry.notify.notify_waiters();
        Ok(())
    }

    /// Resolve every *outstanding* wait for `session_id` with
    /// [`Resolution::Cancelled`], leaving the session usable: already-terminal
    /// waits keep their terminal, and waits registered afterwards behave
    /// normally. This is the standalone host lever, in contrast to the
    /// tombstoning [`revoke_session`](Self::revoke_session).
    pub fn cancel_session(&self, session_id: &SessionId) -> Result<(), RuntimeError> {
        let Some(shard) = self.existing_session_shard(session_id) else {
            return Ok(());
        };
        let mut state = Self::locked_state(&shard);
        for slot in state.promises.values_mut() {
            // Archived slots are already terminal; only live entries are
            // swept. The variant, not an unstated cross-collection
            // assumption, is what excludes them.
            let PromiseSlot::Live(entry) = slot else {
                continue;
            };
            if let PromiseTransition::Store(terminal) = cancel_sweep(
                &entry.verified_key.wait,
                live_state(entry.terminal.as_ref()),
            ) {
                entry.terminal = Some(LiveTerminal::Resolved(terminal));
                entry.notify.notify_waiters();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnId;
    use std::sync::Barrier;
    use std::time::Duration;

    fn turn_scope(session_id: &SessionId, turn_id: &TurnId) -> ExecutionScope {
        ExecutionScope::turn(session_id, turn_id)
    }

    /// ADR 0099 §4, W17: a cancel decision closes an unresolved completion
    /// key — registered or not — so every later resolve is refused, typed,
    /// and a reader observes `Cancelled`; a completion that won first keeps
    /// its answer.
    #[tokio::test]
    async fn a_cancel_decision_refuses_every_later_resolve_of_an_unresolved_key() {
        let registry = AwaitEventRegistry::new();
        let scope = turn_scope(&SessionId::from("fence"), &TurnId::from("turn"));
        let late = Resolution::Ok(serde_json::json!("late"));
        for (call, registered) in [("unregistered", false), ("registered", true)] {
            let wait = AwaitEventWaitIdentity::tool_completion(call);
            let key = registry.key_for(&scope, wait.clone()).expect("key");
            if registered {
                // One poll parks a waiter, which registers the pending slot.
                let parked = registry.await_resolution(
                    &key,
                    CancellationToken::new(),
                    None,
                    &crate::SystemClock,
                );
                let mut parked = std::pin::pin!(parked);
                let polled = std::future::Future::poll(
                    parked.as_mut(),
                    &mut std::task::Context::from_waker(std::task::Waker::noop()),
                );
                assert!(polled.is_pending(), "the waiter parks on the pending key");
            }
            registry
                .fence_cancel_decided(&scope, wait.clone())
                .expect("fence");
            for _ in 0..2 {
                let refusal = registry
                    .resolve(&key, late.clone())
                    .expect_err("a late resolve is refused");
                assert_eq!(
                    refusal.code,
                    crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided
                );
            }
            assert_eq!(
                registry.peek_resolution(&key).expect("peek"),
                Some(Resolution::Cancelled)
            );
        }
        let wait = AwaitEventWaitIdentity::tool_completion("won-first");
        let key = registry.key_for(&scope, wait.clone()).expect("key");
        let first = Resolution::Ok(serde_json::json!("first"));
        registry
            .resolve(&key, first.clone())
            .expect("first resolve");
        registry.fence_cancel_decided(&scope, wait).expect("fence");
        assert_eq!(
            registry.resolve(&key, late).expect("not refused"),
            ResolveOutcome::AlreadyResolved { terminal: first }
        );
    }

    #[tokio::test]
    async fn completion_between_pending_check_and_park_is_delivered() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let registry = Arc::new(AwaitEventRegistry::new());
        let key = registry
            .key_for(
                &turn_scope(&SessionId::from("completion-gap"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("completion key");
        *registry.after_pending.lock_recover() = Some(|registry, key| {
            registry
                .resolve(key, Resolution::Ok(serde_json::json!("delivered")))
                .expect("publish in check-to-park gap");
        });
        let wait =
            registry.await_resolution(&key, CancellationToken::new(), None, &crate::SystemClock);
        tokio::pin!(wait);
        let result = wait.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(
            matches!(result, Poll::Ready(Ok(Resolution::Ok(_)))),
            "stored completion must be observed without another wake: {result:?}"
        );
    }

    #[test]
    fn key_derivation_does_not_register_or_materialize_session_state() {
        let registry = AwaitEventRegistry::new();
        let session_id = SessionId::from("pure-key");
        let scope = turn_scope(&session_id, &TurnId::from("turn"));

        registry
            .key_for(&scope, AwaitEventWaitIdentity::tool_completion("tool-call"))
            .expect("derive key");

        assert!(
            registry
                .existing_session_shard(&SessionId::from("pure-key"))
                .is_none(),
            "key derivation must remain a pure read with no registration write"
        );
        assert!(
            registry
                .outstanding_for_session(&session_id)
                .expect("list derived-only session")
                .is_empty()
        );
        assert!(
            registry
                .outstanding_for_session(&SessionId::from("unknown-native-session"))
                .expect("list unknown session")
                .is_empty()
        );
        assert_eq!(
            registry.counts(),
            (0, 0, 0),
            "administrative reads must not materialize native state"
        );
    }

    #[tokio::test]
    async fn completed_turn_control_entries_leave_the_live_registry_and_are_bounded() {
        let registry = AwaitEventRegistry::with_limits(2, 2);
        let scope = turn_scope(
            &SessionId::from("bounded-turn-control"),
            &TurnId::from("turn-1"),
        );
        let gate = registry
            .key_for(&scope, AwaitEventWaitIdentity::TurnCancelGate)
            .expect("gate key");
        let terminal = registry
            .key_for(&scope, AwaitEventWaitIdentity::TurnTerminal)
            .expect("terminal key");

        registry
            .resolve(
                &gate,
                Resolution::Ok(serde_json::json!({ "gate": "sealed" })),
            )
            .expect("resolve gate");
        registry
            .resolve(
                &terminal,
                Resolution::Ok(serde_json::json!({ "terminal": "done" })),
            )
            .expect("resolve terminal");

        assert!(!registry.has_entry(&gate));
        assert!(!registry.has_entry(&terminal));
        assert!(registry.is_completed_turn_control(&gate));
        assert!(registry.is_completed_turn_control(&terminal));
        assert!(matches!(
            registry
                .await_resolution(
                    &terminal,
                    CancellationToken::new(),
                    None,
                    &crate::SystemClock,
                )
                .await
                .expect("reattach completed terminal"),
            Resolution::Ok(_)
        ));

        for ordinal in 2..=3 {
            let scope = turn_scope(
                &SessionId::from("bounded-turn-control"),
                &TurnId::from(format!("turn-{ordinal}")),
            );
            let gate = registry
                .key_for(&scope, AwaitEventWaitIdentity::TurnCancelGate)
                .expect("next gate key");
            let terminal = registry
                .key_for(&scope, AwaitEventWaitIdentity::TurnTerminal)
                .expect("next terminal key");
            registry
                .resolve(&gate, Resolution::Ok(serde_json::json!("sealed")))
                .expect("resolve next gate");
            registry
                .resolve(&terminal, Resolution::Ok(serde_json::json!("done")))
                .expect("resolve next terminal");
        }
        assert_eq!(registry.counts(), (0, 2, 0));
    }

    /// Archiving replaces a live slot rather than duplicating the promise in
    /// a second collection: the key id occupies exactly one archived slot,
    /// and a resolve after archival reads that archived terminal back.
    #[test]
    fn archiving_leaves_one_archived_slot_which_late_resolves_read() {
        let registry = AwaitEventRegistry::new();
        let scope = turn_scope(&SessionId::from("archive-slot"), &TurnId::from("turn"));
        let gate = registry
            .key_for(&scope, AwaitEventWaitIdentity::TurnCancelGate)
            .expect("gate key");
        let terminal = registry
            .key_for(&scope, AwaitEventWaitIdentity::TurnTerminal)
            .expect("terminal key");

        registry
            .resolve(&gate, Resolution::Ok(serde_json::json!("sealed")))
            .expect("resolve gate");
        let terminal_resolution = Resolution::Ok(serde_json::json!({ "done": true }));
        registry
            .resolve(&terminal, terminal_resolution.clone())
            .expect("resolve terminal");

        let shard = registry.shard_for_scope(&scope);
        let state = AwaitEventRegistry::locked_state(&shard);
        for key in [&gate, &terminal] {
            assert!(
                matches!(
                    state.promises.get(&key.key_id),
                    Some(PromiseSlot::ArchivedTurnControl(_))
                ),
                "archiving leaves exactly one archived slot per key id"
            );
        }
        drop(state);

        let ResolveOutcome::AlreadyResolved { terminal: stored } = registry
            .resolve(&terminal, Resolution::Ok(serde_json::json!("late-write")))
            .expect("resolve after archival")
        else {
            panic!("a resolve after archival must report the stored terminal")
        };
        assert_eq!(stored, terminal_resolution);
    }

    #[tokio::test]
    async fn turn_control_waiter_cancellation_never_resolves_the_gate() {
        let registry = AwaitEventRegistry::with_limits(2, 2);
        let gate = registry
            .key_for(
                &turn_scope(&SessionId::from("waiter-cancel"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .expect("gate key");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = registry
            .await_resolution(&gate, cancel, None, &crate::SystemClock)
            .await
            .expect_err("cancelled waiter must stop without resolving");
        assert_eq!(error.code.as_str(), "turn_control_wait_cancelled");
        assert!(matches!(
            registry
                .resolve(&gate, Resolution::Ok(serde_json::json!("real-writer")))
                .expect("real gate writer"),
            ResolveOutcome::Accepted
        ));
    }

    #[tokio::test]
    async fn session_revoke_drains_entries_and_bounds_tombstones() {
        let registry = AwaitEventRegistry::with_limits(2, 2);
        let key = registry
            .key_for(
                &turn_scope(&SessionId::from("revoke-1"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::TurnTerminal,
            )
            .expect("terminal key");
        let waiter_cancel = CancellationToken::new();
        let wait = registry.await_resolution(&key, waiter_cancel, None, &crate::SystemClock);
        tokio::pin!(wait);
        tokio::select! {
            result = &mut wait => panic!("wait unexpectedly completed: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        registry
            .revoke_session(&SessionId::from("revoke-1"))
            .expect("revoke session");
        assert!(wait.await.is_err());
        assert_eq!(registry.counts().0, 0);

        registry
            .revoke_session(&SessionId::from("revoke-2"))
            .expect("second revoke");
        registry
            .revoke_session(&SessionId::from("revoke-3"))
            .expect("third revoke");
        assert_eq!(registry.counts().2, 2);
    }

    #[test]
    fn verified_signatures_are_cached_with_live_entries() {
        let registry = AwaitEventRegistry::new();
        let key = registry
            .key_for(
                &turn_scope(&SessionId::from("signature-cache"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("await-event key");
        registry
            .resolve(&key, Resolution::Ok(serde_json::json!("done")))
            .expect("resolve key");
        assert_eq!(
            registry
                .verify_uncached_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        for _ in 0..10 {
            assert!(
                registry
                    .peek_resolution(&key)
                    .expect("cached peek")
                    .is_some()
            );
        }
        assert_eq!(
            registry
                .verify_uncached_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "resolved entry peeks must not recompute HMAC signatures"
        );

        let mut tampered = key;
        tampered.signature.push('0');
        assert!(registry.peek_resolution(&tampered).is_err());
        assert_eq!(
            registry
                .verify_uncached_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a cached key mismatch must be rejected without replacing the verified signature"
        );
    }

    #[test]
    fn different_sessions_do_not_share_the_registry_mutex() {
        let registry = Arc::new(AwaitEventRegistry::new());
        let key_a = registry
            .key_for(
                &turn_scope(&SessionId::from("shard-a"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("session A key");
        let key_b = registry
            .key_for(
                &turn_scope(&SessionId::from("shard-b"), &TurnId::from("turn")),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("session B key");
        let shard_a = registry.shard_for_scope(&key_a.scope);
        let _held_a = AwaitEventRegistry::locked_state(&shard_a);
        let (tx, rx) = std::sync::mpsc::channel();
        let registry_b = Arc::clone(&registry);
        std::thread::spawn(move || {
            let result = registry_b.resolve(&key_b, Resolution::Ok(serde_json::json!("done")));
            tx.send(result).expect("return session B result");
        });

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("session B must not wait for session A's mutex")
                .expect("resolve session B"),
            ResolveOutcome::Accepted
        ));
    }

    #[test]
    #[ignore = "manual lane-O await-event contention measurement"]
    fn measure_concurrent_turn_registry_contention() {
        const THREADS: usize = 8;
        const PEEKS_PER_THREAD: usize = 25_000;
        let registry = Arc::new(AwaitEventRegistry::new());
        let keys = (0..THREADS)
            .map(|ordinal| {
                let key = registry
                    .key_for(
                        &turn_scope(
                            &SessionId::from(format!("perf-session-{ordinal}")),
                            &TurnId::from("turn"),
                        ),
                        AwaitEventWaitIdentity::tool_completion("tool"),
                    )
                    .expect("perf key");
                registry
                    .resolve(&key, Resolution::Ok(serde_json::json!(ordinal)))
                    .expect("seed resolution");
                key
            })
            .collect::<Vec<_>>();
        let barrier = Arc::new(Barrier::new(THREADS + 1));
        let started = Instant::now();
        std::thread::scope(|scope| {
            for key in keys {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..PEEKS_PER_THREAD {
                        assert!(
                            registry
                                .peek_resolution(&key)
                                .expect("peek resolution")
                                .is_some()
                        );
                    }
                });
            }
            barrier.wait();
        });
        let elapsed = started.elapsed();
        let operations = THREADS * PEEKS_PER_THREAD;
        eprintln!(
            "await-event contention: threads={THREADS} operations={operations} elapsed_ms={:.3} ns_per_op={:.3} ops_per_sec={:.0}",
            elapsed.as_secs_f64() * 1_000.0,
            elapsed.as_nanos() as f64 / operations as f64,
            operations as f64 / elapsed.as_secs_f64(),
        );
        assert!(elapsed < Duration::from_secs(60));
    }
}

/// FIG-1161 seam 5, model-checked: the waiter registers its `Notify`
/// subscription (`Notified::enable`) while the shard lock is still held, so
/// a resolver or revoker that notifies under the same lock cannot land in
/// the gap between the pending check and the registration. Loom must
/// explore every interleaving; the waiter must observe the terminal (or the
/// revocation error) in all of them — a lost wake would leave
/// `loom::future::block_on` parked forever and deadlock the model.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    fn loom_key(registry: &AwaitEventRegistry, session_id: &str) -> AwaitEventKey {
        registry
            .key_for(
                &ExecutionScope::turn(&SessionId::from(session_id), &crate::TurnId::from("turn")),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("key derivation")
    }

    #[test]
    fn resolution_racing_waiter_registration_is_never_stranded() {
        loom::model(|| {
            let registry = Arc::new(AwaitEventRegistry::new());
            let key = loom_key(&registry, "loom-resolve");

            let resolver = {
                let registry = Arc::clone(&registry);
                let key = key.clone();
                loom::thread::spawn(move || {
                    registry
                        .resolve(&key, Resolution::Ok(serde_json::json!("done")))
                        .expect("resolve");
                })
            };
            let waiter = {
                let registry = Arc::clone(&registry);
                loom::thread::spawn(move || {
                    loom::future::block_on(registry.await_resolution_inner(
                        &key,
                        CancellationToken::new(),
                        None,
                        &crate::SystemClock,
                    ))
                })
            };

            let observed = waiter.join().expect("waiter thread");
            resolver.join().expect("resolver thread");
            assert!(
                matches!(observed, Ok(Resolution::Ok(_))),
                "the waiter must observe the terminal resolution in every \
                 interleaving: {observed:?}"
            );
        });
    }

    #[test]
    fn revocation_racing_waiter_registration_always_wakes_the_waiter() {
        loom::model(|| {
            let registry = Arc::new(AwaitEventRegistry::new());
            let key = loom_key(&registry, "loom-revoke");

            let revoker = {
                let registry = Arc::clone(&registry);
                loom::thread::spawn(move || {
                    registry
                        .revoke_session(&SessionId::from("loom-revoke"))
                        .expect("revoke session");
                })
            };
            let waiter = {
                let registry = Arc::clone(&registry);
                loom::thread::spawn(move || {
                    loom::future::block_on(registry.await_resolution_inner(
                        &key,
                        CancellationToken::new(),
                        None,
                        &crate::SystemClock,
                    ))
                })
            };

            let observed = waiter.join().expect("waiter thread");
            revoker.join().expect("revoker thread");
            assert!(
                observed.is_err(),
                "a revoked session's waiter must stop with an error in every \
                 interleaving: {observed:?}"
            );
        });
    }
}
