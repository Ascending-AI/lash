//! The FIG-3078 acceptance-window controller double, extracted from
//! `lease_and_claims.rs`.
//!
//! It lives here because the parent sits on the 2500-line test budget
//! `scripts/check-production-file-size.py` enforces, and FIG-2266's explicit
//! group methods pushed it over. A real module rather than an `include!`, so
//! `cargo fmt` keeps walking it.

use super::*;

/// The text the second tab admits while the first worker is being replaced.
pub(super) const LATE_TAB_INPUT: &str = "second tab input admitted after the block was journaled";

/// A journaling controller that validates a replayed envelope against its
/// recorded canonical form the way a durable substrate does, and that stages
/// the FIG-3078 race: admit, journal the turn's message block, admit a second
/// tab's `next_turn` input, then lose the worker.
pub(super) struct AcceptanceWindowJournalController {
    native: lash_core::facade_support::NativeRuntimeEffectController,
    journal: std::sync::Mutex<
        HashMap<
            String,
            (
                lash_core::facade_support::CanonicalRuntimeEffectEnvelope,
                lash_core::RuntimeEffectOutcome,
            ),
        >,
    >,
    late_admission: std::sync::Mutex<Option<Arc<RecordingStore>>>,
    admitted_late: std::sync::Mutex<Option<lash_core::PendingTurnInput>>,
    block_journaled: AtomicBool,
    kill_armed: AtomicBool,
    journaling: AtomicBool,
}

impl AcceptanceWindowJournalController {
    pub(super) fn new(store: Arc<RecordingStore>) -> Self {
        Self {
            native: lash_core::facade_support::NativeRuntimeEffectController::default(),
            journal: std::sync::Mutex::new(HashMap::new()),
            late_admission: std::sync::Mutex::new(Some(store)),
            admitted_late: std::sync::Mutex::new(None),
            block_journaled: AtomicBool::new(false),
            kill_armed: AtomicBool::new(true),
            journaling: AtomicBool::new(true),
        }
    }

    /// The replaced turn is settled; later turns run on a live worker with no
    /// journal of their own.
    pub(super) fn retire_journal(&self) {
        self.journaling.store(false, Ordering::SeqCst);
    }

    pub(super) fn late_input_id(&self) -> lash_core::InputId {
        self.admitted_late
            .lock_recover()
            .as_ref()
            .expect("the second tab's input was admitted")
            .input_id
            .clone()
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for AcceptanceWindowJournalController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("acceptance-window-journal:{:p}", self))
    }

    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.native
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for AcceptanceWindowJournalController {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        lash_core::EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if !self.journaling.load(Ordering::SeqCst) {
            return self.native.execute_effect(envelope, local_executor).await;
        }
        let replay_key = envelope.invocation.replay_key().to_string();
        let reconstructed = envelope.canonical_form()?;
        let recorded = {
            let journal = self.journal.lock_recover();
            journal.get(&replay_key).cloned()
        };
        if let Some((recorded, outcome)) = recorded {
            lash_core::facade_support::validate_replayed_effect_envelope(
                &recorded,
                &reconstructed,
                lash_core::RuntimeErrorCode::WorkerReplacementAbort,
                None,
            )?;
            return Ok(outcome);
        }
        if self.block_journaled.load(Ordering::SeqCst)
            && self.kill_armed.swap(false, Ordering::SeqCst)
        {
            return Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::WorkerReplacementAbort,
                "worker replaced after the turn's message block was journaled",
            ));
        }
        let journals_the_block = matches!(
            envelope.command,
            lash_core::RuntimeEffectCommand::LlmCall { .. }
        );
        let outcome = self.native.execute_effect(envelope, local_executor).await?;
        self.journal
            .lock_recover()
            .insert(replay_key, (reconstructed, outcome.clone()));
        if journals_the_block && !self.block_journaled.swap(true, Ordering::SeqCst) {
            let store = {
                let mut slot = self.late_admission.lock_recover();
                slot.take()
            };
            if let Some(store) = store {
                let admitted = enqueue_idle_turn_input(
                    store.as_ref(),
                    &SessionId::from("root"),
                    LATE_TAB_INPUT,
                )
                .await;
                *self.admitted_late.lock_recover() = Some(admitted);
            }
        }
        Ok(outcome)
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.native.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.native.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}
