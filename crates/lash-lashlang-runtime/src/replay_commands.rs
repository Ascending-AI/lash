//! The command protocol both lashlang bridges follow for every command that
//! leaves the VM toward the effect host (FIG-3586): mint the ordinal, admit
//! the command against the recorded frontier, issue its effects through a
//! context whose journal writes ask the command's guard, and close it.
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

use crate::replay_run::{CommandAdmission, CommandShape, IssuedCommand, LashlangReplayRun};

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

    /// Admits `command` to the host as a `shape`: reads the recorded frontier
    /// on the run's first command, refuses a command the journal holds as
    /// another shape, and hands back the context the command's effects are
    /// issued through, carrying the guard its journal writes ask.
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
    /// journal: one the journal does not hold refuses before anything is
    /// dispatched, and every dispatching effect of one it holds carries
    /// `drift` to its engine, which serves the recorded outcome and refuses —
    /// running and recording nothing — an effect it would run live
    /// (FIG-3719). The engine answers, not the frontier read, so this holds
    /// on a positional journal as on a keyed one.
    pub async fn enter_bound(
        &self,
        command: IssuedCommand,
        shape: CommandShape,
        drift: Option<RuntimeEffectControllerError>,
    ) -> Result<CommandInFlight<'run>, ExecutionHostError> {
        if let Err(error) = self.run.ensure_frontier(self.ctx).await {
            return Err(self.abort(error));
        }
        if let Some(drift) = drift {
            let range = self.run.namespace().range();
            // A drifted binding's command replays only what the journal
            // settled; a command the journal does not hold as issued is the
            // run's divergence first, reported as such (FIG-3587).
            let guard = match self.run.enter(&command, shape) {
                Ok(CommandAdmission::Replay) => CommandJournalGuard::open(),
                Ok(CommandAdmission::ReplayRecordedKeys { keys, divergence }) => {
                    CommandJournalGuard::fenced(lash_core::RecordedKeyFence {
                        keys,
                        lower: range.lower,
                        upper: range.upper,
                        refusal: divergence.into_error(&self.attribution()),
                    })
                }
                Ok(CommandAdmission::RefuseWrites(divergence)) | Err(divergence) => {
                    return Err(self.stop(divergence.into_error(&self.attribution())));
                }
                // A positional host answers as the replay reaches each
                // effect; a keyed one holds nothing here, so the command
                // would reach the drifted tool live.
                Ok(CommandAdmission::Live) if self.run.is_positional() => {
                    CommandJournalGuard::open()
                }
                Ok(CommandAdmission::Live) => return Err(self.stop(drift)),
            };
            let range = self.run.namespace().range();
            let guard = Arc::new(guard.served_only(lash_core::ServedOnlyRange {
                lower: range.lower,
                upper: range.upper,
                refusal: drift,
            }));
            return Ok(CommandInFlight {
                ctx: self.ctx.with_command_journal_guard(Arc::clone(&guard)),
                command,
                guard,
            });
        }
        let guard = match self.run.enter(&command, shape) {
            Ok(CommandAdmission::Replay | CommandAdmission::Live) => CommandJournalGuard::open(),
            Ok(CommandAdmission::ReplayRecordedKeys { keys, divergence }) => {
                let range = self.run.namespace().range();
                CommandJournalGuard::fenced(lash_core::RecordedKeyFence {
                    keys,
                    lower: range.lower,
                    upper: range.upper,
                    refusal: divergence.into_error(&self.attribution()),
                })
            }
            // Only the run's own namespace is refused: a call the recorded run
            // made with nothing under it (an orchestrating body that issued no
            // nested effect) still presents its result, and the host serves
            // that from its own record (FIG-3680).
            Ok(CommandAdmission::RefuseWrites(divergence)) => {
                let range = self.run.namespace().range();
                CommandJournalGuard::refusing(lash_core::RefusedWriteRange {
                    lower: range.lower,
                    upper: range.upper,
                    refusal: divergence.into_error(&self.attribution()),
                })
            }
            Err(divergence) => {
                return Err(self.stop(divergence.into_error(&self.attribution())));
            }
        };
        let guard = Arc::new(guard);
        Ok(CommandInFlight {
            ctx: self.ctx.with_command_journal_guard(Arc::clone(&guard)),
            command,
            guard,
        })
    }

    /// Closes a command that reached the host. A replay mismatch any of its
    /// effects met stops the run here, as the run's own divergence; so does
    /// a command the journal recorded that wrote nothing this time.
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
            .finish(&in_flight.command, in_flight.guard.touched())
            .map_err(|divergence| self.stop(divergence.into_error(&self.attribution())))
    }

    /// Closes a command that never reached the host: it failed in the bridge.
    /// A command the journal recorded as dispatched refuses here.
    pub fn skipped(&self, command: &IssuedCommand) -> Result<(), ExecutionHostError> {
        self.run
            .finish(command, false)
            .map_err(|divergence| self.stop(divergence.into_error(&self.attribution())))
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
    /// registry records): refuses when the journal holds a command at or
    /// beyond the ones this run issued — the run ended earlier than the one
    /// that wrote the journal. Writes nothing.
    pub async fn close_unsealed(&self) {
        if self.ctx.has_nested_effect_error() {
            return;
        }
        if let Err(error) = self.run.ensure_frontier(self.ctx).await {
            self.ctx.record_nested_runtime_effect_error(error);
            return;
        }
        if let Err(divergence) = self.run.seal() {
            self.ctx
                .replace_nested_effect_error(divergence.into_error(&self.attribution()));
        }
    }

    /// Journals the run's seal as its last nested effect: the count of
    /// commands it issued and the digest of those it wrote, with `producer`
    /// served back for attribution. Nothing is written after a nested error:
    /// the run aborts and journals nothing more.
    pub async fn seal(&self, producer: serde_json::Value) {
        if self.ctx.has_nested_effect_error() {
            return;
        }
        if let Err(error) = self.run.ensure_frontier(self.ctx).await {
            self.ctx.record_nested_runtime_effect_error(error);
            return;
        }
        let seal = match self.run.seal() {
            Ok(seal) => seal,
            Err(divergence) => {
                self.ctx
                    .replace_nested_effect_error(divergence.into_error(&self.attribution()));
                return;
            }
        };
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
