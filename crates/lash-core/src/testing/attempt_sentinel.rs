//! The attempt-atomicity sentinel: a structural catch-all for nested journal
//! commands emitted from inside a recorded `ToolAttempt` body.
//!
//! A controller-owned tier records a whole tool attempt as one journal entry.
//! Redrive replays that recorded entry *without re-entering the body*, so any
//! journal command the body emitted while it ran is still in the journal but is
//! never re-issued — the handler's next command meets the recorded inner
//! command at the wrong ordinal and an ordinal-addressed engine rejects the
//! invocation (Restate `RT0016`).
//!
//! The guards in [`crate::ToolContext`] close that hazard route by route. This
//! sentinel closes the *class*: it wraps a controller and records every
//! crossing of the controller boundary that happens while a `ToolAttempt`
//! effect is open on that same controller, whatever route produced it. A new
//! `ToolContext` capability that reaches the journal from inside an attempt
//! shows up as an undeclared crossing without anyone writing a bespoke law for
//! it.
//!
//! Depth is tracked on the controller instance rather than on a task-local,
//! because that is the semantically correct question: a journal is one ordered
//! context, and "a command crossed this controller while a recorded attempt was
//! open on it" holds regardless of which task issued it (in-turn emissions
//! frequently hop tasks — the tool-attempt local executor crosses a spawned
//! task boundary, and in-turn effects may be forwarded to an effect-controller
//! driver task).
//!
//! A crossing is *not* automatically a defect: some crossings are pure
//! derivations that issue no engine command (`completion_key`'s await-event key
//! derivation on Restate, FIG-1126). The sentinel therefore reports the
//! crossings; each matrix row declares the literal crossings it expects, and
//! the ordinal-shift question itself is settled at the endpoint tier against
//! real captured journal bytes.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

use crate::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, ExecutionScope,
    Resolution, ResolveOutcome, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeError, SegmentProgress,
};

#[derive(Default)]
struct LedgerState {
    open_attempts: usize,
    attempt_bodies_opened: usize,
    crossings: Vec<String>,
    intent_crossings: std::collections::BTreeMap<String, Vec<String>>,
    /// Process commands without structural attribution are included in every
    /// intent query. Losing metadata must make the one-command law fail by
    /// over-counting; it must never hide a second command.
    unattributed_process_crossings: Vec<String>,
}

/// Records controller-boundary crossings observed while a recorded
/// `ToolAttempt` was open.
///
/// Crossings are rendered as stable labels so tests can compare them against
/// literal expectations instead of recomputing them.
#[derive(Default)]
pub struct NestedJournalLedger {
    state: Mutex<LedgerState>,
}

impl NestedJournalLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn open_attempt(&self) {
        let mut state = self.state.lock_recover();
        state.open_attempts += 1;
        state.attempt_bodies_opened += 1;
    }

    fn close_attempt(&self) {
        let mut state = self.state.lock_recover();
        state.open_attempts = state.open_attempts.saturating_sub(1);
    }

    fn record(&self, crossing: String) {
        let mut state = self.state.lock_recover();
        if state.open_attempts == 0 {
            return;
        }
        state.crossings.push(crossing);
    }

    fn record_intent_crossing(
        &self,
        kind: Option<RuntimeEffectKind>,
        attribution: Option<&crate::RuntimeReplayAttribution>,
        crossing: &str,
    ) {
        let Some(crate::RuntimeReplayAttribution::ToolIntent(identity)) = attribution else {
            if kind == Some(RuntimeEffectKind::Process) {
                self.state
                    .lock_recover()
                    .unattributed_process_crossings
                    .push(crossing.to_string());
            }
            return;
        };
        self.state
            .lock_recover()
            .intent_crossings
            .entry(identity.replay_key.clone())
            .or_default()
            .push(crossing.to_string());
    }

    /// Every crossing recorded while a recorded attempt was open, in order.
    pub fn crossings_inside_attempt(&self) -> Vec<String> {
        self.state.lock_recover().crossings.clone()
    }

    /// How many recorded `ToolAttempt` bodies this controller opened.
    pub fn attempt_bodies_opened(&self) -> usize {
        self.state.lock_recover().attempt_bodies_opened
    }

    /// Whether any journal command crossed the controller from inside a
    /// recorded attempt body.
    pub fn tripped(&self) -> bool {
        !self.state.lock_recover().crossings.is_empty()
    }

    /// Controller crossings attributed to one derived tool-intent replay key.
    pub fn crossings_for_intent(&self, replay_key: &str) -> Vec<String> {
        let state = self.state.lock_recover();
        let mut crossings = state
            .intent_crossings
            .get(replay_key)
            .cloned()
            .unwrap_or_default();
        crossings.extend(state.unattributed_process_crossings.iter().cloned());
        crossings
    }
}

/// A controller decorator that records nested journal-command emissions from
/// inside recorded `ToolAttempt` bodies.
///
/// Wrap the tier's real controller and hand the sentinel to
/// [`crate::ScopedEffectController::borrowed`] so every in-turn capability
/// reaches the journal through it.
pub struct AttemptAtomicitySentinel<'run> {
    inner: &'run dyn RuntimeEffectController,
    ledger: Arc<NestedJournalLedger>,
}

impl<'run> AttemptAtomicitySentinel<'run> {
    pub fn new(inner: &'run dyn RuntimeEffectController, ledger: Arc<NestedJournalLedger>) -> Self {
        Self { inner, ledger }
    }

    pub fn ledger(&self) -> Arc<NestedJournalLedger> {
        Arc::clone(&self.ledger)
    }
}

fn effect_crossing_label(envelope: &RuntimeEffectEnvelope) -> String {
    let kind = envelope
        .invocation
        .effect_kind()
        .map(RuntimeEffectKind::as_str)
        .unwrap_or("no_effect_kind");
    let effect_id = envelope.invocation.effect_id().unwrap_or("no_effect_id");
    format!("execute_effect:{kind}:{effect_id}")
}

#[async_trait::async_trait]
impl AwaitEventResolver for AttemptAtomicitySentinel<'_> {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.inner.replay_ownership()
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        self.inner.journal_addressing()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.inner.allows_process_lifetime_completion_keys()
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.ledger
            .record(format!("await_event_key:{}", scope.id()));
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.ledger
            .record(format!("resolve_await_event:{}", key.key_id));
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.ledger
            .record(format!("peek_await_event:{}", key.key_id));
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.ledger
            .record(format!("await_await_event:{}", key.key_id));
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.ledger
            .record(format!("revoke_await_events_for_session:{session_id}"));
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.ledger
            .record(format!("cancel_await_events_for_session:{session_id}"));
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for AttemptAtomicitySentinel<'_> {
    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.inner.wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.inner.supports_concurrent_effects()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let opens_attempt =
            envelope.invocation.effect_kind() == Some(RuntimeEffectKind::ToolAttempt);
        let crossing = effect_crossing_label(&envelope);
        self.ledger.record_intent_crossing(
            envelope.invocation.effect_kind(),
            envelope
                .invocation
                .replay
                .as_ref()
                .and_then(|replay| replay.attribution.as_ref()),
            &crossing,
        );
        self.ledger.record(crossing);
        if opens_attempt {
            self.ledger.open_attempt();
        }
        let outcome = self.inner.execute_effect(envelope, local_executor).await;
        if opens_attempt {
            self.ledger.close_attempt();
        }
        outcome
    }
}
