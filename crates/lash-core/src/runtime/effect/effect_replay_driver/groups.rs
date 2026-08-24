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
//! It is not the drain. That shipped beside it as
//! [`group_drain`](crate::runtime::effect::group_drain) (FIG-1536), reading this
//! layer's [`EffectReplayPersistence::read_unsettled_group_children`] and
//! executing what it finds through host-supplied executors — the reclamation
//! this layer's leak note below promises.
//!
//! The split between them is *not* symmetric across dispositions, and the reason
//! is what each side can honestly write:
//!
//! * Under
//!   [`LoserPolicy::Cancel`](super::super::group::LoserPolicy::Cancel),
//!   a close cancels the children this process is running and each of them
//!   journals its own cancellation terminal through its own fence. The drain
//!   never touches a `Cancel` group: a terminal for a child it is not running is
//!   a terminal it would have to invent, and a cancellation the effect never saw
//!   is a lie at a rank a real settlement would have taken. It reports such
//!   children as declared-cancelled and leaves them.
//! * Under `RunToCompletion` the drain is the executor of last resort, and the
//!   disposition it applies is the one journaled on the group row at open. That
//!   is why the disposition is durable: the drain never invents one.
//!
//! The residual is therefore narrow and named: children of a `Cancel` group
//! whose process died between open and close stay `in_progress` until their
//! rows are retired with the rest of the group's journal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use lash_sansio::sync::{MutexExt, RwLockExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::runtime::effect::group::{
    EffectGroupHandle, EffectGroupMembership, GroupSettlement, LoserPolicy, RuntimeEffectGroup,
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
///
/// That drain has now shipped
/// ([`group_drain`](crate::runtime::effect::group_drain)), and it reclaims what
/// it was promised and no more. It is named a *closed* group's key by its host,
/// and the entry it can retire is one whose group is closed, whose outstanding
/// count has reached zero, and whose journal it has just emptied. An entry whose
/// caller never closed is still here for the life of the process: nothing about
/// the drain tells it apart from a caller that has not gotten around to closing
/// yet, which is precisely the judgment this note refuses to guess at.
/// Work on a group that this process is doing itself, which a drain pass here
/// would race.
pub(super) enum LocalDrainConflict {
    /// A caller here opened the group and has not closed it.
    OpenToACaller,
    /// The group is closed here, but children this process dispatched have not
    /// reported settling yet.
    RunningHere {
        /// How many, so the refusal can say it.
        outstanding: usize,
    },
}

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
    /// Fired by a close that resolves to [`LoserPolicy::Cancel`]. Children
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
    effective: LoserPolicy,
    closed: bool,
}

impl DurableEffectGroups {
    fn get(&self, group_key: &str) -> Option<Arc<OpenGroup>> {
        self.open.read_recover().get(group_key).cloned()
    }

    /// Whether this process holds `group_key` open on behalf of a caller that
    /// has not closed it.
    ///
    /// The drain's local guard. A closed group's entry may outlive the close —
    /// that is what serves a `RunToCompletion` loser's settlement to a caller
    /// that is gone — so "present here" is not the question; "still owed to a
    /// caller" is.
    fn is_open_to_a_caller(&self, group_key: &str) -> bool {
        self.open
            .read_recover()
            .get(group_key)
            .is_some_and(|state| !state.state.lock_recover().closed)
    }

    /// Why this process must not drain `group_key`, if it must not.
    ///
    /// Both answers are about work *this* host is doing, which is the only kind
    /// the drain can see and the only kind it can race. The second is easy to
    /// miss and cheap to get wrong: a host that closes a `RunToCompletion` group
    /// and drains it in the same breath finds the group no longer open to a
    /// caller, and would happily reclaim a child its own executor is still
    /// running but has stalled long enough for the lease to lapse — stealing
    /// from itself, which is the one race the claim fence cannot narrate away
    /// because both sides are this process.
    pub(super) fn local_drain_conflict(&self, group_key: &str) -> Option<LocalDrainConflict> {
        if self.is_open_to_a_caller(group_key) {
            return Some(LocalDrainConflict::OpenToACaller);
        }
        let outstanding = self.get(group_key)?.outstanding.load(Ordering::Acquire);
        (outstanding != 0).then_some(LocalDrainConflict::RunningHere { outstanding })
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
    /// A host with no registered [`GroupExecutors`] resolver refuses here —
    /// coherently with `supports_effect_groups`, with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// and **before the group row is written**, so a refused open journals
    /// nothing at all.
    ///
    /// Every child is resolved through the resolver **before the group row is
    /// written**, and on a group the journal does not yet hold, any child the
    /// resolver declines refuses the whole open with a typed group-shape error:
    /// a child with no runner is a routing fact, not an outcome, and a group
    /// whose child can never settle makes every rank above it unservable. The
    /// refusal journals nothing, so the retry an operator makes before fixing
    /// the wiring is refused the same way rather than being read as a reopen.
    /// A **reopen** — a group the journal already holds — refuses nothing: a
    /// journaled group meeting a deployment that lost one child's runner is a
    /// deployment change, which the drain reports as `NoExecutor` (ADR 0065)
    /// rather than something this open may deny, since the group is already
    /// recorded and refusing it here would only make the ranks it already holds
    /// unreadable. A reopen *inside this process* resolves nothing at all.
    ///
    /// The returned handle is always at `consumed = 0`: only the caller knows
    /// how far it consumed, and a caller resuming from a durable continuation
    /// restores its own cursor with [`EffectGroupHandle::restored`].
    pub async fn open_effect_group(
        self: &Arc<Self>,
        scope: &ExecutionScope,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let journal_identity = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        // Ahead of the group row on purpose: a host that does not implement
        // groups must journal nothing when it refuses one.
        self.group_executors()?;
        let handle = EffectGroupHandle::new(&group);
        // Resolved ahead of the group row for the same reason, and it is the
        // routing refusal that needs it: a refusal that had already written the
        // group row would turn every later attempt at the same key into a
        // reopen, and a reopen passes a miss through by design — so the retry
        // would open *successfully* around a child that can never settle and
        // park its caller on a rank nothing will ever allocate. A refusal that
        // strands the caller one attempt later is the strand this seam exists to
        // refuse, so the refusal costs the key nothing and the retry is refused
        // identically.
        //
        // Moving the read ahead of the insert does widen one window, and it
        // widens it conservatively: two processes first-opening the same key at
        // once, where only the peer can route every child. The peer's insert can
        // land after this host's `read_group` returns `None`, so this host
        // refuses where the old ordering would have passed the miss through
        // behind that insert. The refusal is typed and journals nothing, the
        // peer's open is unaffected, and a retry finds the peer's row and
        // reopens — so the cost is one refused open in a race, against a strand
        // that was neither typed nor recoverable.
        //
        // Skipped entirely when this process already runs the group: those
        // children are dispatched and settling, and resolving N executors only
        // to drop them is work whose sole output would be a refusal for a live
        // group. The durable fence below still judges the reopen's shape.
        //
        // The position map is built here for the same reason: a child with no
        // replay key can never be claimed, and that refusal must cost the key
        // nothing either.
        let prepared = if self.groups.get(group.group_key()).is_some() {
            None
        } else {
            let replay_keys = replay_keys_of(&group)?;
            Some((replay_keys, self.resolve_group_children(&group).await?))
        };
        let record = EffectGroupRecord::from_group(
            &group,
            journal_identity.key(),
            journal_identity.session_id().map(str::to_string),
            self.clock.timestamp_ms(),
        );
        let persisted = self.persistence.open_group(&record).await?;
        fence_reopen(&record, &persisted)?;
        let Some((replay_keys, executors)) = prepared else {
            // Already running here. The durable fence above has judged the
            // shape, so there is nothing left to check and nothing to dispatch.
            return Ok(handle);
        };
        if self.groups.get(group.group_key()).is_some() {
            return Ok(handle);
        }

        let dispatched = executors
            .iter()
            .filter(|executor| executor.is_some())
            .count();
        let state = {
            let mut open = self.groups.open.write_recover();
            if open.contains_key(group.group_key()) {
                return Ok(handle);
            }
            let state = Arc::new(OpenGroup {
                // The children this process actually dispatched, not the group's
                // arity: a reopen that has no runner for one child never spawns a
                // task that could report it finished, and counting it here would
                // pin the entry open for the life of the process.
                outstanding: AtomicUsize::new(dispatched),
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

    /// Resolves each child's executor through this host's registered resolver,
    /// before anything of this group is written.
    ///
    /// All of them, not one at a time as each is dispatched: resolving lazily
    /// would journal the group first and discover the gap after, leaving a
    /// recorded group whose missing child permanently owns a rank no settlement
    /// can take. The refusal names the child's position and replay key, because
    /// "some child of this group has no runner" is not something an operator can
    /// act on.
    ///
    /// **What a miss means depends on whether the journal already holds the
    /// group**, and that is a question the journal answers rather than one the
    /// insert's return value can be read for. An *unrecorded* group is a first
    /// open: the miss refuses the whole open, and because nothing has been
    /// written the key is untouched, so a retry under the same unwired
    /// deployment is refused identically instead of being read as a reopen. A
    /// *recorded* group is a deployment that lost a runner, which ADR 0065 makes
    /// the drain's `NoExecutor` case: the `None` is passed through, because
    /// refusing there would deny a resuming caller the ranks the group already
    /// holds.
    ///
    /// The read costs the healthy path nothing — it happens only once a child
    /// has actually missed.
    async fn resolve_group_children(
        &self,
        group: &RuntimeEffectGroup,
    ) -> Result<Vec<Option<RuntimeEffectLocalExecutor<'static>>>, RuntimeEffectControllerError>
    {
        let resolver = self.group_executors()?;
        let mut resolved = Vec::with_capacity(group.children().len());
        let mut missed = None;
        for (position, child) in group.children().iter().enumerate() {
            let executor = resolver.executor_for(child);
            if executor.is_none() && missed.is_none() {
                missed = Some((position, child));
            }
            resolved.push(executor);
        }
        let Some((position, child)) = missed else {
            return Ok(resolved);
        };
        if self
            .persistence
            .read_group(group.group_key())
            .await?
            .is_some()
        {
            return Ok(resolved);
        }
        Err(group_shape_error(format!(
            "child {position} of durable effect group {} names a command this \
             host has no runner for{}, so the group is refused before anything \
             of it is journaled: a recorded group whose child can never settle \
             holds a rank no settlement can take, every rank above it is \
             unservable, and a group row left behind by this refusal would make \
             the next attempt a reopen that strands the caller instead of \
             refusing again",
            group.group_key(),
            child
                .invocation
                .replay_key()
                .map(|key| format!(" (replay key {key})"))
                .unwrap_or_default(),
        )))
    }

    /// Spawns one host-owned task per child this host has a runner for.
    ///
    /// The task set is the host's, not the caller's: a group whose children ran
    /// inside the caller's future would drop its losers the moment the caller
    /// was dropped, which is precisely what `RunToCompletion` forbids.
    ///
    /// A `None` executor reaches here only on a reopen, and the child is left
    /// alone rather than failed: the deployment cannot run it, which is the
    /// drain's `NoExecutor` report and not a terminal this host may synthesize.
    fn dispatch_group_children(
        self: &Arc<Self>,
        scope: &ExecutionScope,
        state: &Arc<OpenGroup>,
        group: RuntimeEffectGroup,
        executors: Vec<Option<RuntimeEffectLocalExecutor<'static>>>,
    ) {
        let group_key = Arc::<str>::from(group.group_key());
        for (child, executor) in group
            .children()
            .iter()
            .cloned()
            .zip(executors)
            .filter_map(|(child, executor)| executor.map(|executor| (child, executor)))
        {
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

    /// Ask retention's question about a group named only by key.
    ///
    /// The drain's entry point into retention: it settles children of groups
    /// this process may never have opened, so it has a key and no state. A group
    /// with no entry here is already retired as far as this process is
    /// concerned, and the call is a no-op.
    pub(super) async fn retire_group_if_complete(&self, group_key: &str) {
        if let Some(state) = self.groups.get(group_key) {
            self.reap_if_complete(group_key, &state).await;
        }
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
    ///
    /// An unwired host refuses here as it refuses an open, with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported):
    /// the capability flag answers for the whole surface, not for `open` alone.
    pub async fn await_next_group_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.group_executors()?;
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
        let outcome = match stored.state {
            EffectRowState::Settled(EffectTerminal::Completed { outcome_json }) => {
                Ok(serde_json::from_str::<RuntimeEffectOutcome>(&outcome_json)
                    .map_err(|err| vocabulary.decode_error(err))?)
            }
            EffectRowState::Settled(EffectTerminal::Failed { error_json }) => Err(
                serde_json::from_str::<RuntimeEffectControllerError>(&error_json)
                    .map_err(|err| vocabulary.decode_error(err))?,
            ),
            EffectRowState::Corrupt(defect) => {
                return Err(vocabulary.error(EffectReplayFailure::CorruptRow, defect.message()));
            }
            // A rank is allocated in the same transaction that writes the
            // terminal, so an otherwise-valid in-progress row is corrupt in
            // this group-specific context and is refused rather than reported
            // as some plausible outcome.
            EffectRowState::InProgress => {
                return Err(vocabulary.error(
                    EffectReplayFailure::CorruptRow,
                    format!(
                        "child `{}` of durable effect group {group_key} holds settlement \
                         rank {} under status `{}`; a rank is allocated only with a \
                         terminal",
                        stored.replay_key,
                        stored.sequence,
                        EffectRowStatus::InProgress.column()
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
    /// Under [`LoserPolicy::Cancel`] the children this process runs are
    /// cancelled, and each writes its cancellation as its own terminal through
    /// its own fence, allocating a rank exactly as any other outcome would.
    /// Children of this group that this process does not run are the drain's
    /// (FIG-1536), which applies the disposition on the group row.
    ///
    /// An unwired host refuses here too, with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// and ahead of the idempotent success below: a host that answers `false` to
    /// the capability flag and `Ok(())` to a close is the incoherence the flag's
    /// law forbids.
    pub async fn close_effect_group(
        &self,
        handle: &EffectGroupHandle,
        requested: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.group_executors()?;
        let Some(state) = self.groups.get(handle.group_key()) else {
            return Ok(());
        };
        let cancelled = {
            let mut inner = state.state.lock_recover();
            // Resolved against the disposition in force rather than the declared
            // one, so narrowing is cumulative: a group closed as `Cancel` cannot
            // be widened back by a second close either.
            let effective = LoserPolicy::resolve_close(inner.effective, requested)?;
            inner.effective = effective;
            inner.closed = true;
            matches!(effective, LoserPolicy::Cancel)
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

pub(super) fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

/// A drain refusal a caller fixes by waiting, not by changing anything.
///
/// Distinct from [`group_shape_error`] on purpose. "This host is still working
/// the group" and "no such group exists" are the same shape of `Err` to a
/// caller that only has a code, and they call for opposite actions: retry
/// shortly, versus never. A retryable condition that only a message grep can
/// identify is a seam a host gets wrong once and then works around.
pub(super) fn drain_deferred_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupDrainDeferred, message)
}

fn closed_group_error(group_key: &str) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "durable effect group {group_key} is closed to its caller; a closed group's \
         remaining children settle under host ownership and the caller may not \
         observe them"
    ))
}
