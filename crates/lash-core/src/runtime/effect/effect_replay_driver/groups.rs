//! The durable effect-group host every SQL backend gets for free (FIG-1564).
//!
//! Layer 1 gave the substrates the *journal shape* of a group — a group row
//! carrying the counter, a `group_key` and `settlement_seq` on each child, and
//! the N1/N2/N3 rules that keep rank a fact. What it did not give them was a
//! **host**: on sqlite and postgres the four contract methods
//! (`supports_effect_groups`, `open_effect_group`, `await_next_settlement`,
//! `close_effect_group`) still inherited their fail-closed defaults, so no
//! production group row was ever written and no child ever carried a
//! `group_key`. This module is that host, written once over
//! [`EffectReplayPersistence`] so both stores delegate to it exactly as they
//! already delegate `execute_effect`.
//!
//! # What is durable and what is process-local
//!
//! Everything a *replay* depends on is in the journal: the group's wake rule,
//! its declared loser disposition, its child count, and the settlement rank of
//! every child that has settled. [`DurableEffectGroups`] holds only what the
//! process that opened the group knows and the journal deliberately does not
//! record — the replay key at each *position* (ADR 0065 fixes the child table's
//! growth at two columns and names a position column as the third copy of a
//! fact the caller already holds), and the cancellation token this process's own
//! children select on.
//!
//! The consequence is the property this layer exists to prove: a second process
//! that reopens the same group re-derives the position map from the group it was
//! handed, and reads every settled rank out of the journal. It does not need —
//! and does not have — the first process's memory.
//!
//! # Three mechanisms, matching the in-memory reference host
//!
//! The inline controller
//! ([`InlineRuntimeEffectController`](crate::InlineRuntimeEffectController)) is
//! the conformance definition of the contract's observable semantics, so this
//! host is written to agree with it mechanism for mechanism:
//!
//! * **Children run on host-owned tasks.** Dropping the caller must not drop the
//!   losers, because `RunToCompletion` says a losing promise keeps running.
//! * **The rank is allocated at settlement by a single allocator.** Here that is
//!   layer 1's single-row counter bump inside the child's own finalize
//!   transaction (N1) rather than the inline tier's per-group lock, which is the
//!   same discipline against the same defect.
//! * **A settlement is served from the record, never re-raced.** Rank
//!   `consumed + 1` is a `SELECT`; once decided it re-reads the same child,
//!   which is what makes a replayed frame observe what the pre-crash frame did.
//!
//! # What this layer is not
//!
//! It is not the drain (FIG-1536). A close under
//! [`LoserDisposition::Cancel`](super::super::group::LoserDisposition::Cancel)
//! cancels the children *this process is running* and each of those journals its
//! own cancellation terminal through its own fence; children of the same group
//! being run by another process, or never claimed at all, are left for the
//! host-owned drain driver, which reads them through
//! [`EffectReplayPersistence::read_unsettled_group_children`] and applies the
//! disposition journaled on the group row. That split is why the disposition is
//! declared at open and durable: the drain never invents one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use lash_sansio::sync::{MutexExt, RwLockExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::runtime::effect::group::{
    CheckedEffectGroup, EffectGroupHandle, EffectGroupMembership, GroupSettlement,
    LoserDisposition, RuntimeEffectGroup,
};

/// How long a caller parked on rank `n` waits before re-reading the journal.
///
/// A settlement written by *another* process reaches this one only by being
/// read, so the wait is a poll and not purely a notification. The in-process
/// [`Notify`] shortens it to nothing for the common case where the settling
/// child is one of this host's own tasks; the interval is what bounds the
/// cross-process case. It matches [`BUSY_POLL`], the same trade-off the claim
/// loop already makes for the same reason.
const SETTLEMENT_POLL: Duration = BUSY_POLL;

/// Every group this driver has open, keyed exactly as ADR 0065 keys them.
///
/// A read-mostly index of per-group states: siblings of *different* groups have
/// no reason to contend, and the state under one key is small and rarely
/// written.
///
/// # Retention, and the one entry that outlives its group
///
/// An entry is removed when the group is closed *and* complete (see
/// `reap_if_complete`). A group whose caller never closes — a frame that
/// panicked between open and close, or a host that drops the session — keeps
/// its entry for the life of the process: a `Vec<String>` of replay keys, a
/// token, and a counter, per such group. This is a bounded-per-group leak and
/// it is named rather than papered over, because the two obvious fixes are both
/// wrong here. A timeout would guess a close the caller never made and cancel
/// `RunToCompletion` losers that are still legitimately running; retiring on
/// "all children finished" without a close would strand a resumed caller that
/// is entitled to read the ranks those children recorded, which is exactly the
/// replay this layer exists to serve. The real reclamation is the drain
/// (FIG-1536): a group whose caller is gone is the drain's to finish, and
/// retiring the entry is a consequence of finishing it, not a policy of its
/// own.
#[derive(Default)]
pub(super) struct DurableEffectGroups {
    open: RwLock<HashMap<String, Arc<OpenGroup>>>,
}

/// One group this process has open.
struct OpenGroup {
    /// The replay key at each position, in child order. The journal names a
    /// settled child by `replay_key`; the contract reports it by position, and
    /// this is the map between them — held rather than stored, because the
    /// caller that opened the group is the party that knows it.
    replay_keys: Vec<String>,
    /// Fired by a close that resolves to [`LoserDisposition::Cancel`]. Children
    /// take child tokens, so cancelling the group cancels exactly the children
    /// this process is still running.
    cancel: CancellationToken,
    state: Mutex<OpenGroupState>,
    /// Woken when one of this process's children journals a settlement, so a
    /// parked caller re-reads immediately instead of waiting out the poll.
    settled: Notify,
    /// How many of this process's dispatched child tasks have not returned yet.
    ///
    /// Counted rather than asked of the journal, because the journal answers a
    /// different question: a child task that has been spawned but has not
    /// claimed its row yet is *outstanding here* and *absent there*. Retiring
    /// the state on the journal's answer alone would drop the cancellation
    /// token out from under a child that has not started, which is exactly the
    /// window a close taken immediately after open lands in.
    outstanding: AtomicUsize,
}

struct OpenGroupState {
    /// The disposition in force. Starts at the group's *declared* disposition
    /// and only ever narrows, so a second close cannot widen what a first one
    /// tightened.
    effective: LoserDisposition,
    closed: bool,
}

impl DurableEffectGroups {
    fn get(&self, group_key: &str) -> Option<Arc<OpenGroup>> {
        self.open.read_recover().get(group_key).cloned()
    }

    fn reap(&self, group_key: &str, state: &Arc<OpenGroup>) {
        let mut open = self.open.write_recover();
        if open
            .get(group_key)
            .is_some_and(|current| Arc::ptr_eq(current, state))
        {
            open.remove(group_key);
        }
    }
}

impl<P: EffectReplayPersistence + 'static, A: AwaitEventBackend + 'static>
    EffectReplayDriver<P, A>
{
    /// Open — or reopen — a durable effect group, dispatching one host-owned
    /// task per child.
    ///
    /// The group row is written first, in its own transaction, before any child
    /// claims (N2), and the record the substrate reports back is what the reopen
    /// fence is judged against: a group already recorded under this key with a
    /// different child count, wake rule, or declared disposition is refused
    /// rather than reopened. Child count is the reason the fence must be durable
    /// — a shrunk child vec renumbers every rank above the truncation, and the
    /// per-child envelope-hash fence cannot see it.
    ///
    /// A reopen dispatches its children again on purpose, and that is safe for
    /// exactly the reason the journal exists: each child goes through
    /// [`execute_effect`](EffectReplayDriver::execute_effect), so a child whose
    /// terminal is already recorded replays it, a child under another process's
    /// live lease waits and then replays it, and only a child with an expired
    /// lease or no row at all is executed. A reopen *inside this process* skips
    /// the dispatch entirely, because those children are still running on this
    /// host's tasks and re-running them would double the side effects the first
    /// dispatch is still producing.
    ///
    /// The returned handle is always at `consumed = 0`: only the caller knows
    /// how far it consumed, and a caller resuming from a durable continuation
    /// restores its own cursor with [`EffectGroupHandle::restored`].
    pub async fn open_effect_group(
        self: &Arc<Self>,
        scope: &ExecutionScope,
        checked: CheckedEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let journal_identity = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        let (group, executors) = checked.into_parts();
        let handle = EffectGroupHandle::new(&group);
        let record = EffectGroupRecord::from_group(
            &group,
            journal_identity.key(),
            journal_identity.session_id().map(str::to_string),
            self.clock.timestamp_ms(),
        );
        let persisted = self.persistence.open_group(&record).await?;
        fence_reopen(&record, &persisted)?;

        let replay_keys = replay_keys_of(&group)?;
        let state = {
            let mut open = self.groups.open.write_recover();
            if open.contains_key(group.group_key()) {
                // Already running here. The durable fence above has already
                // judged the shape, so there is nothing left to check and
                // nothing to dispatch.
                return Ok(handle);
            }
            let state = Arc::new(OpenGroup {
                outstanding: AtomicUsize::new(replay_keys.len()),
                replay_keys,
                cancel: CancellationToken::new(),
                state: Mutex::new(OpenGroupState {
                    effective: persisted.loser_disposition,
                    closed: false,
                }),
                settled: Notify::new(),
            });
            open.insert(group.group_key().to_string(), Arc::clone(&state));
            state
        };
        self.dispatch_group_children(scope, &state, group, executors);
        Ok(handle)
    }

    /// Spawns one host-owned task per child.
    ///
    /// The task set is the host's, not the caller's: a group whose children ran
    /// inside the caller's future would drop its losers the moment the caller
    /// was dropped, which is precisely what `RunToCompletion` forbids.
    fn dispatch_group_children(
        self: &Arc<Self>,
        scope: &ExecutionScope,
        state: &Arc<OpenGroup>,
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    ) {
        let group_key = Arc::<str>::from(group.group_key());
        for (child, executor) in group.children().iter().cloned().zip(executors) {
            let driver = Arc::clone(self);
            let state = Arc::clone(state);
            let group_key = Arc::clone(&group_key);
            let scope = scope.clone();
            let cancel = state.cancel.child_token();
            crate::task::spawn(async move {
                // The result is discarded here on purpose: a child's outcome is
                // reported to its caller through the journal, by rank, and this
                // task's return value has no other reader. A failure is already
                // journaled as that child's terminal.
                let _ = Box::pin(driver.execute_effect_cancellable(
                    &scope,
                    child,
                    executor,
                    Some(&cancel),
                ))
                .await;
                driver.group_child_finished(&group_key, &state).await;
            });
        }
    }

    /// Wakes any parked caller and retires the group's process-local state once
    /// the group is both closed and complete.
    ///
    /// Retention is bounded by close **and** completion, for the same reason it
    /// is on the inline tier: a `RunToCompletion` loser keeps settling after the
    /// caller is gone, and its settlement is exactly what this state is here to
    /// let a caller read. Completion is two claims, not one: this process's own
    /// child tasks must have returned, *and* the journal must hold no unsettled
    /// child of the group — a sibling being run by another process is a reason
    /// to keep serving this caller its settlements.
    async fn group_child_finished(&self, group_key: &str, state: &Arc<OpenGroup>) {
        state.settled.notify_waiters();
        state.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.reap_if_complete(group_key, state).await;
    }

    /// Retire the process-local state of a group that is closed to its caller
    /// and has nothing left to settle.
    async fn reap_if_complete(&self, group_key: &str, state: &Arc<OpenGroup>) {
        let closed = state.state.lock_recover().closed;
        if !closed || state.outstanding.load(Ordering::Acquire) != 0 {
            return;
        }
        if self
            .persistence
            .read_unsettled_group_children(group_key)
            .await
            .is_ok_and(|unsettled| unsettled.is_empty())
        {
            self.groups.reap(group_key, state);
        }
    }

    /// Await the settlement at rank `handle.consumed() + 1`, serving it from the
    /// journal.
    ///
    /// The cursor advances on exactly the settlement returned, so a cancelled or
    /// refused await leaves rank `n` to be read again — by this caller or by a
    /// replayed frame — rather than skipped.
    pub async fn await_next_group_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let state = self
            .groups
            .get(handle.group_key())
            .ok_or_else(|| closed_group_error(handle.group_key()))?;
        if handle.is_exhausted() {
            return Err(group_shape_error(format!(
                "durable effect group {} has all {} settlements consumed; check \
                 is_exhausted() before awaiting rather than awaiting past the group",
                handle.group_key(),
                handle.children()
            )));
        }
        loop {
            let notified = state.settled.notified();
            tokio::pin!(notified);
            // Enabled *before* the journal read, so a sibling that settles
            // between the read and the park is caught by this future rather
            // than slept through: `Notify::notified()` only starts listening
            // when it is first polled, and `notify_waiters` wakes listeners,
            // not arrivals.
            notified.as_mut().enable();
            let closed = state.state.lock_recover().closed;
            if closed {
                return Err(closed_group_error(handle.group_key()));
            }
            let rank = handle.consumed() + 1;
            if let Some(stored) = self
                .persistence
                .read_group_settlement(handle.group_key(), rank)
                .await?
            {
                let settlement = self.decode_settlement(handle.group_key(), &state, stored)?;
                handle.advance()?;
                return Ok(settlement);
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                        format!(
                            "the await of settlement {rank} of durable effect group {} \
                             was cancelled; the group's rank is untouched and a later \
                             await resumes at the same settlement",
                            handle.group_key()
                        ),
                    ));
                }
                () = &mut notified => {}
                () = self.clock.sleep(SETTLEMENT_POLL) => {}
            }
        }
    }

    /// Turns a journal row into the settlement the contract delivers.
    ///
    /// The position comes from the position map rather than from a column: a
    /// settled row names its child by `replay_key`, which is already the child's
    /// durable identity, and the host that opened the group holds the mapping
    /// exactly.
    fn decode_settlement(
        &self,
        group_key: &str,
        state: &OpenGroup,
        stored: StoredGroupSettlement,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let vocabulary = self.vocabulary();
        let position = state
            .replay_keys
            .iter()
            .position(|key| *key == stored.replay_key)
            .ok_or_else(|| {
                vocabulary.error(
                    EffectReplayFailure::CorruptRow,
                    format!(
                        "durable effect group {group_key} recorded a settlement for \
                         replay key `{}`, which is not one of its {} children",
                        stored.replay_key,
                        state.replay_keys.len()
                    ),
                )
            })?;
        let outcome = match EffectRowStatus::parse(&stored.status) {
            Some(EffectRowStatus::Completed) => {
                let outcome_json = stored.outcome_json.ok_or_else(|| {
                    vocabulary.error(
                        EffectReplayFailure::CorruptRow,
                        EffectRowDefect::MissingOutcome.message(),
                    )
                })?;
                Ok(serde_json::from_str::<RuntimeEffectOutcome>(&outcome_json)
                    .map_err(|err| vocabulary.decode_error(err))?)
            }
            Some(EffectRowStatus::Failed) => {
                let error_json = stored.error_json.ok_or_else(|| {
                    vocabulary.error(
                        EffectReplayFailure::CorruptRow,
                        EffectRowDefect::MissingError.message(),
                    )
                })?;
                Err(
                    serde_json::from_str::<RuntimeEffectControllerError>(&error_json)
                        .map_err(|err| vocabulary.decode_error(err))?,
                )
            }
            // A rank is allocated in the same transaction that writes the
            // terminal, so a ranked row is terminal by construction. Anything
            // else is a corrupt journal and is refused rather than reported as
            // some plausible outcome.
            _ => {
                return Err(vocabulary.error(
                    EffectReplayFailure::CorruptRow,
                    format!(
                        "child `{}` of durable effect group {group_key} holds settlement \
                         rank {} under status `{}`; a rank is allocated only with a \
                         terminal",
                        stored.replay_key, stored.sequence, stored.status
                    ),
                ));
            }
        };
        Ok(GroupSettlement {
            position,
            sequence: stored.sequence,
            outcome,
        })
    }

    /// Release the caller's interest in the group, applying the disposition the
    /// close resolves to.
    ///
    /// Idempotent: a group this process is not running — because it was already
    /// completed and retired, or because the closing frame is a replay in a
    /// fresh process — closes successfully, since there is nothing left here for
    /// a disposition to decide and the declared one is journaled for the drain.
    ///
    /// Under [`LoserDisposition::Cancel`] the children this process runs are
    /// cancelled, and each writes its cancellation as its own terminal through
    /// its own fence, allocating a rank exactly as any other outcome would.
    /// Children of this group that this process does not run are the drain's
    /// (FIG-1536), which applies the disposition on the group row.
    pub async fn close_effect_group(
        &self,
        handle: &EffectGroupHandle,
        requested: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Some(state) = self.groups.get(handle.group_key()) else {
            return Ok(());
        };
        let cancelled = {
            let mut inner = state.state.lock_recover();
            // Resolved against the disposition in force rather than the declared
            // one, so narrowing is cumulative: a group closed as `Cancel` cannot
            // be widened back by a second close either.
            let effective = LoserDisposition::resolve_close(inner.effective, requested)?;
            inner.effective = effective;
            inner.closed = true;
            matches!(effective, LoserDisposition::Cancel)
        };
        if cancelled {
            state.cancel.cancel();
        }
        state.settled.notify_waiters();
        self.reap_if_complete(handle.group_key(), &state).await;
        Ok(())
    }
}

/// The replay key of each child, in position order.
///
/// A child without one could never be claimed, so a group containing one is
/// refused at open rather than at the moment its rank fails to resolve.
///
/// Distinctness is not rechecked here: [`RuntimeEffectGroup::try_new`] is the
/// sole constructor and refuses two children sharing a replay key, so the
/// position map this builds is one entry per journaled child by construction.
fn replay_keys_of(group: &RuntimeEffectGroup) -> Result<Vec<String>, RuntimeEffectControllerError> {
    group
        .children()
        .iter()
        .enumerate()
        .map(|(position, child)| {
            child
                .invocation
                .replay_key()
                .map(str::to_string)
                .ok_or_else(|| {
                    group_shape_error(format!(
                        "child {position} of durable effect group {} has no replay key, \
                         so it can never be claimed and its rank can never be served",
                        group.group_key()
                    ))
                })
        })
        .collect()
}

/// Refuses a reopen whose shape disagrees with the group already recorded under
/// this key.
fn fence_reopen(
    opening: &EffectGroupRecord,
    persisted: &EffectGroupRecord,
) -> Result<(), RuntimeEffectControllerError> {
    if opening.children != persisted.children {
        return Err(group_shape_error(format!(
            "durable effect group {} is recorded with {} children but was reopened \
             with {}; a changed child count renumbers every rank above the change, \
             so the settlements already consumed would no longer name the children \
             that produced them",
            opening.group_key, persisted.children, opening.children
        )));
    }
    if opening.wake != persisted.wake {
        return Err(group_shape_error(format!(
            "durable effect group {} is recorded under wake rule {:?} but was \
             reopened under {:?}; the wake rule is journaled identity and a reopen \
             may not change it",
            opening.group_key, persisted.wake, opening.wake
        )));
    }
    if opening.loser_disposition != persisted.loser_disposition {
        return Err(group_shape_error(format!(
            "durable effect group {} declared loser disposition {:?} at open but was \
             reopened declaring {:?}; the declared disposition is what a drain of \
             this group applies, so a reopen may not restate it",
            opening.group_key, persisted.loser_disposition, opening.loser_disposition
        )));
    }
    Ok(())
}

/// The terminal a cancelled group child journals.
///
/// Named the same way and carrying the same code as the in-memory reference
/// host's, because a caller reading a cancelled child's outcome must not be able
/// to tell which tier ran it.
pub(super) fn child_cancelled_error(
    membership: Option<&EffectGroupMembership>,
) -> RuntimeEffectControllerError {
    let (group_key, position) = match membership {
        Some(membership) => (membership.group_key.as_str(), membership.position),
        // Unreachable through `dispatch_group_children`, whose children are
        // group members by construction. Kept honest rather than panicking: the
        // cancellation is still this effect's terminal whatever the envelope
        // says.
        None => ("<ungrouped>", 0),
    };
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        format!(
            "child {position} of durable effect group {group_key} was cancelled by \
             the group's declared loser disposition; the cancellation is this \
             child's terminal"
        ),
    )
}

fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

fn closed_group_error(group_key: &str) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "durable effect group {group_key} is closed to its caller; a closed group's \
         remaining children settle under host ownership and the caller may not \
         observe them"
    ))
}
