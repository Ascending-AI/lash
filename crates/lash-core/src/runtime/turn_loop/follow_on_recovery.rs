//! Recovery of the pending follow-on at the start of a drive (ADR 0101 §3,
//! FIG-3542).
//!
//! A committed agent-frame switch owes its follow-on turn on the session
//! head. When the drive that wrote it runs on, the logical run takes the
//! follow-on inline and nothing here is involved. When that drive died
//! between the switch commit and the follow-on's terminal commit, or the
//! follow-on failed before its commit, the next drive recovers it here,
//! before it admits anything: every admission but the follow-on's own is
//! blocked while it is owed anyway.
//!
//! The session drive's admission admits an owed follow-on as a root of its
//! own, named by the recovery count it records. The root records its
//! decision, and raises the count, in its `drive-follow-on` step
//! (FIG-4361), then drives the recorded answer here. Nothing below is
//! engine-specific.

use super::*;

impl LashRuntime {
    /// Drive a recovered follow-on as a logical run of its own, continuing
    /// its chain from the recorded position.
    ///
    /// It runs under the drain's own identity, or, for an anonymous drain, a
    /// drain identity named after the follow-on. The drive answers the
    /// follow-on and nothing else; the next drain admits what is queued.
    pub(in crate::runtime) async fn drive_recovered_follow_on(
        &mut self,
        recovery: crate::store::FollowOnRecovery,
        queued_opts: &QueuedTurnOptions<'_>,
        fence: DriveFence,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let (start, owed) = match recovery {
            crate::store::FollowOnRecovery::Run(owed) => {
                let (input, options) = crate::runtime::logical_turn::follow_on_input(
                    &owed,
                    crate::TurnContext::default(),
                );
                (LogicalTurnStart::Input(input, options), owed)
            }
            crate::store::FollowOnRecovery::Exhausted(owed) => {
                (LogicalTurnStart::ExhaustedFollowOn(owed.clone()), owed)
            }
        };
        let scope = queued_opts.source.identity().unwrap_or_else(|| {
            crate::ExecutionScope::queue_drain(
                self.state.session_id.clone(),
                format!("follow-on:{}", owed.follow_on_turn_id),
            )
        });
        let opts = queued_opts.bind(scope)?;
        let run = self
            .drive_logical_turn(
                start,
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
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
