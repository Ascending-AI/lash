//! In-process await-event (Durable Wait) registry backing the inline effect
//! host and controller.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use hmac::{Hmac, Mac};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::RuntimeError;

use super::executor::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome,
};
use super::promise_semantics::{
    PromiseState, PromiseTransition, SessionRevocationTransition, cancel_sweep, constant_time_eq,
    derive_key_id, resolve, revoke_session, session_allows_access, sign_material,
};

type HmacSha256 = Hmac<sha2::Sha256>;

const COMPLETED_TURN_CONTROL_KEY_LIMIT: usize = 4_096;
const REVOKED_SESSION_LIMIT: usize = 4_096;

#[derive(Debug)]
struct AwaitEventEntry {
    verified_key: AwaitEventKey,
    terminal: Option<Resolution>,
    notify: Arc<Notify>,
}

impl AwaitEventEntry {
    fn for_key(key: &AwaitEventKey) -> Self {
        Self {
            verified_key: key.clone(),
            terminal: None,
            notify: Arc::new(Notify::new()),
        }
    }
}

#[derive(Debug)]
struct AwaitEventRegistryState {
    entries: HashMap<String, AwaitEventEntry>,
    completed_turn_control: HashMap<String, CompletedTurnControlEntry>,
    completed_turn_control_order: VecDeque<String>,
    revoked: bool,
}

#[derive(Debug)]
struct CompletedTurnControlEntry {
    verified_key: AwaitEventKey,
    terminal: Resolution,
}

impl AwaitEventRegistryState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            completed_turn_control: HashMap::new(),
            completed_turn_control_order: VecDeque::new(),
            revoked: false,
        }
    }
}

type AwaitEventRegistryShard = Arc<std::sync::Mutex<AwaitEventRegistryState>>;

#[derive(Debug)]
pub(super) struct AwaitEventRegistry {
    secret: Vec<u8>,
    session_shards: RwLock<HashMap<String, AwaitEventRegistryShard>>,
    unscoped_shard: AwaitEventRegistryShard,
    revoked_session_order: std::sync::Mutex<VecDeque<String>>,
    completed_turn_control_key_limit: usize,
    revoked_session_limit: usize,
    #[cfg(test)]
    verify_uncached_calls: std::sync::atomic::AtomicUsize,
}

impl AwaitEventRegistry {
    pub(super) fn new() -> Self {
        Self::with_limits(COMPLETED_TURN_CONTROL_KEY_LIMIT, REVOKED_SESSION_LIMIT)
    }

    fn with_limits(completed_turn_control_key_limit: usize, revoked_session_limit: usize) -> Self {
        Self {
            secret: uuid::Uuid::new_v4().as_bytes().to_vec(),
            session_shards: RwLock::new(HashMap::new()),
            unscoped_shard: Arc::new(std::sync::Mutex::new(AwaitEventRegistryState::new())),
            revoked_session_order: std::sync::Mutex::new(VecDeque::new()),
            completed_turn_control_key_limit,
            revoked_session_limit,
            #[cfg(test)]
            verify_uncached_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn shard_for_session(&self, session_id: &str) -> Result<AwaitEventRegistryShard, RuntimeError> {
        if let Some(shard) = self
            .session_shards
            .read()
            .map_err(|_| Self::poisoned())?
            .get(session_id)
            .cloned()
        {
            return Ok(shard);
        }
        let mut shards = self.session_shards.write().map_err(|_| Self::poisoned())?;
        Ok(Arc::clone(
            shards
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(std::sync::Mutex::new(AwaitEventRegistryState::new()))),
        ))
    }

    fn existing_session_shard(
        &self,
        session_id: &str,
    ) -> Result<Option<AwaitEventRegistryShard>, RuntimeError> {
        Ok(self
            .session_shards
            .read()
            .map_err(|_| Self::poisoned())?
            .get(session_id)
            .cloned())
    }

    fn shard_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<AwaitEventRegistryShard, RuntimeError> {
        match scope.session_id() {
            Some(session_id) => self.shard_for_session(session_id),
            None => Ok(Arc::clone(&self.unscoped_shard)),
        }
    }

    fn locked_state(
        shard: &AwaitEventRegistryShard,
    ) -> Result<std::sync::MutexGuard<'_, AwaitEventRegistryState>, RuntimeError> {
        shard.lock().map_err(|_| Self::poisoned())
    }

    fn poisoned() -> RuntimeError {
        RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventRegistryPoisoned,
            "await-event registry lock poisoned",
        )
    }

    pub(super) fn key_for(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        wait.validate()?;
        match scope.session_id() {
            Some(session_id) => {
                if let Some(shard) = self.existing_session_shard(session_id)? {
                    let state = Self::locked_state(&shard)?;
                    if !session_allows_access(state.revoked) {
                        return Err(Self::unknown_or_revoked());
                    }
                }
            }
            None => {
                let state = Self::locked_state(&self.unscoped_shard)?;
                if !session_allows_access(state.revoked) {
                    return Err(Self::unknown_or_revoked());
                }
            }
        }
        self.derive_key(scope, wait)
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

    pub(super) fn resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !self.verify_uncached(key)? {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        let shard = self.shard_for_scope(&key.scope)?;
        let mut state = Self::locked_state(&shard)?;
        let session_state = state.revoked.then_some(PromiseState::Revoked);
        if let Some(transition) = session_state.map(|state| resolve(state, resolution.clone())) {
            return Ok(transition
                .resolve_outcome()
                .expect("revoked resolve always has a public outcome"));
        }
        if let Some(completed) = state.completed_turn_control.get(&key.key_id) {
            if !Self::verified_key_matches(&completed.verified_key, key) {
                return Ok(ResolveOutcome::UnknownOrRevoked);
            }
            return Ok(resolve(
                PromiseState::Resolved(completed.terminal.clone()),
                resolution,
            )
            .resolve_outcome()
            .expect("resolved promise always has a public outcome"));
        }
        if let Some(entry) = state.entries.get_mut(&key.key_id) {
            if !Self::verified_key_matches(&entry.verified_key, key) {
                return Ok(ResolveOutcome::UnknownOrRevoked);
            }
            let observed = entry
                .terminal
                .clone()
                .map_or(PromiseState::Pending, PromiseState::Resolved);
            match resolve(observed, resolution) {
                PromiseTransition::Store(terminal) => {
                    entry.terminal = Some(terminal);
                    entry.notify.notify_waiters();
                }
                transition => {
                    return Ok(transition
                        .resolve_outcome()
                        .expect("normal resolve always has a public outcome"));
                }
            }
        } else {
            let mut entry = AwaitEventEntry::for_key(key);
            let PromiseTransition::Store(terminal) = resolve(PromiseState::Missing, resolution)
            else {
                unreachable!("missing promise always buffers the proposed terminal")
            };
            entry.terminal = Some(terminal);
            entry.notify.notify_waiters();
            state.entries.insert(key.key_id.clone(), entry);
        }
        if matches!(key.wait, AwaitEventWaitIdentity::TurnTerminal) {
            self.archive_turn_control(&mut state, key)?;
        }
        Ok(ResolveOutcome::Accepted)
    }

    pub(super) fn peek_resolution(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        let shard = self.shard_for_scope(&key.scope)?;
        let state = Self::locked_state(&shard)?;
        if state.revoked {
            return Err(Self::unknown_or_revoked());
        }
        if let Some(completed) = state.completed_turn_control.get(&key.key_id) {
            if !Self::verified_key_matches(&completed.verified_key, key) {
                return Err(Self::unknown_or_revoked());
            }
            return Ok(Some(completed.terminal.clone()));
        }
        if let Some(entry) = state.entries.get(&key.key_id) {
            if !Self::verified_key_matches(&entry.verified_key, key) {
                return Err(Self::unknown_or_revoked());
            }
            return Ok(entry.terminal.clone());
        }
        if !self.verify_uncached(key)? {
            return Err(Self::unknown_or_revoked());
        }
        Ok(None)
    }

    fn archive_turn_control(
        &self,
        state: &mut AwaitEventRegistryState,
        terminal_key: &AwaitEventKey,
    ) -> Result<(), RuntimeError> {
        let gate_key =
            self.derive_key(&terminal_key.scope, AwaitEventWaitIdentity::TurnCancelGate)?;
        for key_id in [&gate_key.key_id, &terminal_key.key_id] {
            let Some(entry) = state.entries.remove(key_id) else {
                continue;
            };
            let Some(terminal) = entry.terminal else {
                continue;
            };
            if state
                .completed_turn_control
                .insert(
                    key_id.clone(),
                    CompletedTurnControlEntry {
                        verified_key: entry.verified_key,
                        terminal,
                    },
                )
                .is_none()
            {
                state.completed_turn_control_order.push_back(key_id.clone());
            }
        }
        while state.completed_turn_control.len() > self.completed_turn_control_key_limit {
            let Some(key_id) = state.completed_turn_control_order.pop_front() else {
                break;
            };
            state.completed_turn_control.remove(&key_id);
        }
        Ok(())
    }

    pub(super) async fn await_resolution(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        clock: &dyn crate::Clock,
    ) -> Result<Resolution, RuntimeError> {
        if let Some(resolution) = self.peek_resolution(key)? {
            return Ok(resolution);
        }
        crate::runtime::process_worker::release_process_execution_permit_while(
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
        let shard = self.shard_for_scope(&key.scope)?;
        loop {
            let notify = {
                let mut state = Self::locked_state(&shard)?;
                if state.revoked {
                    return Err(Self::unknown_or_revoked());
                }
                if let Some(completed) = state.completed_turn_control.get(&key.key_id) {
                    if !Self::verified_key_matches(&completed.verified_key, key) {
                        return Err(Self::unknown_or_revoked());
                    }
                    return Ok(completed.terminal.clone());
                }
                if let Some(entry) = state.entries.get(&key.key_id)
                    && !Self::verified_key_matches(&entry.verified_key, key)
                {
                    return Err(Self::unknown_or_revoked());
                }
                if !state.entries.contains_key(&key.key_id) {
                    if !self.verify_uncached(key)? {
                        return Err(Self::unknown_or_revoked());
                    }
                    state
                        .entries
                        .insert(key.key_id.clone(), AwaitEventEntry::for_key(key));
                }
                let entry = state
                    .entries
                    .get(&key.key_id)
                    .expect("await-event entry inserted above");
                if let Some(terminal) = entry.terminal.clone() {
                    return Ok(terminal);
                }
                Arc::clone(&entry.notify)
            };
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitCancelled,
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled)?;
                    }
                    _ = clock.sleep_until(deadline) => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitTimeout,
                                "turn-control waiter timed out without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Timeout)?;
                    }
                    _ = notify.notified() => {}
                }
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(crate::RuntimeErrorCode::TurnControlWaitCancelled,
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled)?;
                    }
                    _ = notify.notified() => {}
                }
            }
        }
    }

    /// Revoke every await-event for `session_id`: in-flight waiters error with
    /// `await_event_unknown_or_revoked`, matching entries are drained, and a
    /// bounded recent-session tombstone cache rejects keys created after
    /// deletion without growing for the lifetime of the process.
    pub(super) fn revoke_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let shard = self.shard_for_session(session_id)?;
        let newly_revoked = {
            let mut state = Self::locked_state(&shard)?;
            let transition = revoke_session(state.revoked);
            let newly_revoked = transition == SessionRevocationTransition::MarkRevoked;
            state.revoked = true;
            for entry in state.entries.values() {
                entry.notify.notify_waiters();
            }
            state.entries.clear();
            state.completed_turn_control.clear();
            state.completed_turn_control_order.clear();
            newly_revoked
        };
        if !newly_revoked {
            return Ok(());
        }
        let expired = {
            let mut order = self
                .revoked_session_order
                .lock()
                .map_err(|_| Self::poisoned())?;
            order.push_back(session_id.to_string());
            let mut expired = Vec::new();
            while order.len() > self.revoked_session_limit {
                if let Some(session_id) = order.pop_front() {
                    expired.push(session_id);
                }
            }
            expired
        };
        if !expired.is_empty() {
            let mut shards = self.session_shards.write().map_err(|_| Self::poisoned())?;
            for session_id in expired {
                shards.remove(&session_id);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn counts(&self) -> Result<(usize, usize, usize), RuntimeError> {
        let mut shards = self
            .session_shards
            .read()
            .map_err(|_| Self::poisoned())?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        shards.push(Arc::clone(&self.unscoped_shard));
        let mut counts = (0, 0, 0);
        for shard in shards {
            let state = Self::locked_state(&shard)?;
            counts.0 += state.entries.len();
            counts.1 += state.completed_turn_control.len();
            counts.2 += usize::from(state.revoked);
        }
        Ok(counts)
    }

    #[cfg(test)]
    fn has_entry(&self, key: &AwaitEventKey) -> Result<bool, RuntimeError> {
        let shard = self.shard_for_scope(&key.scope)?;
        let state = Self::locked_state(&shard)?;
        Ok(state.entries.contains_key(&key.key_id))
    }

    #[cfg(test)]
    fn is_completed_turn_control(&self, key: &AwaitEventKey) -> Result<bool, RuntimeError> {
        let shard = self.shard_for_scope(&key.scope)?;
        let state = Self::locked_state(&shard)?;
        Ok(state.completed_turn_control.contains_key(&key.key_id))
    }

    /// Resolve every *outstanding* wait for `session_id` with
    /// [`Resolution::Cancelled`], leaving the session usable: already-terminal
    /// waits keep their terminal, and waits registered afterwards behave
    /// normally. This is the standalone host lever, in contrast to the
    /// tombstoning [`revoke_session`](Self::revoke_session).
    pub(super) fn cancel_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let Some(shard) = self.existing_session_shard(session_id)? else {
            return Ok(());
        };
        let mut state = Self::locked_state(&shard)?;
        for entry in state.entries.values_mut() {
            let observed = entry
                .terminal
                .clone()
                .map_or(PromiseState::Pending, PromiseState::Resolved);
            if let PromiseTransition::Store(terminal) =
                cancel_sweep(&entry.verified_key.wait, observed)
            {
                entry.terminal = Some(terminal);
                entry.notify.notify_waiters();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Duration;

    fn turn_scope(session_id: &str, turn_id: &str) -> ExecutionScope {
        ExecutionScope::turn(session_id, turn_id)
    }

    #[test]
    fn key_derivation_does_not_register_or_materialize_session_state() {
        let registry = AwaitEventRegistry::new();
        let scope = turn_scope("pure-key", "turn");

        registry
            .key_for(&scope, AwaitEventWaitIdentity::tool_completion("tool-call"))
            .expect("derive key");

        assert!(
            registry
                .existing_session_shard("pure-key")
                .expect("read registry")
                .is_none(),
            "key derivation must remain a pure read with no registration write"
        );
    }

    #[tokio::test]
    async fn completed_turn_control_entries_leave_the_live_registry_and_are_bounded() {
        let registry = AwaitEventRegistry::with_limits(2, 2);
        let scope = turn_scope("bounded-turn-control", "turn-1");
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

        assert!(!registry.has_entry(&gate).expect("gate entry lookup"));
        assert!(
            !registry
                .has_entry(&terminal)
                .expect("terminal entry lookup")
        );
        assert!(
            registry
                .is_completed_turn_control(&gate)
                .expect("completed gate lookup")
        );
        assert!(
            registry
                .is_completed_turn_control(&terminal)
                .expect("completed terminal lookup")
        );
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
            let scope = turn_scope("bounded-turn-control", &format!("turn-{ordinal}"));
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
        assert_eq!(registry.counts().expect("registry counts"), (0, 2, 0));
    }

    #[tokio::test]
    async fn turn_control_waiter_cancellation_never_resolves_the_gate() {
        let registry = AwaitEventRegistry::with_limits(2, 2);
        let gate = registry
            .key_for(
                &turn_scope("waiter-cancel", "turn"),
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
                &turn_scope("revoke-1", "turn"),
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
        registry.revoke_session("revoke-1").expect("revoke session");
        assert!(wait.await.is_err());
        assert_eq!(registry.counts().expect("drained counts").0, 0);

        registry.revoke_session("revoke-2").expect("second revoke");
        registry.revoke_session("revoke-3").expect("third revoke");
        assert_eq!(registry.counts().expect("bounded revokes").2, 2);
    }

    #[test]
    fn verified_signatures_are_cached_with_live_entries() {
        let registry = AwaitEventRegistry::new();
        let key = registry
            .key_for(
                &turn_scope("signature-cache", "turn"),
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
                &turn_scope("shard-a", "turn"),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("session A key");
        let key_b = registry
            .key_for(
                &turn_scope("shard-b", "turn"),
                AwaitEventWaitIdentity::tool_completion("tool"),
            )
            .expect("session B key");
        let shard_a = registry
            .shard_for_scope(&key_a.scope)
            .expect("session A shard");
        let _held_a = AwaitEventRegistry::locked_state(&shard_a).expect("lock session A");
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
                        &turn_scope(&format!("perf-session-{ordinal}"), "turn"),
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
