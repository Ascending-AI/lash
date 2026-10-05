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
//! A body holds only the sealed [`crate::AttemptContext`], which has no route
//! to the journal. This sentinel closes the *class*: it wraps a controller and
//! records every crossing of the controller boundary that happens while a
//! `ToolAttempt` effect is open on that same controller, whatever route
//! produced it. A new capability that reaches the journal from inside an
//! attempt shows up as an undeclared crossing without anyone writing a bespoke
//! law for it.
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

use crate::SessionId;
use std::sync::{Arc, Mutex};

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
    let kind = envelope.command.kind().as_str();
    let effect_id = envelope.invocation.effect_id();
    format!("execute_effect:{kind}:{effect_id}")
}

#[async_trait::async_trait]
impl AwaitEventResolver for AttemptAtomicitySentinel<'_> {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
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
    ) -> Result<Resolution, RuntimeError> {
        self.ledger
            .record(format!("await_await_event:{}", key.key_id));
        self.inner.await_await_event(key, cancel).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.ledger
            .record(format!("revoke_await_events_for_session:{session_id}"));
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.ledger
            .record(format!("cancel_await_events_for_session:{session_id}"));
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for AttemptAtomicitySentinel<'_> {
    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.inner.attempt_observation()
    }

    fn hands_over_turns(&self) -> bool {
        self.inner.hands_over_turns()
    }

    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.inner.wants_segment_boundary(progress)
    }

    async fn observe_process_cancel(
        &self,
        lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        self.inner.observe_process_cancel(lent_stop).await
    }

    async fn record_process_drive_step(
        &self,
        name: String,
        step: crate::ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.record_process_drive_step(name, step).await
    }

    fn run_record_observer(&self) -> Option<&crate::trace::RunRecordObserver> {
        self.inner.run_record_observer()
    }

    async fn record_run_schedule(
        &self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.ledger.record(format!("run_schedule:{name}"));
        self.inner.record_run_schedule(name, step).await
    }

    fn start_run_attempt<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        self.ledger.record(format!("run_attempt:{name}"));
        self.inner.start_run_attempt(name, step)
    }

    fn start_run_prepare<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunStartPrepareStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_dispatch::RunStartPrepared> {
        self.inner.start_run_prepare(name, step)
    }

    async fn issue_run_realization<'run>(
        &'run self,
        request: crate::tool_dispatch::RealizationRequest,
    ) -> Result<crate::tool_dispatch::IssuedRealization<'run>, RuntimeEffectControllerError> {
        self.ledger
            .record(format!("run_realization:{}", request.key));
        self.inner.issue_run_realization(request).await
    }

    /// Attach to previously issued protected work without sending or executing it again.
    async fn attach_run_realization<'run>(
        &'run self,
        invocation_id: String,
    ) -> Result<
        crate::tool_dispatch::RunSelectable<'run, crate::tool_dispatch::RealizationReceipt>,
        RuntimeEffectControllerError,
    > {
        self.inner.attach_run_realization(invocation_id).await
    }

    fn start_run_retry(&self, backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        self.ledger.record(format!("run_retry:{backoff_ms}"));
        self.inner.start_run_retry(backoff_ms)
    }

    async fn arm_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.ledger.record("arm_run_source".to_owned());
        self.inner.arm_run_source(descriptor).await
    }
    async fn attach_run_process_terminal(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.ledger.record("attach_run_process_terminal".to_owned());
        self.inner.attach_run_process_terminal(descriptor).await
    }
    async fn cancel_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, RuntimeEffectControllerError> {
        self.ledger.record("cancel_run_source".to_owned());
        self.inner.cancel_run_source(descriptor).await
    }
    async fn await_run_sources(
        &self,
        subscriptions: Vec<crate::tool_run::SourceSubscription>,
        cancel: crate::TurnCancelWait,
    ) -> Result<(usize, crate::tool_run::SourceSeal), RuntimeEffectControllerError> {
        self.ledger.record("await_run_sources".to_owned());
        self.inner.await_run_sources(subscriptions, cancel).await
    }

    async fn record_run_record(
        &self,
        name: String,
        step: crate::RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        // A Run record journals a command: one issued from inside an open
        // attempt is a crossing like any effect's.
        self.ledger.record(format!("run_record:{name}"));
        self.inner.record_run_record(name, step).await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let effect_kind = envelope.command.kind();
        let opens_attempt = effect_kind == RuntimeEffectKind::ToolAttempt;
        let crossing = effect_crossing_label(&envelope);
        self.ledger.record_intent_crossing(
            Some(effect_kind),
            envelope.invocation.replay_attribution(),
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

    async fn read_recorded_journal(
        &self,
        range: &crate::RecordedKeyRange,
    ) -> Result<crate::RecordedJournal, crate::RuntimeEffectControllerError> {
        self.inner.read_recorded_journal(range).await
    }
}
