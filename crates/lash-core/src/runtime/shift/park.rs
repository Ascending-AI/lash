//! A logical run's park (FIG-3600 S7, D2 §1.3): the record an aborting run
//! writes when its abort is a refusal no redrive of the same build can get
//! past.
//!
//! A park names the **logical run**, never a physical turn: a frame switch's
//! follow-on, an S4 follow-on and a redrive all run under the run, and the
//! operator verbs (redrive, cancel, fork) act on it. Parked is durable and
//! non-terminal: the run writes no terminal evidence, keeps its admission and
//! keeps `Turn(run)` open, and admission answers `Parked` while the row
//! exists.
//!
//! A run that already has terminal evidence never parks (P2). The store
//! refuses the write with `StoreError::RunAlreadyTerminal` inside its own
//! transaction, which fences a zombie execution that resumed after an
//! operator's cancel or fork: its abort leaves nothing behind.

use crate::runtime::LashRuntime;
use crate::{RuntimeError, StoreError, TurnId};

impl LashRuntime {
    /// The logical run a park of the running turn names, and so the run
    /// whose park the turn's commit clears (D2 §1.3): the admitted run's,
    /// when a shift runs one — the run a follow-on recovery ends, not the
    /// recovery's admission name — else `scope_run`, the run the turn's
    /// controller runs under, else the physical `turn` itself.
    pub(in crate::runtime) fn park_run(&self, scope_run: Option<TurnId>, turn: &TurnId) -> TurnId {
        self.shift_run
            .as_ref()
            .map(|run| run.run().clone())
            .or(scope_run)
            .unwrap_or_else(|| turn.clone())
    }

    /// Record `run`'s park when its abort is a refusal that parks it
    /// (FIG-3586, FIG-3600).
    ///
    /// A parked run keeps every admission it holds, exactly as any live-fault
    /// abort does, so each redrive under the same build refuses again with
    /// nothing dispatched; the park is the typed, queryable record of why it
    /// stopped, which `drain_status` counts. Best effort, like every
    /// abort-path repair: the run is already aborting and this must not
    /// replace its error. A park the store cannot write leaves the run
    /// exactly as the abort left it — its held admission still keeps the
    /// deployment from reporting drained.
    pub(in crate::runtime) async fn record_turn_park_after_abort(
        &self,
        err: &RuntimeError,
        run: &TurnId,
        journal_generation: Option<&crate::engine::BuildGeneration>,
    ) {
        let Some(reason) = crate::store::ParkReason::of_error(err) else {
            return;
        };
        // Under a shift the park names the admitted run's logical run,
        // whatever name the aborting site knew it by.
        let run = &self.park_run(Some(run.clone()), run);
        // The runtime's own store: a run refused at its admission parks
        // before its plugin transition has built any session (FIG-4857).
        let Some(store) = self.services.store.clone() else {
            return;
        };
        // FIG-3795 S9: the park records the drain generation of the build the
        // parked journal belongs to — the caller's where it holds one (a
        // resumed run's recorded admission), else the running run's admitted
        // generation. No engine binds a build generation of its own any more
        // (I0, FIG-5194), so a park with neither is not recorded.
        let recorded = journal_generation
            .or_else(|| self.shift_run.as_ref().map(|run| run.journal_generation()));
        let Some(generation) = recorded.cloned() else {
            tracing::warn!(
                session_id = %self.state.session_id,
                turn_id = %run,
                error = %crate::engine::GenerationUnbound,
                "turn park not recorded"
            );
            return;
        };
        let mut write = crate::store::TurnParkWrite::refusal(
            self.state.session_id.clone(),
            run.clone(),
            reason,
            self.host.core.clock.timestamp_ms(),
        );
        write.build_generation = Some(generation);
        let reason_code = write.reason.code().as_str();
        let effect_kind = write.reason.effect_kind();
        match lash_core_execution::runtime::record_run_park(
            store.store().as_ref(),
            &write,
            self.host.core.tracing.metrics(),
        )
        .await
        {
            Ok(park) => {
                tracing::warn!(
                    session_id = %self.state.session_id,
                    run = %run,
                    code = %err.code,
                    reason_code,
                    effect_kind,
                    park_id = %park.park_id,
                    attempts = park.attempts,
                    event = "turn.parked",
                    "run parked on a replay refusal; redrive it under the build that wrote its \
                     journal, cancel it, or fork from before it"
                );
            }
            // P2: an operator already ended the run. This execution is a
            // zombie of it, and its abort leaves nothing behind.
            Err(StoreError::RunAlreadyTerminal { by, .. }) => tracing::info!(
                session_id = %self.state.session_id,
                run = %run,
                code = %err.code,
                ended_by = ?by,
                event = "turn.park_refused_terminal",
                "a run with terminal evidence refused its park; the aborting execution is \
                 released"
            ),
            Err(StoreError::RunInputWithdrawn { .. }) => tracing::info!(
                session_id = %self.state.session_id,
                run = %run,
                event = "turn.park_refused_withdrawn",
                "a run whose input was withdrawn refused its stale park"
            ),
            Err(error) => tracing::warn!(
                session_id = %self.state.session_id,
                run = %run,
                error = %error,
                event = "turn.park_record_failed",
                "failed to record the run's park; its held admission still keeps the deployment \
                 from draining"
            ),
        }
    }
}

pub use lash_core_execution::runtime::{StoreParkRecovery, run_park_recorded};
