//! The command protocol both lashlang bridges follow for every command that
//! leaves the VM toward the effect host (FIG-3586): mint the ordinal, issue
//! its effects through a context whose journal writes ask the command's
//! guard, and close it.
//!
//! A refusal at any step stops the run the same way on both bridges: the
//! refusal becomes the execution's nested error — so a cell's turn parks and
//! a process segment fails its run instead of committing — and the run's own
//! cancellation scope is cancelled, so no further command leaves the run
//! whatever a guest handler does with the error it is handed. That is what
//! makes the refusal uncatchable: `try { await a() } catch {}; await b()`
//! never dispatches `b` after `a` diverged.

use std::sync::Arc;

use lash_core::{CommandJournalGuard, RuntimeEffectControllerError, RuntimeExecutionContext};
use lashlang::ExecutionHostError;

use crate::replay_run::{CommandShape, IssuedCommand, LashlangReplayRun};

/// One command in flight: its ordinal and key, the context its effects are
/// issued through, and the guard that context's journal writes ask.
pub struct CommandInFlight<'run> {
    pub command: IssuedCommand,
    pub ctx: RuntimeExecutionContext<'run>,
    guard: Arc<CommandJournalGuard>,
}

/// One bridge's view of its run for one step: the run, the execution it
/// issues through, the run's cancellation scope, and who the bridge is, for
/// the attribution a refusal carries.
pub struct ReplayCommands<'a, 'run> {
    pub run: &'a LashlangReplayRun,
    pub ctx: &'a RuntimeExecutionContext<'run>,
    pub cancellation: &'a crate::ExecutionCancellation,
    pub producer: String,
}

impl<'run> ReplayCommands<'_, 'run> {
    /// Mints the next command's issue ordinal. Every command that leaves the
    /// VM toward the effect host takes one here, before anything else can
    /// fail, so a command refused in the bridge still holds its place and
    /// every later command keeps its key.
    pub fn issue(&self) -> Result<IssuedCommand, ExecutionHostError> {
        self.run.issue().map_err(|error| self.stop(error))
    }

    /// Admits `command` to the host as a `shape` and hands back the context
    /// the command's effects are issued through, carrying the guard its
    /// journal writes ask.
    pub async fn enter(
        &self,
        command: IssuedCommand,
        shape: CommandShape,
    ) -> Result<CommandInFlight<'run>, ExecutionHostError> {
        self.enter_bound(command, shape, None).await
    }

    /// [`Self::enter`] for a command that calls a host tool binding which
    /// drifted since the pass that wrote the journal (FIG-3587): `drift` is
    /// the refusal naming it. Such a command is served only from the
    /// journal, and a fresh run's journal holds nothing, so it refuses
    /// before anything is dispatched.
    pub async fn enter_bound(
        &self,
        command: IssuedCommand,
        _shape: CommandShape,
        drift: Option<RuntimeEffectControllerError>,
    ) -> Result<CommandInFlight<'run>, ExecutionHostError> {
        if let Some(drift) = drift {
            return Err(self.stop(drift));
        }
        let guard = Arc::new(CommandJournalGuard::open());
        Ok(CommandInFlight {
            ctx: self.ctx.with_command_journal_guard(Arc::clone(&guard)),
            command,
            guard,
        })
    }

    /// Closes a command that reached the host. A replay mismatch any of its
    /// effects met stops the run here, as the run's own divergence.
    pub fn finish(&self, in_flight: &CommandInFlight<'_>) -> Result<(), ExecutionHostError> {
        if let Some(refusal) = in_flight.guard.tripped() {
            return Err(self.stop(refusal));
        }
        if let Some(mismatch) = self.ctx.nested_replay_mismatch() {
            return Err(self.stop(retype_replay_mismatch(
                mismatch,
                in_flight.command.key.as_str(),
                &self.attribution(),
            )));
        }
        self.run
            .finish(&in_flight.command, in_flight.guard.touched());
        Ok(())
    }

    /// Leaves a command open for the segment that resumes the run: the wait
    /// it issued was handed over, so the run stops on it and its successor
    /// issues it again under the same ordinal. A replay mismatch its effects
    /// met stops the run here, as [`Self::finish`] would.
    pub fn hand_over(&self, in_flight: &CommandInFlight<'_>) -> Result<(), ExecutionHostError> {
        if let Some(refusal) = in_flight.guard.tripped() {
            return Err(self.stop(refusal));
        }
        self.run
            .hand_over(&in_flight.command)
            .map_err(|divergence| self.stop(divergence.into_error(&self.attribution())))
    }

    /// Closes a command that never reached the host: it failed in the bridge.
    pub fn skipped(&self, command: &IssuedCommand) -> Result<(), ExecutionHostError> {
        self.run.finish(command, false);
        Ok(())
    }

    /// A journal error one of a command's effects returned directly. A replay
    /// mismatch stops the run; any other error is `fallback`'s to shape.
    pub fn journal_error(
        &self,
        in_flight: &CommandInFlight<'_>,
        error: RuntimeEffectControllerError,
        fallback: impl FnOnce(RuntimeEffectControllerError) -> ExecutionHostError,
    ) -> ExecutionHostError {
        if error.code.is_replay_mismatch() {
            return self.stop(retype_replay_mismatch(
                error,
                in_flight.command.key.as_str(),
                &self.attribution(),
            ));
        }
        fallback(error)
    }

    /// Who wrote the journal this run replays, and who is replaying it.
    pub fn attribution(&self) -> crate::SealAttribution {
        self.run.attribution(self.producer.clone())
    }

    /// Stops the run on a replay refusal.
    pub fn stop(&self, refusal: RuntimeEffectControllerError) -> ExecutionHostError {
        let message = refusal.message.clone();
        self.ctx.replace_nested_effect_error(refusal);
        self.cancellation.cancel();
        ExecutionHostError::new(message)
    }

    /// Aborts the run on a journal fault it cannot continue past: recorded as
    /// the execution's nested error, which aborts it like a crash.
    pub fn abort(&self, error: RuntimeEffectControllerError) -> ExecutionHostError {
        let message = error.message.clone();
        self.ctx.record_nested_runtime_effect_error(error);
        self.cancellation.cancel();
        ExecutionHostError::new(message)
    }

    /// Closes a run that journals no seal (a process body, whose terminal the
    /// registry records). Writes nothing.
    pub async fn close_unsealed(&self) {}

    /// Journals the run's seal as its last nested effect: the count of
    /// commands it issued and the digest of those it wrote, with `producer`
    /// served back for attribution. Nothing is written after a nested error:
    /// the run aborts and journals nothing more.
    pub async fn seal(&self, producer: serde_json::Value) {
        if self.ctx.has_nested_effect_error() {
            return;
        }
        let seal = self.run.seal();
        let facts = format!(
            "issued={}:dispatched={}",
            seal.issued_count,
            seal.dispatched_ordinals_digest.as_str()
        );
        if let Err(error) = self
            .ctx
            .journal_run_seal(seal.key.clone(), facts, producer)
            .await
        {
            if error.code.is_replay_mismatch() {
                self.ctx.replace_nested_effect_error(retype_replay_mismatch(
                    error,
                    &seal.key,
                    &self.attribution(),
                ));
            } else {
                self.ctx.record_nested_runtime_effect_error(error);
            }
        }
    }
}

/// A replay mismatch a command met at a recorded entry, re-typed as the
/// run's own divergence so the run stops and the operator reads the run's
/// terms — the key, who wrote the journal — beside the substrate's divergent
/// paths. Any other controller error passes through unchanged.
pub fn retype_replay_mismatch(
    error: RuntimeEffectControllerError,
    key: &str,
    attribution: &crate::SealAttribution,
) -> RuntimeEffectControllerError {
    if !error.code.is_replay_mismatch()
        || error.code == lash_core::RuntimeErrorCode::LashlangCellReplayDivergence
        || error.code == lash_core::RuntimeErrorCode::LashlangCellBindingDrift
    {
        return error;
    }
    let mut retyped = RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::LashlangCellReplayDivergence,
        format!(
            "lashlang run diverged from its journal at `{key}` ({}): {}. Nothing was \
             dispatched: redeploy the build that wrote the journal, cancel, or fork from \
             before this command",
            attribution.describe(),
            error.message
        ),
    );
    if let Some(summary) = error.summary {
        retyped = retyped.with_summary(*summary);
    }
    retyped
}
