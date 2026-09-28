//! Recovery of the pending follow-on at the start of a drive (ADR 0101 §3,
//! FIG-3542).
//!
//! A committed agent-frame switch owes its follow-on turn on the session
//! head. When the drive that wrote it runs on, the logical run takes the
//! follow-on inline and nothing here is involved. When that drive died
//! between the switch commit and the follow-on's terminal commit, the next
//! drive recovers it here, before it claims anything: every claim but the
//! follow-on's own is blocked while it is owed anyway.
//!
//! The session drive's admission admits an owed follow-on as a root of its
//! own, named by the recovery count it records
//! ([`recover_admitted_follow_on`]), so every engine's drive recovers it
//! before any other work. Nothing below is engine-specific.
//!
//! [`recover_admitted_follow_on`]: LashRuntime::recover_admitted_follow_on

use super::*;

impl LashRuntime {
    /// Raise the owed follow-on's recovery count in a fenced head write
    /// before its first effect, or, once the raised count would pass the
    /// host's bound, answer [`FollowOnRecovery::Exhausted`] without writing:
    /// the follow-on then commits as its failed terminal.
    ///
    /// `recorded` is the count a drive admission recorded for this recovery.
    /// The decision is taken on it, and a count already past it was raised by
    /// an earlier execution of the same recovery, which this one continues
    /// without raising again. Without it the live count decides.
    ///
    /// [`FollowOnRecovery::Exhausted`]: crate::store::FollowOnRecovery::Exhausted
    async fn recover_pending_follow_on(
        &mut self,
        store: &dyn crate::store::RuntimePersistence,
        fence: &crate::ClaimAuthority,
        recorded: Option<u32>,
    ) -> Result<crate::store::FollowOnRecovery, RuntimeError> {
        let owed = self
            .state
            .pending_follow_on
            .as_deref()
            .cloned()
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::FollowOnPending,
                    "no pending follow-on to recover",
                )
            })?;
        let basis = crate::store::PendingFollowOn {
            attempts: recorded.unwrap_or(owed.attempts),
            ..owed.clone()
        };
        let recovery = basis
            .recovery(
                self.host
                    .core
                    .durability
                    .queued_work_batching
                    .max_follow_on_recoveries(),
            )
            .map_err(super::runtime_error_from_store_commit)?;
        if owed.attempts > basis.attempts {
            // This recovery's raise landed on an earlier execution of it.
            return Ok(match recovery {
                crate::store::FollowOnRecovery::Run(_) => crate::store::FollowOnRecovery::Run(owed),
                crate::store::FollowOnRecovery::Exhausted(_) => {
                    crate::store::FollowOnRecovery::Exhausted(owed)
                }
            });
        }
        if let crate::store::FollowOnRecovery::Run(_) = &recovery {
            let raised = store
                .raise_pending_follow_on_attempts(fence, &owed.follow_on_turn_id)
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            self.state.pending_follow_on = Some(Box::new(raised.clone()));
            return Ok(crate::store::FollowOnRecovery::Run(raised));
        }
        Ok(recovery)
    }

    /// Recover `follow_on` as the root drive admission admitted for it
    /// (ADR 0101 §3, FIG-3542), from the recovery count `recorded` with the
    /// admission.
    ///
    /// Admission decided that the head owes it. A follow-on no longer owed
    /// when the root takes the lane was answered by another driver since:
    /// the root cedes and runs nothing.
    pub(in crate::runtime) async fn recover_admitted_follow_on(
        &mut self,
        opts: QueuedTurnOptions<'_>,
        follow_on: &TurnId,
        recorded: u32,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let Some(lease) = self.claim_drive_authority_for_queued_work(&opts).await? else {
            return Ok(QueuedTurnDrain::Empty(
                EmptyQueuedDrainReason::ExecutionLaneBusy,
            ));
        };
        let prepared = async {
            let store = self
                .session
                .as_ref()
                .and_then(|session| session.history_store())
                .ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorCode::QueuedWork,
                        "a follow-on recovery requires persistence",
                    )
                })?;
            self.refresh_resident_head_under_lease(Some(&lease)).await?;
            if !self
                .state
                .pending_follow_on
                .as_deref()
                .is_some_and(|owed| owed.is_turn(follow_on))
            {
                return Ok(None);
            }
            self.recover_pending_follow_on(store.as_ref(), &lease.fence(), Some(recorded))
                .await
                .map(Some)
        }
        .await;
        let recovery = match prepared {
            Ok(Some(recovery)) => recovery,
            Ok(None) => {
                let _ = lease.release_if_live().await;
                return Ok(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::ClaimRefused(
                        crate::QueuedWorkClaimRefusal::ClaimRaceLost,
                    ),
                ));
            }
            Err(error) => {
                let _ = lease.release_if_live().await;
                return Err(error);
            }
        };
        Box::pin(self.drive_recovered_follow_on(recovery, &opts, lease)).await
    }

    /// Drive a recovered follow-on as a logical run of its own, continuing
    /// its chain from the recorded position.
    ///
    /// It runs under the drain's own identity, or, for an anonymous drain, a
    /// drain identity named after the follow-on. The drive answers the
    /// follow-on and nothing else; the next drain claims what is queued.
    async fn drive_recovered_follow_on(
        &mut self,
        recovery: crate::store::FollowOnRecovery,
        queued_opts: &QueuedTurnOptions<'_>,
        lease: DriveClaimGuard,
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
        let opts = match queued_opts.bind(scope) {
            Ok(opts) => opts,
            Err(error) => {
                let _ = lease.release_if_live().await;
                return Err(error);
            }
        };
        let mut lease = Some(lease);
        let result = self
            .drive_logical_turn(
                start,
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
                LogicalTurnClaims::new(Vec::new(), Vec::new()),
                &mut lease,
                TurnStopwatch::start(self.host.core.clock.as_ref()),
            )
            .await;
        let run = self.settle_drive_authority(lease.as_ref(), result).await?;
        Ok(match run.into_final_turn() {
            Some(turn) => QueuedTurnDrain::Ran(turn),
            None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::ClaimRefused(
                crate::QueuedWorkClaimRefusal::FollowOnPending,
            )),
        })
    }
}
