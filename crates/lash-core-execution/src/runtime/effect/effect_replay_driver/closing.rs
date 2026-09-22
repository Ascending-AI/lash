//! Durable closing and the four-step finalization (ADR 0099 §7, FIG-3410).
//!
//! Closing a durable effect group is a journal fact, written to the group
//! row's `lifecycle` column **before** any admission stops or any cancel
//! decision is issued — §7's ordering, and the reason `close` is safe to
//! leave behind a process that dies in its window: the recorded row is the
//! authority a redriven turn resumes from.
//!
//! Finalization is four recorded steps:
//!
//! 1. **Obligations.** Every accepted child is ranked. This host's own
//!    still-running children are awaited through the group's task-finished
//!    notify — unbounded for a `RunToCompletion` child, which is a protected
//!    obligation this host owes, and bounded by the drain budget measured
//!    from each cancel-decided child's decision instant, because that rank
//!    is already seated and waiting on a body that ignores its token is
//!    optional. A drain pass then discharges committed-but-undrained children
//!    and drives children no process is running. Children this process cannot
//!    finish — live lease elsewhere, no executor — leave the step unrecorded
//!    and the run reports [`GroupFinalizationReport::Pending`].
//! 2. **Outcome and accounting.** The opener's step, run through
//!    [`OpenerFinalizationSteps`] and recorded only after it returns — defined
//!    as incorporating every settled rank through
//!    `RuntimeExecutionContext::incorporate_tool_settlement` under
//!    `SettlementSource::GroupRank`, which the carried `IncorporationLedger`
//!    makes idempotent across a crash-and-resume.
//! 3. **Parent end.** The same, for the opener's end record.
//! 4. **Retirement.** The lifecycle CASes to `settled` and the process-local
//!    entry is reaped only through the shared closed-and-complete guard — a
//!    reopen that renewed interest in between keeps the entry it is serving.
//!
//! The cursor in the column is what makes the sequence resumable: a step is
//! run only while the cursor does not record it, so a crash between a step
//! and its recording re-runs the step — which is why every step is
//! idempotent — and a step already recorded is never re-run.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use lash_sansio::sync::MutexExt;

use super::groups::group_shape_error;
use super::{
    EffectGroupLifecycle, EffectGroupLifecyclePhase, EffectGroupRecord, EffectReplayRowStore,
    FinalizationStep, StoreEffectReplayDriver,
};
use crate::ExecutionScope;
use crate::runtime::effect::RuntimeEffectControllerError;
use crate::runtime::effect::await_event_coordinator::AwaitEventBackend;
use crate::runtime::effect::group::LoserPolicy;
use crate::runtime::effect::group_closing::{
    GroupFinalizationReport, OpenerFinalizationSteps, StoreEffectGroupClosing,
};

/// The [`StoreEffectGroupClosing`] handed out by both SQL effect hosts — the
/// same relationship [`DurableGroupDrain`](super::drain::DurableGroupDrain)
/// has to [`StoreEffectGroupDrain`]: one wrapper over the shared driver, so
/// the lifecycle a host's `close` writes and the cursor this object advances
/// are the same row under the same owner identity.
pub struct DurableGroupClosing<P, A> {
    driver: Arc<StoreEffectReplayDriver<P, A>>,
}

#[async_trait]
impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static> StoreEffectGroupClosing
    for DurableGroupClosing<P, A>
{
    async fn read_group_lifecycle(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupLifecycle>, RuntimeEffectControllerError> {
        Ok(self
            .driver
            .row_store
            .read_group(group_key)
            .await?
            .map(|record| record.lifecycle))
    }

    async fn finalize_group(
        &self,
        group_key: &str,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<GroupFinalizationReport, RuntimeEffectControllerError> {
        self.driver.finalize_group_record(group_key, steps).await
    }

    async fn resume_closing_groups(
        &self,
        scope: &ExecutionScope,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<Vec<GroupFinalizationReport>, RuntimeEffectControllerError> {
        let scope_id = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?
            .key()
            .to_string();
        let records = self.driver.row_store.read_closing_groups(&scope_id).await?;
        let mut reports = Vec::with_capacity(records.len());
        for record in records {
            reports.push(
                self.driver
                    .finalize_group_record(&record.group_key, steps)
                    .await?,
            );
        }
        Ok(reports)
    }

    async fn scope_is_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeEffectControllerError> {
        self.driver.row_store.scope_is_quiescent(scope).await
    }
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static>
    StoreEffectReplayDriver<P, A>
{
    /// The closing seam over this driver's own journal and resolver.
    pub fn into_group_closing(self: Arc<Self>) -> Arc<dyn StoreEffectGroupClosing> {
        Arc::new(DurableGroupClosing { driver: self })
    }
}

/// Whether step 1's obligation pass emptied the group's unsettled set.
enum ObligationDrain {
    /// `read_unsettled_group_children` is empty; the step records.
    Complete,
    /// Children this process cannot finish remain — the group stays
    /// `closing` and a later resume retries.
    Pending(usize),
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static>
    StoreEffectReplayDriver<P, A>
{
    /// Run `group_key` through whichever of the four finalization steps its
    /// recorded cursor has not reached.
    ///
    /// The loop re-reads the durable lifecycle after every CAS, so two
    /// finalizers racing one group converge on the same cursor rather than
    /// each re-running a step the other already recorded: a CAS that answers
    /// a further-along `closing` is adopted, and one that answers `settled`
    /// ends the run.
    pub(super) async fn finalize_group_record(
        self: &Arc<Self>,
        group_key: &str,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<GroupFinalizationReport, RuntimeEffectControllerError> {
        let record = self.row_store.read_group(group_key).await?.ok_or_else(|| {
            group_shape_error(format!(
                "durable effect group {group_key} is not journaled; \
                     finalization needs the row the open recorded"
            ))
        })?;
        let mut lifecycle = record.lifecycle;
        loop {
            let EffectGroupLifecycle::Closing {
                disposition,
                finalized,
            } = lifecycle
            else {
                return match lifecycle {
                    EffectGroupLifecycle::Settled { .. } => Ok(GroupFinalizationReport::Settled {
                        group_key: group_key.to_string(),
                    }),
                    EffectGroupLifecycle::Live => Err(group_shape_error(format!(
                        "durable effect group {group_key} is live; only a group \
                         whose close is recorded can be finalized"
                    ))),
                    EffectGroupLifecycle::Closing { .. } => {
                        unreachable!("the let-else binds every Closing")
                    }
                };
            };
            match finalized.completed() {
                0 => {
                    match self.drain_group_obligations(&record, disposition).await? {
                        ObligationDrain::Complete => {}
                        ObligationDrain::Pending(unsettled) => {
                            return Ok(GroupFinalizationReport::Pending {
                                group_key: group_key.to_string(),
                                unsettled,
                            });
                        }
                    }
                    lifecycle = self
                        .advance_finalization(group_key, disposition, finalized)
                        .await?;
                }
                1 => {
                    steps.commit_outcome_and_accounting(group_key).await?;
                    lifecycle = self
                        .advance_finalization(group_key, disposition, finalized)
                        .await?;
                }
                2 => {
                    steps.record_parent_end(group_key).await?;
                    lifecycle = self
                        .advance_finalization(group_key, disposition, finalized)
                        .await?;
                }
                _ => {
                    lifecycle = self
                        .row_store
                        .transition_group_lifecycle(
                            group_key,
                            &[EffectGroupLifecyclePhase::Closing],
                            &EffectGroupLifecycle::settled(disposition),
                        )
                        .await?;
                    if matches!(lifecycle, EffectGroupLifecycle::Settled { .. }) {
                        // Retire the process-local entry only through the same
                        // re-judged guard a settlement applies: a reopen that
                        // cleared `closed` since the settle was recorded is
                        // serving a live caller and must keep its entry.
                        if let Some(state) = self.groups.get(group_key) {
                            self.reap_if_complete(group_key, &state).await;
                        }
                        return Ok(GroupFinalizationReport::Settled {
                            group_key: group_key.to_string(),
                        });
                    }
                }
            }
        }
    }

    /// Record step `completed`'s successor on the group row and answer the
    /// lifecycle now durable — this finalizer's own write on a hit, or the
    /// racing finalizer's further-along cursor on a miss.
    async fn advance_finalization(
        &self,
        group_key: &str,
        disposition: LoserPolicy,
        completed: FinalizationStep,
    ) -> Result<EffectGroupLifecycle, RuntimeEffectControllerError> {
        let next = completed.next().ok_or_else(|| {
            group_shape_error(format!(
                "durable effect group {group_key} records a finalization cursor \
                 past the last step; settling is step 4's own transition"
            ))
        })?;
        self.row_store
            .transition_group_lifecycle(
                group_key,
                &[EffectGroupLifecyclePhase::Closing],
                &EffectGroupLifecycle::Closing {
                    disposition,
                    finalized: next,
                },
            )
            .await
    }

    /// §7 step 1: every accepted child ranked.
    ///
    /// First the drain pass — over the *effective* disposition, which is what
    /// the recorded close committed to rather than what the group declared —
    /// over every unsettled child this process is not itself running. Then
    /// the local wait for this host's own tasks. Then the confirming re-read
    /// that decides `Complete` from `Pending`.
    async fn drain_group_obligations(
        self: &Arc<Self>,
        record: &EffectGroupRecord,
        disposition: LoserPolicy,
    ) -> Result<ObligationDrain, RuntimeEffectControllerError> {
        let group_key = record.group_key.as_str();
        // The pass runs under the effective disposition: `drain_group_child`
        // branches on `loser_disposition` to decide a cancel, so the closing
        // record's committed answer is the one that applies.
        let effective_record = EffectGroupRecord {
            loser_disposition: disposition,
            ..record.clone()
        };
        let running: HashSet<String> = self
            .groups
            .get(group_key)
            .map(|state| state.state.lock_recover().running.clone())
            .unwrap_or_default();
        let queued = self
            .row_store
            .read_unsettled_group_children(group_key)
            .await?;
        let now = self.clock.timestamp_ms();
        for child in queued {
            // A child this process dispatched and has not finished finishes
            // under its own task — the pass's `RunningHere` refusal, applied
            // before the read, so the drain never steals from itself.
            if running.contains(&child.replay_key) {
                continue;
            }
            self.drain_group_child(&effective_record, &child, now, &CancellationToken::new())
                .await?;
        }
        self.await_local_obligations(group_key).await;
        let remaining = self
            .row_store
            .read_unsettled_group_children(group_key)
            .await?;
        Ok(if remaining.is_empty() {
            ObligationDrain::Complete
        } else {
            ObligationDrain::Pending(remaining.len())
        })
    }

    /// Wait for this host's still-running children to return — the F1/F2
    /// loop: a cancel-decided child is awaited only up to its drain budget
    /// measured from the decision instant, and a `RunToCompletion` child is a
    /// protected obligation this host owes, awaited without bound.
    ///
    /// Not a fixed sleep: the loop blocks on the group's task-finished notify
    /// with the recomputed remaining-budget deadline as the other select arm,
    /// so a body that returned early stops being waited for the moment its
    /// notify lands, and the deadline shrinks to what is still owed.
    async fn await_local_obligations(&self, group_key: &str) {
        let Some(state) = self.groups.get(group_key) else {
            return;
        };
        let notified = state.settled.notified();
        tokio::pin!(notified);
        loop {
            notified.as_mut().enable();
            let deadline = {
                let inner = state.state.lock_recover();
                let now = Instant::now();
                let budget = self.drain_budget.duration();
                let mut latest: Option<Instant> = None;
                let mut waiting = false;
                for key in &inner.running {
                    match inner.decided_at.get(key) {
                        // Never cancel-decided: a `RunToCompletion` child is
                        // this host's protected obligation, waited on for as
                        // long as it runs.
                        None => waiting = true,
                        Some(decided_at) => {
                            let expiry = *decided_at + budget;
                            if now < expiry {
                                waiting = true;
                                latest = Some(latest.map_or(expiry, |so_far| so_far.max(expiry)));
                            }
                            // Past its budget: logically cancelled — the rank
                            // its decision seated is already durable, and
                            // whatever the body still does cannot write a
                            // settlement the journal will take.
                        }
                    }
                }
                if !waiting {
                    return;
                }
                latest
            };
            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        () = &mut notified => {}
                        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                    }
                }
                None => notified.as_mut().await,
            }
        }
    }
}
