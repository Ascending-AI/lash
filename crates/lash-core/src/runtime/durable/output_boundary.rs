//! A plain round's outcomes and emissions held for the next provider boundary.
//! Losing the activation loses these writes: a successor follows the recorded
//! Once/Repeatable start, and never uses these provisional answers.

use lash_core_execution::runtime::actor::round::DeferredOutcome;
use lash_durable::{ActorTx, CommitLabel, Release};

use super::{ActorContext, Effect, TurnDrive, TurnError, turn_cancel};

#[derive(Default)]
pub(super) struct OutputBoundary {
    outcome: Option<Box<DeferredOutcome>>,
    emissions: Vec<Effect>,
}

impl OutputBoundary {
    pub(super) fn held(&self) -> bool {
        self.outcome.is_some()
    }

    pub(super) fn set(&mut self, outcome: Option<Box<DeferredOutcome>>) {
        self.outcome = outcome;
    }

    pub(super) fn hold(&mut self, effect: Effect) {
        self.emissions.push(effect);
    }

    pub(super) fn append_to(&self, tx: &mut ActorTx) {
        if let Some(outcome) = &self.outcome {
            outcome.append_to(tx);
        }
    }

    pub(super) async fn committed(
        &mut self,
        cx: &ActorContext,
        drive: &mut dyn TurnDrive,
    ) -> Result<(), TurnError> {
        if let Some(outcome) = self.outcome.take() {
            outcome.committed();
        }
        for effect in std::mem::take(&mut self.emissions) {
            drive.local(cx, effect).await?;
        }
        Ok(())
    }

    /// An intervening output, callback or drain consumes a recorded boundary.
    /// A fresh accepted Immediate stop discards the provisional answer instead.
    pub(super) async fn flush(
        &mut self,
        cx: &ActorContext,
        drive: &mut dyn TurnDrive,
        release: Option<Release>,
    ) -> Result<bool, TurnError> {
        let mut tx = cx.begin().await?;
        if turn_cancel::immediate_in(&tx) || turn_cancel::close_requested_in(&tx) {
            return Ok(false);
        }
        self.append_to(&mut tx);
        match release {
            Some(release) => {
                cx.commit_draining(tx, CommitLabel::ROUND_OUTCOME, release)
                    .await?
            }
            None => cx.commit(tx, CommitLabel::ROUND_OUTCOME).await?,
        };
        self.committed(cx, drive).await?;
        Ok(true)
    }
}
