//! Recovery of the pending follow-on at the start of a shift (ADR 0101 §3,
//! FIG-3542).
//!
//! A committed agent-frame switch owes its follow-on turn on the session
//! head. When the shift that wrote it runs on, the logical run takes the
//! follow-on inline and nothing here is involved. When that shift died
//! between the switch commit and the follow-on's terminal commit, or the
//! follow-on failed before its commit, the next shift recovers it here,
//! before it admits anything: every admission but the follow-on's own is
//! blocked while it is owed anyway.
//!
//! The session shift's admission admits an owed follow-on as a run of its
//! own, named by the recovery count it records. The run records its
//! decision, and raises the count, in its `shift-follow-on` step
//! (FIG-4361), then executes the recorded answer here. Nothing below is
//! engine-specific.

use super::*;

impl LashRuntime {
    /// Execute a recovered follow-on, continuing its logical run from the
    /// recorded position.
    ///
    /// It runs on `controller`, scoped to the turn of the logical run that
    /// owed the follow-on. The shift answers the follow-on and nothing else;
    /// the next admission admits what is queued.
    pub(in crate::runtime) async fn execute_recovered_follow_on(
        &mut self,
        recovery: crate::store::FollowOnRecovery,
        controller: crate::ActorContext,
        sinks: &crate::runtime::shift::ShiftSinks<'_>,
        fence: ShiftFence,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let start = match recovery {
            crate::store::FollowOnRecovery::Run(owed) => LogicalTurnStart::Input(
                crate::runtime::logical_turn::follow_on_input(&owed, crate::TurnContext::default()),
            ),
            crate::store::FollowOnRecovery::Exhausted(owed) => {
                LogicalTurnStart::ExhaustedFollowOn(Box::new(owed))
            }
        };
        let run = self
            .execute_logical_turn(
                start,
                sinks.events,
                sinks.turn_events,
                controller,
                sinks.local_stop.clone(),
                LogicalTurnAdmissions::new(Vec::new(), Vec::new()),
                Some(&fence),
                TurnStopwatch::start(self.host.core.clock.as_ref()),
            )
            .await?;
        Ok(match run.into_final_turn() {
            Some(turn) => QueuedTurnDrain::Ran(turn),
            None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::AdmissionRefused(
                crate::AdmissionRefusal::FollowOnPending,
            )),
        })
    }
}
