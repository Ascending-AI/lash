//! The loser drain, over the journal both SQL tiers already keep (FIG-1536).
//!
//! [`group_drain`](crate::runtime::effect::group_drain) states the contract and
//! the three guards; this module is the one implementation of it, written over
//! [`EffectReplayRowStore`] so sqlite and postgres are held to the same laws
//! by the same code rather than by two copies.
//!
//! # Why a pass reads the journal twice
//!
//! A pass reads the unsettled children to get its queue, and reads them again
//! after it has attempted any of them. The second read is not a paranoia check:
//! it is what makes [`ChildDrainOutcome::Settled`] a *read* fact instead of an
//! inference from a return value that cannot carry it. `execute_effect` answers
//! `Err` both for a child whose own terminal is a failure — journaled, ranked,
//! settled — and for a drain whose lease was fenced out before it could write
//! anything, and no inspection of that error separates the two reliably. The
//! journal separates them exactly, and the same read decides whether the group
//! became reclaimable, which the pass owes anyway.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::groups::{LocalDrainConflict, drain_deferred_error, group_shape_error};
use super::*;
use crate::runtime::effect::group::LoserPolicy;
use crate::runtime::effect::group_drain::{
    ChildDrainOutcome, DrainedChild, GroupDrainReport, StoreEffectGroupDrain,
};

/// The drain a durable effect host hands out: one effect-replay driver, which
/// already holds the host wiring that says how a journaled child is run.
struct DurableGroupDrain<P, A> {
    driver: Arc<StoreEffectReplayDriver<P, A>>,
}

#[async_trait]
impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static> StoreEffectGroupDrain
    for DurableGroupDrain<P, A>
{
    async fn drain_group(
        &self,
        group_key: &str,
        cancel: &CancellationToken,
    ) -> Result<GroupDrainReport, RuntimeEffectControllerError> {
        self.driver.drain_effect_group(group_key, cancel).await
    }
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static>
    StoreEffectReplayDriver<P, A>
{
    /// Hand back the drain over this driver's journal.
    ///
    /// It resolves children through the same
    /// [`GroupExecutors`](crate::runtime::effect::group_drain::GroupExecutors)
    /// the open and every retry resolve through, registered
    /// once on this driver by the host that owns the runners a journaled command
    /// needs — not reached for out of whatever session is in scope when a group
    /// turns out to need draining. That is the whole point of the seam: a drain
    /// assembled from ambient state would run a losing child against a session
    /// that never opened it, and a drain with a *different* resolver from the
    /// open would run it against different code than the caller would have.
    ///
    /// A host that has registered no resolver still gets a drain, and its passes
    /// report every child as
    /// [`NoExecutor`](crate::ChildDrainOutcome::NoExecutor) — the queue is real
    /// and another host can still finish it, which is exactly what that outcome
    /// says.
    ///
    /// Takes `Arc<Self>` because the drain executes children through this same
    /// driver: one lease identity, one owner id, one journal.
    pub fn into_group_drain(self: Arc<Self>) -> Arc<dyn StoreEffectGroupDrain> {
        Arc::new(DurableGroupDrain { driver: self })
    }

    /// One drain pass. See [`StoreEffectGroupDrain::drain_group`].
    async fn drain_effect_group(
        self: &Arc<Self>,
        group_key: &str,
        cancel: &CancellationToken,
    ) -> Result<GroupDrainReport, RuntimeEffectControllerError> {
        match self.groups.local_drain_conflict(group_key) {
            Some(LocalDrainConflict::OpenToACaller) => {
                return Err(drain_deferred_error(format!(
                    "durable effect group {group_key} is open to a caller in this \
                     process; the drain reclaims closed groups, and finishing this \
                     one's children now would settle them out from under the caller \
                     entitled to read them; retry once that caller has closed"
                )));
            }
            Some(LocalDrainConflict::RunningHere { outstanding }) => {
                return Err(drain_deferred_error(format!(
                    "durable effect group {group_key} is closed here but still has \
                     {outstanding} child(ren) this process dispatched and has not seen \
                     settle; the drain reclaims children whose executor is gone, and \
                     this host would be reclaiming them from itself; retry once they \
                     have settled"
                )));
            }
            None => {}
        }
        let Some(record) = self.row_store.read_group(group_key).await? else {
            return Err(group_shape_error(format!(
                "no durable effect group is recorded under {group_key}; the drain \
                 applies the disposition the group declared and will not invent \
                 one for a group the journal does not hold"
            )));
        };
        let queued = self
            .row_store
            .read_unsettled_group_children(group_key)
            .await?;
        let now = self.clock.timestamp_ms();
        let mut children = Vec::with_capacity(queued.len());
        let mut attempted = false;
        // Once the token has fired, the rest of the queue is reported as
        // untouched rather than examined: a pass that kept reading after being
        // told to stop would be answering a question nobody is waiting for, and
        // a report that omitted the tail would read as an empty queue.
        let mut interrupted = false;
        for child in queued {
            let outcome = if interrupted || cancel.is_cancelled() {
                interrupted = true;
                ChildDrainOutcome::Interrupted
            } else {
                let outcome = self.drain_group_child(&record, &child, now, cancel).await?;
                interrupted = outcome == ChildDrainOutcome::Interrupted;
                attempted |= outcome == ChildDrainOutcome::Settled;
                outcome
            };
            children.push(DrainedChild {
                replay_key: child.replay_key,
                outcome,
            });
        }
        if attempted {
            self.confirm_settlements(group_key, &mut children).await?;
        }
        // Retention is closed AND outstanding == 0 AND journal-empty; the drain
        // can only have moved the third of those, so it asks rather than
        // decides. A group this process never opened has no state to retire and
        // this is a no-op — the common case, since the drain exists for groups
        // whose opener is gone.
        self.retire_group_if_complete(group_key).await;
        Ok(GroupDrainReport {
            group_key: group_key.to_string(),
            disposition: record.loser_disposition,
            children,
        })
    }

    /// What the pass does with one child that held no rank.
    async fn drain_group_child(
        self: &Arc<Self>,
        record: &EffectGroupRecord,
        child: &UnsettledGroupChild,
        now_ms: u64,
        cancel: &CancellationToken,
    ) -> Result<ChildDrainOutcome, RuntimeEffectControllerError> {
        // Classify on the §4 commit state first — it is the durable fact every
        // other column answers to. The pairings below are the ones the
        // arbitration transaction writes atomically, so anything else is a
        // torn journal: reported per child rather than raised as the pass's
        // error, because a torn row is one row. Failing the pass would make
        // every healthy sibling of that group undrainable for as long as the
        // corruption lasted — the group would be exactly as stuck as if
        // nothing had been detected, with the detection as the cause.
        match (child.commit_state, &child.state) {
            // Committed but undrained: the crash window between the §4 point
            // and the §5 discharge. The child's declared intents are durable by
            // the time its commit lands — they journal inside the attempt
            // that produces the final record — so the discharge owed here is
            // the rank write behind the commit-order barrier, not a
            // re-execution.
            (Some(EffectCommitState::Committed), Some(EffectRowState::Settled(_))) => {
                return self.discharge_committed_child(record, child).await;
            }
            // Boundary-committed mid-attempt: the §4 commit journaled the
            // decision, the position and the drain input while the terminal is
            // still owed. This is the committed-before-discharge crash window,
            // not corruption, and it is resumed by re-execution below — the
            // journaled intents replay and the settle path's `AlreadyCommitted`
            // answer finishes the discharge with the fresh terminal. A
            // committed child is protected in both directions: the cancel
            // disposition cannot decide it, and the rank write must not run
            // ahead of the terminal its `drain_input` still owes.
            (Some(EffectCommitState::Committed), Some(EffectRowState::InProgress)) => {
                if child.lease_expires_at_ms > now_ms {
                    return Ok(ChildDrainOutcome::LeaseLive {
                        expires_at_ms: child.lease_expires_at_ms,
                    });
                }
            }
            // Undecided and unraced: a never-claimed child (no replay row, no
            // commit state) or a claimed one still in flight (`pending`) — the
            // work the disposition and the lease rule below decide over.
            (None | Some(EffectCommitState::Pending), None | Some(EffectRowState::InProgress)) => {
                if record.loser_disposition == LoserPolicy::Cancel {
                    // §4: the cancel decision is a durable journal write, not a
                    // signal the child's own process must still be alive to act
                    // on — the drain exists precisely for an opener that
                    // declared the disposition and never journaled it. A live
                    // lease does not stop the decision: it races the claim's
                    // finalize at the commit-state CAS, which is the
                    // linearization point that decides it.
                    return self.decide_cancel_child(record, child).await;
                }
                if child.lease_expires_at_ms > now_ms {
                    return Ok(ChildDrainOutcome::LeaseLive {
                        expires_at_ms: child.lease_expires_at_ms,
                    });
                }
            }
            // Everything else pairs facts no arbitration transaction commits:
            // a commit state with no matching replay state, a terminal row
            // whose state never moved off pending, a `drained` or
            // `cancel_decided` row holding no rank.
            (_, state) => {
                return Ok(ChildDrainOutcome::Corrupt {
                    status: match state {
                        Some(state) => state.status_column().to_string(),
                        None => "<no replay row>".to_string(),
                    },
                });
            }
        }
        let envelope = self.decode_drained_child(&record.group_key, child)?;
        let scope = ExecutionScope::from_journal_key(&child.scope_id).ok_or_else(|| {
            self.vocabulary().error(
                EffectReplayFailure::CorruptRow,
                format!(
                    "child `{}` of durable effect group {} records scope id `{}`, \
                     which no version of this runtime writes; the drain will not \
                     re-execute an effect under a scope it had to guess",
                    child.replay_key, record.group_key, child.scope_id
                ),
            )
        })?;
        // A host that registered no resolver answers `NoExecutor` for every
        // child, which is the accurate report: the queue is real and another
        // host can still finish it.
        let executor = match self.group_executors() {
            Ok(executors) => executors.executor_for(&envelope),
            Err(_) => None,
        };
        let Some(executor) = executor else {
            return Ok(ChildDrainOutcome::NoExecutor);
        };
        // The `Ok`/`Err` distinction is dropped on purpose. A child reports its
        // outcome to its caller through the journal, by rank, and this pass is
        // not that caller: it has no position map and no cursor. Whether the
        // child settled is read back from the journal, which is the only place
        // that answers it for a claim this pass may not have won. What is *not*
        // dropped is `Ok(None)` — a busy claim is the one answer the journal
        // cannot give afterwards, because by then the competing executor may
        // have finished and the row would read as an ordinary settlement.
        let execution = Box::pin(self.execute_effect_yielding(&scope, envelope, executor));
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(ChildDrainOutcome::Interrupted),
            result = execution => Ok(match result {
                Ok(None) => ChildDrainOutcome::Contested,
                Ok(Some(_)) | Err(_) => ChildDrainOutcome::Settled,
            }),
        }
    }

    /// Downgrades every claimed settlement the journal does not confirm.
    ///
    /// Called once per pass rather than once per child: one read answers the
    /// question for all of them, and a per-child read would multiply the pass's
    /// journal traffic by its queue length to learn the same thing.
    async fn confirm_settlements(
        &self,
        group_key: &str,
        children: &mut [DrainedChild],
    ) -> Result<(), RuntimeEffectControllerError> {
        let still_unsettled = self
            .row_store
            .read_unsettled_group_children(group_key)
            .await?
            .into_iter()
            .map(|child| child.replay_key)
            .collect::<std::collections::BTreeSet<_>>();
        downgrade_unconfirmed(children, &still_unsettled);
        Ok(())
    }

    /// Journals the cancel disposition's decision for one undecided child —
    /// the durable half of a `Cancel` drain, and the reason a cancelled child
    /// needs no live process to reach its terminal.
    ///
    /// A `FinalCommitted` answer means the child's own final record won the
    /// §4 point while this pass ran; the child is then exactly the
    /// committed-but-undrained case this same pass discharges.
    async fn decide_cancel_child(
        &self,
        record: &EffectGroupRecord,
        child: &UnsettledGroupChild,
    ) -> Result<ChildDrainOutcome, RuntimeEffectControllerError> {
        let cancelled = child_cancelled_error(&record.group_key, child.position as usize);
        let canonical = CanonicalRuntimeEffectEnvelope::capture(
            &self.decode_drained_child(&record.group_key, child)?,
        )?;
        let outcome = self
            .row_store
            .decide_cancel(&EffectCancelRequest {
                group_key: record.group_key.clone(),
                replay_key: child.replay_key.clone(),
                terminal: EffectTerminal::Failed {
                    error_json: serde_json::to_string(&cancelled)
                        .map_err(|err| self.vocabulary().encode_error(err))?,
                },
                envelope_json: serde_json::to_string(&canonical)
                    .map_err(|err| self.vocabulary().encode_error(err))?,
                envelope_hash: canonical.hash().to_string(),
            })
            .await?;
        match outcome {
            EffectCancelOutcome::Decided { .. } | EffectCancelOutcome::AlreadyDecided { .. } => {
                Ok(ChildDrainOutcome::Decided)
            }
            EffectCancelOutcome::FinalCommitted { .. } => {
                self.discharge_committed_child(record, child).await
            }
        }
    }

    /// Seats one committed child at its settlement rank — the §5 discharge the
    /// crash window between the §4 decision and the rank write can leave owed.
    ///
    /// `Blocked` reports as [`ChildDrainOutcome::Contested`]: a lower-commit
    /// sibling's drain is still in flight on some host, which is the same
    /// "stays queued, a later pass finishes it" fact a contested claim reports.
    async fn discharge_committed_child(
        &self,
        record: &EffectGroupRecord,
        child: &UnsettledGroupChild,
    ) -> Result<ChildDrainOutcome, RuntimeEffectControllerError> {
        let outcome = self
            .row_store
            .discharge_child(&EffectDischargeRequest {
                group_key: record.group_key.clone(),
                scope_id: child.scope_id.clone(),
                replay_key: child.replay_key.clone(),
                terminal: None,
            })
            .await?;
        Ok(match outcome {
            EffectDischargeOutcome::Discharged { .. }
            | EffectDischargeOutcome::AlreadyDischarged { .. } => ChildDrainOutcome::Decided,
            EffectDischargeOutcome::Blocked => ChildDrainOutcome::Contested,
        })
    }

    /// Rebuilds the effect a journaled child records.
    ///
    /// The membership column holds the accepted envelope itself — the whole
    /// `RuntimeEffectEnvelope` the opener declared, retained so a successor
    /// that never saw it can still run the child. Decoding it and handing the
    /// result back to `execute_effect` reproduces the same canonical envelope
    /// and the same hash — `capture` normalizes the bytes the hash is taken
    /// over — so the rebuilt child passes the same replay fence. A pass that
    /// rebuilt the envelope from anything else would be refused by that
    /// fence, which is the correct refusal and a useless one.
    fn decode_drained_child(
        &self,
        group_key: &str,
        child: &UnsettledGroupChild,
    ) -> Result<RuntimeEffectEnvelope, RuntimeEffectControllerError> {
        let vocabulary = self.vocabulary();
        if child.command_version != super::super::TOOL_CHILD_REQUEST_VERSION {
            return Err(vocabulary.error(
                EffectReplayFailure::CorruptRow,
                format!(
                    "child `{}` of durable effect group {group_key} was minted \
                     under command version {} but this build reconstructs \
                     version {}; a request that cannot be read is corruption, \
                     not a guess",
                    child.replay_key,
                    child.command_version,
                    super::super::TOOL_CHILD_REQUEST_VERSION,
                ),
            ));
        }
        serde_json::from_str(&child.envelope_json).map_err(|err| {
            vocabulary.error(
                EffectReplayFailure::CorruptRow,
                format!(
                    "child `{}` of durable effect group {group_key} records an \
                     accepted envelope this build cannot decode: {err}",
                    child.replay_key
                ),
            )
        })
    }
}

/// Turns a claimed settlement the journal does not confirm into a contest.
///
/// Split out from the read so the decision can be stated and tested on its own.
/// It has to be defensive: reaching it needs this pass's own claim to have been
/// stolen mid-flight, which needs its renewal loop to have already failed, which
/// in a live process means the substrate expired a lease it was being asked to
/// keep — clock skew, a substrate pause, a long stall. That is not something a
/// conformance law can stage through a host surface, and it is exactly the
/// event that must not be reported as a settlement.
fn downgrade_unconfirmed(
    children: &mut [DrainedChild],
    still_unsettled: &std::collections::BTreeSet<String>,
) {
    for child in children.iter_mut() {
        if matches!(
            child.outcome,
            ChildDrainOutcome::Settled | ChildDrainOutcome::Decided
        ) && still_unsettled.contains(&child.replay_key)
        {
            child.outcome = ChildDrainOutcome::Contested;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained(replay_key: &str, outcome: ChildDrainOutcome) -> DrainedChild {
        DrainedChild {
            replay_key: replay_key.to_string(),
            outcome,
        }
    }

    /// Only an unconfirmed *settled-or-decided* claim is downgraded, and the
    /// journal's answer is what decides it.
    ///
    /// The three ways to get this wrong are all here: downgrading a settlement
    /// the journal does confirm (which loses a real settlement from the count),
    /// leaving an unconfirmed one alone (which reports a settlement the pass
    /// cannot see), and rewriting an outcome that never claimed a journal write
    /// in the first place — a skipped, interrupted, or torn child says something
    /// the journal read has no bearing on.
    #[test]
    fn only_an_unconfirmed_claimed_settlement_becomes_a_contest() {
        let mut children = vec![
            drained("confirmed", ChildDrainOutcome::Settled),
            drained("unconfirmed", ChildDrainOutcome::Settled),
            drained("decided", ChildDrainOutcome::Decided),
            drained("undecided", ChildDrainOutcome::Decided),
            drained("skipped", ChildDrainOutcome::LeaseLive { expires_at_ms: 1 }),
            drained("stopped", ChildDrainOutcome::Interrupted),
            drained(
                "torn",
                ChildDrainOutcome::Corrupt {
                    status: "completed".to_string(),
                },
            ),
        ];
        // Every child except the ones whose writes did commit is still in the
        // journal's unsettled set, so anything the pass rewrites here it
        // rewrote wrongly.
        let still_unsettled = ["unconfirmed", "undecided", "skipped", "stopped", "torn"]
            .into_iter()
            .map(str::to_string)
            .collect();

        downgrade_unconfirmed(&mut children, &still_unsettled);

        assert_eq!(children[0].outcome, ChildDrainOutcome::Settled);
        assert_eq!(children[1].outcome, ChildDrainOutcome::Contested);
        assert_eq!(children[2].outcome, ChildDrainOutcome::Decided);
        assert_eq!(children[3].outcome, ChildDrainOutcome::Contested);
        assert_eq!(
            children[4].outcome,
            ChildDrainOutcome::LeaseLive { expires_at_ms: 1 }
        );
        assert_eq!(children[5].outcome, ChildDrainOutcome::Interrupted);
        assert_eq!(
            children[6].outcome,
            ChildDrainOutcome::Corrupt {
                status: "completed".to_string()
            }
        );
    }
}
