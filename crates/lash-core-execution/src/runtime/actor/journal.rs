//! The journal-era scaffolding the deleted scoped controller carried, ported
//! as it was: a replayed language command's journal guard, the owner-step
//! token, the shift's journal frontier and the effect count.
//!
//! None of it is a substrate seam. L4 (FIG-5174) deletes the owner-step
//! token and the frontier with the run coordinator's journal. The command
//! guard goes with the commands' issue path: a cell already resumes from its
//! snapshot (L7, FIG-5177), and its operations become their tools' own
//! admitted executions with L4; a process resumes from its snapshot with
//! L7b (FIG-5198).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::ActorContext;
use crate::{RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor};

/// The invocation frontier and owner await, shared across every rescope.
#[derive(Default)]
pub(crate) struct DriveFrontier {
    journal: crate::trace::JournalFrontier,
    owner_step: std::sync::Mutex<Option<String>>,
}

/// Completion and future drop both release command admission.
struct OwnerAwaitedStep(Arc<DriveFrontier>);

impl Drop for OwnerAwaitedStep {
    fn drop(&mut self) {
        *self
            .0
            .owner_step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl ActorContext {
    /// This context serving one replayed language command: every journal
    /// write made through it asks `guard` first (FIG-3586).
    #[must_use]
    pub fn with_journal_guard(&self, guard: Arc<crate::CommandJournalGuard>) -> Self {
        let mut next = self.scope.with();
        next.journal_guard = Some(guard);
        Self {
            idle_poll: self.idle_poll,
            inner: Arc::clone(&self.inner),
            scope: Arc::new(next),
        }
    }

    /// The command guard this context serves under, when it serves one.
    #[cfg(test)]
    pub(crate) fn journal_guard(&self) -> Option<Arc<crate::CommandJournalGuard>> {
        self.scope.journal_guard.clone()
    }

    /// Asks this context's command guard, when it has one, to admit a
    /// journal write.
    ///
    /// # Errors
    ///
    /// The guard's or the owner step's refusal.
    pub fn admit_journal_write(&self) -> Result<(), RuntimeEffectControllerError> {
        self.admit_journal_write_at(None)
    }

    /// [`Self::admit_journal_write`] for a write at `key`.
    ///
    /// # Errors
    ///
    /// The guard's or the owner step's refusal.
    pub fn admit_journal_write_at(
        &self,
        key: Option<&str>,
    ) -> Result<(), RuntimeEffectControllerError> {
        if let Some(active) = self
            .scope
            .frontier
            .owner_step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::JournalWriteDuringOwnerStep,
                format!(
                    "cannot register {} while owner step {active} is in flight",
                    key.unwrap_or("an unkeyed command")
                ),
            ));
        }
        match &self.scope.journal_guard {
            Some(guard) => guard.admit(key),
            None => Ok(()),
        }
    }

    /// Await one owner step. While it is awaited, no concurrent actor may
    /// register another command. The token is released even when this
    /// future is dropped.
    ///
    /// # Errors
    ///
    /// `JournalWriteDuringOwnerStep` while another owner step is awaited,
    /// or the step's error.
    pub async fn await_owner_step<T, F>(
        &self,
        name: String,
        step: F,
    ) -> Result<T, RuntimeEffectControllerError>
    where
        F: std::future::Future<Output = Result<T, RuntimeEffectControllerError>>,
    {
        {
            let mut held = self
                .scope
                .frontier
                .owner_step
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(active) = held.as_ref() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::JournalWriteDuringOwnerStep,
                    format!("cannot await {name} while owner step {active} is in flight"),
                ));
            }
            *held = Some(name);
        }
        let _release = OwnerAwaitedStep(Arc::clone(&self.scope.frontier));
        step.await
    }

    /// Where this context's shift stands relative to its journal.
    #[must_use]
    pub fn frontier(&self) -> &crate::trace::JournalFrontier {
        &self.scope.frontier.journal
    }

    /// This context as part of `shift`'s shift: it shares its frontier.
    #[must_use]
    pub fn in_drive_of(&self, shift: &ActorContext) -> Self {
        let mut next = self.scope.with();
        next.frontier = Arc::clone(&shift.scope.frontier);
        Self {
            idle_poll: self.idle_poll,
            inner: Arc::clone(&self.inner),
            scope: Arc::new(next),
        }
    }

    /// How many effects this context and its clones have executed.
    #[must_use]
    pub fn effects_executed(&self) -> u64 {
        self.scope.ordinals.effects.load(Ordering::SeqCst)
    }

    /// Asks this context's command guard to admit `envelope`, and marks
    /// `local_executor` served only when the guard serves its command only
    /// from the journal (FIG-3587, FIG-3719).
    ///
    /// # Errors
    ///
    /// The guard's refusal.
    pub fn guard_local_executor<'executor>(
        &self,
        envelope: &RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'executor>,
    ) -> Result<RuntimeEffectLocalExecutor<'executor>, RuntimeEffectControllerError> {
        let mut local_executor = local_executor.issued_under(
            self.scope.frontier.journal.clone(),
            None,
            self.trace_scope().cloned(),
        );
        self.admit_journal_write_at(Some(envelope.invocation.effect_replay_key()))?;
        if let Some(guard) = &self.scope.journal_guard {
            // Only a dispatching effect is served only (FIG-3587, FIG-3725,
            // FIG-3779).
            let dispatches = match &envelope.command {
                crate::RuntimeEffectCommand::TraceBoundary { .. }
                | crate::RuntimeEffectCommand::LoadExecutionEnv { .. }
                | crate::RuntimeEffectCommand::PresentToolResult { .. } => false,
                crate::RuntimeEffectCommand::Process { command } => {
                    !matches!(command.as_ref(), crate::ProcessCommand::Await { .. })
                }
                _ => true,
            };
            let key = envelope.invocation.effect_replay_key();
            if let Some(range) = guard
                .served_only
                .as_ref()
                .filter(|range| dispatches && range.judges(key))
            {
                let refusal = range.refusal.clone();
                local_executor =
                    local_executor.serving_only_from_journal(refusal, Arc::clone(guard));
            }
        }
        Ok(local_executor)
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
