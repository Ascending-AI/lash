//! This engine's race arm against a turn's cancellation gate (FIG-3672 P9).

use tokio_util::sync::CancellationToken;

use super::{
    AwaitEventBackend, AwaitEventKey, AwaitEventWaitIdentity, EffectReplayRowStore, ExecutionScope,
    Resolution, RuntimeEffectControllerError, StoreEffectReplayDriver,
};

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// This engine's race arm against a turn's cancellation gate (FIG-3672
    /// P9): resolves once the gate pair of `scope`'s turn asks the turn to
    /// stop now, and never otherwise. A wait this engine records races it
    /// inside its own execution, so a replay serves what the race recorded.
    /// `None` — a wait that observes no turn — never resolves.
    pub(crate) async fn turn_stop(
        &self,
        scope: Option<&ExecutionScope>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Some(scope) = scope else {
            return std::future::pending().await;
        };
        let pair = crate::runtime::turn_control::TurnCancelGatePair::new(
            self.await_event_key(scope, AwaitEventWaitIdentity::TurnCancelGate)
                .await?,
            self.await_event_key(scope, AwaitEventWaitIdentity::TurnCancelEscalation)
                .await?,
        );
        match pair
            .await_stop(|key| async move {
                self.await_await_event(&key, CancellationToken::new(), None)
                    .await
            })
            .await?
        {
            Some(_) => Ok(()),
            None => std::future::pending().await,
        }
    }

    /// A durable await this engine records, raced against its turn's
    /// cancellation gate inside the recorded execution: a stop resolves the
    /// wait `Cancelled`, and that outcome is what the journal records.
    pub(super) async fn await_event_racing_turn(
        &self,
        key: &AwaitEventKey,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        races_turn_gate: bool,
    ) -> Result<crate::RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let crate::runtime::effect::executor::RuntimeAwaitEventOptions {
            cancellation,
            deadline,
            clock,
            observe_turn_cancel,
            turn_cancel_scope,
        } = local_executor.into_await_event_options()?;
        let turn_scope = turn_cancel_scope.filter(|_| observe_turn_cancel && races_turn_gate);
        let wait = self.await_events.await_resolution_with_clock(
            key,
            cancellation,
            deadline,
            clock.as_ref(),
        );
        let resolution = tokio::select! {
            biased;
            stop = self.turn_stop(turn_scope.as_ref()) => {
                stop?;
                Resolution::Cancelled
            }
            resolution = wait => resolution.map_err(RuntimeEffectControllerError::from)?,
        };
        Ok(crate::RuntimeEffectOutcome::AwaitEvent { resolution })
    }

    /// A process command this engine records, run with the turn's cancellation
    /// delivered by the engine: when the command observes a turn (a process
    /// await), the engine races the turn's gate beside it and fires the
    /// command's cooperative stop on a win, so the command's own recorded
    /// outcome says what the stop did (FIG-3672 P9).
    pub(super) async fn process_racing_turn(
        &self,
        mut process: crate::runtime::effect::executor::ProcessLocalExecution,
        command: crate::ProcessCommand,
        races_turn_gate: bool,
    ) -> Result<crate::ProcessEffectOutcome, RuntimeEffectControllerError> {
        let scope = process
            .turn_cancellation
            .as_ref()
            .map(|turn| turn.scope.clone())
            .filter(|_| races_turn_gate);
        let Some(scope) = scope else {
            return process.execute(command).await;
        };
        let stop = CancellationToken::new();
        if let Some(turn) = process.turn_cancellation.as_mut() {
            turn.cancellation = stop.clone();
        }
        let body = process.execute(command);
        tokio::pin!(body);
        tokio::select! {
            biased;
            outcome = &mut body => outcome,
            observed = self.turn_stop(Some(&scope)) => {
                observed?;
                stop.cancel();
                body.await
            }
        }
    }
}
