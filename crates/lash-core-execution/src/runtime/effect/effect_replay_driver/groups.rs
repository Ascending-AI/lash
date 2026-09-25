//! The durable effect-group host every SQL backend gets for free (FIG-1564).
//!
//! Layer 1 gave the substrates the *journal shape* of a group — a group row
//! carrying the counter, a `group_key` and `settlement_seq` on each child, and
//! the N1/N2/N3 rules that keep rank a fact. What it did not give them was a
//! **host**: on sqlite and postgres the three contract methods
//! (`open_effect_group`, `await_next_settlement`, `close_effect_group`) still
//! inherited the fail-closed defaults they carried until FIG-2266, so no
//! production group row was ever written and no child ever carried a
//! `group_key`. This module is that host, written once over
//! [`EffectReplayRowStore`] so both stores delegate to it exactly as they
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
//! # Three mechanisms
//!
//! The contract's observable semantics are pinned by the effect-group
//! conformance laws every engine runs, and three mechanisms carry them here:
//!
//! * **Children run on host-owned tasks.** Dropping the caller must not drop the
//!   losers, because `RunToCompletion` says a losing promise keeps running.
//! * **The rank is allocated at settlement by a single allocator.** Here that is
//!   layer 1's single-row counter bump inside the child's own finalize
//!   transaction (N1).
//! * **A settlement is served from the record, never re-raced.** Rank
//!   `consumed + 1` is a `SELECT`; once decided it re-reads the same child,
//!   which is what makes a replayed frame observe what the pre-crash frame did.
//!
//! # What this layer is not
//!
//! It is not the drain. That shipped beside it as
//! [`group_drain`](crate::runtime::effect::group_drain) (FIG-1536), reading this
//! layer's [`EffectReplayRowStore::read_unsettled_group_children`] and
//! executing what it finds through host-supplied executors — the reclamation
//! this layer's leak note below promises.
//!
//! The split between them is *not* symmetric across dispositions, and the reason
//! is what each side can honestly write:
//!
//! * Under
//!   [`LoserPolicy::Cancel`](super::super::group::LoserPolicy::Cancel),
//!   a close journals the cancel decision itself: every outstanding child is a
//!   loser the moment the group closes, so the close writes the cancelled
//!   terminal and its rank through the membership row's decision CAS — the
//!   same linearization point a child's own final record races (ADR 0099 §4).
//!   The in-process cancellation token still fires, to stop work the decision
//!   has already made unrecordable, and a child whose final beat the decision
//!   keeps its committed terminal. Children of a `Cancel` group whose process
//!   died between open and close are decided by the drain, which writes the
//!   identical terminal from the retained membership — the decision is a
//!   journal write, not a signal a dead process failed to receive.
//! * Under `RunToCompletion` the drain is the executor of last resort, and the
//!   disposition it applies is the one journaled on the group row at open. That
//!   is why the disposition is durable: the drain never invents one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use lash_sansio::sync::{MutexExt, RwLockExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
pub(super) use crate::runtime::effect::group::group_shape_error;
use crate::runtime::effect::group::{
    EffectGroupHandle, EffectGroupRecordAccessor, GroupReopen, GroupSettlement, GroupWakePolicy,
    LoserPolicy, RuntimeEffectGroup, await_cancelled_error, closed_group_error,
    exhausted_group_error, fence_reopen, fence_reopen_content,
};
use crate::runtime::effect::group_closing::GroupOnlyFinalization;

impl EffectGroupRecordAccessor for EffectGroupRecord {
    fn group_key(&self) -> &str {
        &self.group_key
    }

    fn children(&self) -> usize {
        self.expected_children
    }

    fn wake(&self) -> GroupWakePolicy {
        self.wake
    }

    fn loser_disposition(&self) -> LoserPolicy {
        self.loser_disposition
    }
}

/// Every group this driver has open, keyed exactly as ADR 0065 keys them.
///
/// A read-mostly index of per-group states: siblings of *different* groups have
/// no reason to contend, and the state under one key is small and rarely
/// written.
///
/// # Retention, and the one entry that outlives its group
///
/// An entry is removed when the group is closed *and* complete (see
/// `reap_if_complete`). A reopen is a new caller interest: it clears the
/// entry's `closed` flag (the narrowed disposition stays cumulative), and the
/// entry is reaped only after the caller's *next* close once children
/// complete. A group whose caller never closes — a frame that
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
///
/// `pub(super)` fields because the finalizer in [`super::closing`] reads the
/// running set and the task-finished notify — the same visibility the drain
/// already has through [`DurableEffectGroups`]' `pub(super)` helpers.
pub(super) struct OpenGroup {
    /// The child identity at each position, in child order. The journal names
    /// a settled child by `replay_key`; the contract reports it by position,
    /// and this is the map between them — held rather than stored, because the
    /// caller that opened the group is the party that knows it. The canonical
    /// envelope and its hash ride along because a `Cancel` close owes each
    /// undecided child a `decide_cancel`, and the decision's insert path binds
    /// the pair the claim would have written.
    pub(super) children: Vec<OpenGroupChild>,
    /// Fired by a close that resolves to [`LoserPolicy::Cancel`]. Children
    /// take child tokens, so cancelling the group cancels exactly the children
    /// this process is still running.
    cancel: CancellationToken,
    pub(super) state: Mutex<OpenGroupState>,
    /// Woken when one of this process's children journals a settlement — or
    /// returns, since the two share the notify — so a parked caller re-reads
    /// immediately and a finalizer's step-1 wait sees the finished task.
    pub(super) settled: Notify,
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

pub(super) struct OpenGroupState {
    /// Released by the caller's close; a reopen clears it. Pure caller
    /// interest: the durable disposition the group closes under lives on the
    /// row's `lifecycle`, not here (ADR 0099 §7).
    pub(super) closed: bool,
    /// Replay keys of children this process dispatched and has not finished —
    /// the set finalization step 1 waits on: a `RunToCompletion` member of it
    /// is a protected obligation this host owes, a cancel-decided one is
    /// waited on only up to its drain budget.
    pub(super) running: HashSet<String>,
    /// Replay key → the instant its cancel decision committed, recorded where
    /// `decide_cancel` answered `Decided` or `AlreadyDecided`. The drain
    /// budget is measured from this instant, per §7: the decision is the
    /// durable fact and what the budget bounds is waiting on the body.
    pub(super) decided_at: HashMap<String, Instant>,
}

/// One child's durable identity as this process holds it.
pub(super) struct OpenGroupChild {
    replay_key: String,
    /// The canonical envelope `capture` builds over the accepted envelope —
    /// the wire form the child's claim row records, and what a `decide_cancel`
    /// insert writes for a child that was never claimed.
    envelope_json: String,
    /// The hash inside `envelope_json`.
    envelope_hash: String,
    /// The completion key the child parks on when it is a deferrable tool
    /// child, which its cancel decision closes (ADR 0099 §4, W17).
    completion_wait: Option<(crate::ExecutionScope, crate::AwaitEventWaitIdentity)>,
}

impl DurableEffectGroups {
    pub(super) fn get(&self, group_key: &str) -> Option<Arc<OpenGroup>> {
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
        if open.get(group_key).is_some_and(|current| {
            // Re-judge the retirement under the write lock: the caller's
            // checks ran before a read of the journal, and a reopen that
            // cleared `closed` in between — or a child task that was still
            // finishing — must not be retired out from under it.
            Arc::ptr_eq(current, state)
                && current.state.lock_recover().closed
                && current.outstanding.load(Ordering::Acquire) == 0
        }) {
            open.remove(group_key);
        }
    }
}

impl<P: EffectReplayRowStore + 'static, A: AwaitEventBackend + 'static>
    StoreEffectReplayDriver<P, A>
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
    /// [`execute_effect`](StoreEffectReplayDriver::execute_effect), so a child whose
    /// terminal is already recorded replays it, a child under another process's
    /// live lease waits and then replays it, and only a child with an expired
    /// lease or no row at all is executed. A reopen *inside this process* skips
    /// the dispatch entirely, because those children are still running on this
    /// host's tasks and re-running them would double the side effects the first
    /// dispatch is still producing — and it clears the entry's `closed` flag,
    /// because a reopen is a new caller interest entitled to read the ranks a
    /// closed-but-unreaped group kept recording.
    ///
    /// A host with no registered [`GroupExecutors`] resolver refuses here —
    /// as its two sibling methods do, with
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
        group.validate_execution_scope(scope)?;
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
        let prepared = if self.groups.get(group.group_key()).is_some() {
            None
        } else {
            let children = child_identities_of(&group)?;
            Some((children, self.resolve_group_children(&group).await?))
        };
        let record = EffectGroupRecord::from_group(
            &group,
            journal_identity.key(),
            journal_identity.session_id().cloned(),
            self.clock.timestamp_ms(),
        );
        // Retained before the open is acknowledged, in the same transaction as
        // the group row and ahead of it (ADR 0065 N2, ADR 0099 §3): a persisted
        // accepted group may never exist without discoverable complete input.
        let offered = accepted_membership(&group, self.clock.timestamp_ms())?;
        let persisted = self.row_store.open_group(&record, &offered).await?;
        // A content-checked reopen (FIG-3586) is a replay of a recorded
        // command: a shape it no longer matches is that command's divergence,
        // reported under the replay-mismatch code so the run parks rather
        // than failing.
        fence_reopen(&record, &persisted).map_err(|error| {
            if group.reopen() == GroupReopen::RetainedContent {
                crate::runtime::effect::group::as_replay_mismatch(
                    error,
                    self.vocabulary().code(EffectReplayFailure::HashConflict),
                )
            } else {
                error
            }
        })?;
        // A content-checked reopen (FIG-3586) judges the offer against the
        // journal's retained children before anything is dispatched — and
        // before an in-process reopen short-circuits below — so a redrive
        // whose aggregate differs is refused at the group head.
        if group.reopen() == GroupReopen::RetainedContent {
            let retained = reconstruct_group(
                &group,
                self.row_store
                    .read_group_membership(group.group_key())
                    .await?,
                self.vocabulary(),
            )?;
            fence_reopen_content(
                &group,
                retained.children(),
                self.vocabulary().code(EffectReplayFailure::HashConflict),
            )?;
        }
        let (offered_children, offered_executors) = match prepared {
            Some(prepared) => prepared,
            None => {
                // Already running here. The durable fence above has judged the
                // shape, so there is nothing left to check and nothing to
                // dispatch. The reopen is a new caller interest: an entry
                // closed by an earlier caller but not yet reaped opens again —
                // a closed group's settlements keep landing under host
                // ownership precisely so a caller may read them. Only `closed`
                // clears; the narrowed disposition stays cumulative. The clear
                // runs under the map's write lock so a `reap` re-judging the
                // entry under the same lock cannot retire it out from under
                // the new handle.
                let still_open = {
                    let open = self.groups.open.write_recover();
                    if let Some(existing) = open.get(group.group_key()) {
                        existing.state.lock_recover().closed = false;
                        true
                    } else {
                        false
                    }
                };
                if still_open {
                    return Ok(handle);
                }
                // The entry raced a reap and lost: it existed at the earlier
                // check and is gone now. The reopen is still a new caller
                // interest — resolve the children this run skipped and open
                // the group fresh.
                let children = child_identities_of(&group)?;
                (children, self.resolve_group_children(&group).await?)
            }
        };
        {
            let open = self.groups.open.write_recover();
            if let Some(existing) = open.get(group.group_key()) {
                existing.state.lock_recover().closed = false;
                return Ok(handle);
            }
        }

        // Dispatch from the *journal's* membership, never from `group`.
        //
        // One path rather than a branch on "was this a first open or a reopen".
        // On a first open the membership just written is the caller's, so
        // reading it back changes nothing except that what runs is provably
        // what was durably accepted. On a reopen the caller's children are
        // ignored entirely, which is ADR 0099's W1: a successor that never saw
        // the opener's `RuntimeEffectGroup` still dispatches every accepted
        // child, and a caller that re-presents different children cannot
        // substitute them. The branch is also not available: SQLite's
        // `INSERT … ON CONFLICT DO NOTHING` cannot report whether it inserted.
        let retained = self
            .row_store
            .read_group_membership(group.group_key())
            .await?;
        let group = reconstruct_group(&group, retained, self.vocabulary())?;
        // A replay key is the child's durable identity, not its request's:
        // two envelopes can share a key while carrying different recorded
        // authority. Canonical identity — the hash and canonical JSON
        // `child_identities_of` computes — is the evidence the offered runner
        // was bound to the retained request, and the retained row's
        // formatting is not identity. A matching offer keeps its staged
        // runner; anything else is re-resolved through this host's resolver
        // against the *retained* envelope — the same answer the loser drain
        // gets — and a resolver that cannot run that recorded request answers
        // `None`, the drain's `NoExecutor` case.
        let children = child_identities_of(&group)?;
        let mut offered: HashMap<&str, (&OpenGroupChild, RuntimeEffectLocalExecutor<'static>)> =
            offered_children
                .iter()
                .zip(offered_executors)
                .filter_map(|(child, executor)| {
                    executor.map(|executor| (child.replay_key.as_str(), (child, executor)))
                })
                .collect();
        let resolver = self.group_executors()?;
        let executors = group
            .children()
            .iter()
            .zip(children.iter())
            .map(
                |(envelope, retained)| match offered.remove(retained.replay_key.as_str()) {
                    Some((offered, executor))
                        if self.offered_executor_may_run(offered, retained) =>
                    {
                        Some(executor)
                    }
                    _ => resolver.executor_for(envelope),
                },
            )
            .collect::<Vec<_>>();
        // A reopen dispatches by the durable lifecycle's answer, not the
        // caller's. `closing` under `Cancel` dispatches nothing — every
        // undecided child is owed a `decide_cancel`, which finalization step 1
        // issues — and `settled` dispatches nothing because its obligations are
        // discharged and it exists only to serve ranks. `closing` under
        // `RunToCompletion` still dispatches: those accepted children are the
        // protected obligations a recovery is entitled to keep driving.
        let executors = match persisted.lifecycle {
            EffectGroupLifecycle::Live
            | EffectGroupLifecycle::Closing {
                disposition: LoserPolicy::RunToCompletion,
                ..
            } => executors,
            EffectGroupLifecycle::Closing { .. } | EffectGroupLifecycle::Settled { .. } => {
                (0..executors.len()).map(|_| None).collect()
            }
        };
        // The replay keys this process is about to spawn, registered in
        // `running` *before* any task exists so a fast-finishing child cannot
        // remove a key that was never inserted.
        let running: HashSet<String> = group
            .children()
            .iter()
            .zip(executors.iter())
            .filter(|(_, executor)| executor.is_some())
            .map(|(child, _)| child.invocation.replay_key().to_string())
            .collect();
        let dispatched = running.len();
        let state = {
            let mut open = self.groups.open.write_recover();
            if let Some(existing) = open.get(group.group_key()) {
                existing.state.lock_recover().closed = false;
                return Ok(handle);
            }
            let state = Arc::new(OpenGroup {
                // The children this process actually dispatched, not the group's
                // arity: a reopen that has no runner for one child never spawns a
                // task that could report it finished, and counting it here would
                // pin the entry open for the life of the process.
                outstanding: AtomicUsize::new(dispatched),
                children,
                cancel: CancellationToken::new(),
                state: Mutex::new(OpenGroupState {
                    closed: false,
                    running,
                    decided_at: HashMap::new(),
                }),
                settled: Notify::new(),
            });
            open.insert(group.group_key().to_string(), Arc::clone(&state));
            state
        };
        self.dispatch_group_children(scope, &state, group, executors);
        Ok(handle)
    }

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
            .row_store
            .read_group(group.group_key())
            .await?
            .is_some()
        {
            return Ok(resolved);
        }
        Err(group_shape_error(format!(
            "child {position} of durable effect group {} names a command this \
             host has no runner for (replay key {}), so the group is refused before anything \
             of it is journaled: a recorded group whose child can never settle \
             holds a rank no settlement can take, every rank above it is \
             unservable, and a group row left behind by this refusal would make \
             the next attempt a reopen that strands the caller instead of \
            refusing again",
            group.group_key(),
            child.invocation.replay_key(),
        )))
    }

    /// Whether an executor the caller staged for its own offered child may run
    /// the retained child of the same replay key.
    ///
    /// Outside `testing` the only honest answer is canonical identity: the
    /// staged runner is bound to the *offered* envelope's authority, and the
    /// replay key is the child's durable identity rather than its request's —
    /// two envelopes sharing a key is exactly what a reoffering successor
    /// presents. The canonical hash and canonical JSON `child_identities_of`
    /// computes are the evidence; the retained row's formatting is not
    /// identity. The testing build's `KeyOnly` strategy is the leak the
    /// two-opener differential is red-proved against; it is selected per host
    /// through [`StoreEffectReplayDriver::set_offered_child_selection`], never
    /// here.
    #[cfg(not(feature = "testing"))]
    fn offered_executor_may_run(
        &self,
        offered: &OpenGroupChild,
        retained: &OpenGroupChild,
    ) -> bool {
        offered.envelope_hash == retained.envelope_hash
            && offered.envelope_json == retained.envelope_json
    }

    /// The `testing` build reads the strategy installed on this host. See the
    /// non-testing twin for what the honest answer is.
    #[cfg(feature = "testing")]
    fn offered_executor_may_run(
        &self,
        offered: &OpenGroupChild,
        retained: &OpenGroupChild,
    ) -> bool {
        if self.offered_child_selection.load(Ordering::SeqCst)
            == OfferedChildSelection::KeyOnly as usize
        {
            return true;
        }
        offered.envelope_hash == retained.envelope_hash
            && offered.envelope_json == retained.envelope_json
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
            let replay_key = child.invocation.replay_key().to_string();
            // The wait an `AwaitEvent` child parks on is the child's own, and
            // the close that cancels the child releases it (ADR 0099 §12).
            let wait_key = match &child.command {
                RuntimeEffectCommand::AwaitEvent { key } => Some(key.clone()),
                _ => None,
            };
            // The child inherits its opener's process execution permit, as a
            // batch leaf did, so a nested process await releases and
            // reacquires the slot the worker granted this run.
            crate::task::spawn(
                lash_core_ids::execution_permit::inherit_process_execution_permit(async move {
                    // The result is discarded here on purpose: a child's outcome is
                    // reported to its caller through the journal, by rank, and this
                    // task's return value has no other reader. A failure is already
                    // journaled as that child's terminal.
                    let _ = Box::pin(driver.execute_effect_cancellable(
                        &scope,
                        child,
                        executor,
                        Some(&cancel),
                        None,
                    ))
                    .await;
                    // Released here, after the execution returned, however far
                    // it got: a child the close cancelled while parked, and
                    // one it cancelled before it ever claimed — whose
                    // execution replays the recorded cancel terminal and
                    // never parks — both leave a wait nothing else resolves
                    // (FIG-3567). The token fires only on a `Cancel` close; a
                    // wait that already holds its terminal (the child
                    // resolved and committed first) refuses the release, so
                    // this never overwrites a real outcome.
                    if let Some(key) = &wait_key
                        && cancel.is_cancelled()
                    {
                        let _ = driver
                            .await_events
                            .resolve(key, crate::Resolution::Cancelled)
                            .await;
                    }
                    driver
                        .group_child_finished(&group_key, &replay_key, &state)
                        .await;
                }),
            );
        }
    }

    /// Wakes any parked caller and retires the group's process-local state once
    /// the group is both closed and complete.
    ///
    /// Retention is bounded by close **and** completion, for the same reason it
    /// is on the native substrate: a `RunToCompletion` loser keeps settling after the
    /// caller is gone, and its settlement is exactly what this state is here to
    /// let a caller read. Completion is two claims, not one: this process's own
    /// child tasks must have returned, *and* the journal must hold no unsettled
    /// child of the group — a sibling being run by another process is a reason
    /// to keep serving this caller its settlements.
    async fn group_child_finished(
        &self,
        group_key: &str,
        replay_key: &str,
        state: &Arc<OpenGroup>,
    ) {
        {
            // Removed before the notify so a finalizer woken by it reads the
            // task as finished rather than still running.
            let mut inner = state.state.lock_recover();
            inner.running.remove(replay_key);
            inner.decided_at.remove(replay_key);
        }
        state.outstanding.fetch_sub(1, Ordering::AcqRel);
        state.settled.notify_waiters();
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
    ///
    /// `pub(super)` because finalization step 4 (`super::closing`) reaps
    /// through this same guard — a reopen that renewed interest since the
    /// settle was recorded must keep the entry it is serving.
    pub(super) async fn reap_if_complete(&self, group_key: &str, state: &Arc<OpenGroup>) {
        let closed = state.state.lock_recover().closed;
        if !closed || state.outstanding.load(Ordering::Acquire) != 0 {
            return;
        }
        if self
            .row_store
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
        cancel: crate::runtime::TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.group_executors()?;
        let state = self
            .groups
            .get(handle.group_key())
            .ok_or_else(|| closed_group_error(handle.group_key()))?;
        if handle.is_exhausted() {
            return Err(exhausted_group_error(handle));
        }
        // The journal's group wake — shared with every driver over this
        // database — is what turns a settlement committed by *another* host
        // into a wake here; `state.settled` still answers for the
        // process-local signals a commit does not produce (a child task
        // finishing, the group closing).
        let watch = self
            .watch_journal(EffectJournalSubject::Group {
                group_key: handle.group_key(),
            })
            .await;
        // The engine races the rank wait against the turn's cancellation gate
        // itself (FIG-3672 P9), as it races any wait the turn observes: the
        // caller's token is only its execution's own cooperative cancel.
        let turn_stop = self.turn_stop(cancel.observed_scope());
        tokio::pin!(turn_stop);
        let cancel = cancel.cancellation().clone();
        loop {
            // Both enabled *before* the journal read, so a sibling that
            // settles between the read and the park is caught rather than
            // slept through: `notify_waiters` wakes listeners, not arrivals.
            let armed = watch.arm();
            let settled = state.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let closed = state.state.lock_recover().closed;
            if closed {
                return Err(closed_group_error(handle.group_key()));
            }
            let rank = handle.consumed() + 1;
            if let Some(stored) = self
                .row_store
                .read_group_settlement(handle.group_key(), rank)
                .await?
            {
                let settlement = self.decode_settlement(handle.group_key(), &state, stored)?;
                handle.advance()?;
                return Ok(settlement);
            }
            tokio::select! {
                // The execution's own stop: for a rank wait that observes no
                // turn (a process body's), the process drive's. P16
                // (FIG-3673) replaces with a recorded race.
                () = cancel.cancelled() => {
                    return Err(await_cancelled_error(handle.group_key(), rank));
                }
                stop = &mut turn_stop => {
                    stop?;
                    return Err(await_cancelled_error(handle.group_key(), rank));
                }
                _ = armed.park(&*self.clock, None) => {}
                () = &mut settled => {}
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
            .children
            .iter()
            .position(|child| child.replay_key == stored.replay_key)
            .ok_or_else(|| {
                vocabulary.error(
                    EffectReplayFailure::CorruptRow,
                    format!(
                        "durable effect group {group_key} recorded a settlement for \
                         replay key `{}`, which is not one of its {} children",
                        stored.replay_key,
                        state.children.len()
                    ),
                )
            })?;
        let outcome = self.decode_group_terminal(group_key, &stored)?;
        Ok(GroupSettlement {
            position,
            sequence: stored.sequence,
            outcome,
        })
    }

    /// Decode the recorded terminal of a settled group row — the shared half
    /// of [`decode_settlement`](Self::decode_settlement) and
    /// [`read_recorded_group_settlement`](Self::read_recorded_group_settlement),
    /// which differ only in whether the caller needs the declared position.
    fn decode_group_terminal(
        &self,
        group_key: &str,
        stored: &StoredGroupSettlement,
    ) -> Result<
        Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
        RuntimeEffectControllerError,
    > {
        let vocabulary = self.vocabulary();
        let outcome = match &stored.state {
            EffectRowState::Settled(EffectTerminal::Completed { outcome_json }) => {
                Ok(serde_json::from_str::<RuntimeEffectOutcome>(outcome_json)
                    .map_err(|err| vocabulary.decode_error(err))?)
            }
            // A settled `Failed` terminal is the child's recorded failure
            // replaying, not a live fault: stamp it journaled (FIG-3528) so
            // the settlement consumer keeps it on the model-visible surface
            // instead of aborting the enclosing turn.
            EffectRowState::Settled(EffectTerminal::Failed { error_json }) => Err(
                serde_json::from_str::<RuntimeEffectControllerError>(error_json)
                    .map_err(|err| vocabulary.decode_error(err))?
                    .into_journaled(),
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
        Ok(outcome)
    }

    /// Read the group's settlement at `rank` without touching any caller
    /// cursor (ADR 0099 §8): the incorporation prefix record reads the journal
    /// through this seam, so it names the settled child by its replay key and
    /// needs no position map — an opener that never opened the group
    /// in-process still incorporates the recorded prefix.
    pub async fn read_recorded_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<crate::runtime::effect::RankedGroupSettlement>, RuntimeEffectControllerError>
    {
        let rank = usize::try_from(rank).map_err(|_| {
            self.vocabulary().error(
                EffectReplayFailure::CorruptRow,
                format!("durable effect group {group_key} was asked for rank {rank}, which no journal can hold"),
            )
        })?;
        let Some(stored) = self
            .row_store
            .read_group_settlement(group_key, rank)
            .await?
        else {
            return Ok(None);
        };
        let outcome = self.decode_group_terminal(group_key, &stored)?;
        Ok(Some(crate::runtime::effect::RankedGroupSettlement {
            sequence: stored.sequence,
            child_replay_key: stored.replay_key,
            outcome,
        }))
    }

    /// Release the caller's interest in the group, recording the close durably
    /// first (ADR 0099 §7).
    ///
    /// **The lifecycle write precedes everything else.** Before any admission
    /// stops and before any cancel decision is issued, the group row's
    /// `lifecycle` column CASes to
    /// `closing` with the effective disposition — `resolve_close` over the
    /// disposition the row already committed, so a second close narrows further
    /// or repeats, never widens. That ordering is the whole durability
    /// argument: a process that dies in the close window leaves the fact
    /// recorded, and a redriven turn's `resume_closing_groups` finishes what
    /// the window interrupted.
    ///
    /// Under [`LoserPolicy::Cancel`] the close then journals the cancel decision
    /// for every child that has not already committed a final record — the
    /// durable half of the disposition (ADR 0099 §4). The cancelled terminal
    /// and its rank are written through the membership row's decision CAS, so
    /// a child whose final record won first keeps it, a child still running
    /// here is then stopped by the in-process token and refused at its own
    /// finalize with `CancelDecided`, and a child no process is running is
    /// settled identically — the decision does not wait on a live claimant.
    ///
    /// `close` returns without waiting on finalization: releasing caller
    /// interest and finishing the group's obligations are different steps
    /// (§4/§7's split), so the finalizer runs on a host-owned task over
    /// [`GroupOnlyFinalization`](crate::runtime::effect::GroupOnlyFinalization)
    /// — a consumer close owes no opener steps; the turn or process exit that
    /// owns the opener (FIG-3397) supplies the real ones.
    ///
    /// Idempotent: a group this process is not running — because it was already
    /// completed and retired, or because the closing frame is a replay in a
    /// fresh process — closes successfully, since the CAS is a no-op on a
    /// settled row and there is nothing left here for a disposition to decide.
    ///
    /// An unwired host refuses here too, with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// and ahead of the idempotent success below: a host that answers `false` to
    /// the capability flag and `Ok(())` to a close is the incoherence the flag's
    /// law forbids.
    pub async fn close_effect_group(
        self: &Arc<Self>,
        handle: &EffectGroupHandle,
        requested: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.group_executors()?;
        let group_key = handle.group_key();
        let Some(record) = self.row_store.read_group(group_key).await? else {
            // A group the journal does not hold was never opened or is already
            // retired; either way there is nothing left to release.
            return Ok(());
        };
        let recorded = record
            .lifecycle
            .closing_disposition()
            .unwrap_or(record.loser_disposition);
        let effective = LoserPolicy::resolve_close(recorded, requested)?;
        let closing = match record.lifecycle {
            EffectGroupLifecycle::Settled { .. } => return Ok(()),
            EffectGroupLifecycle::Live => EffectGroupLifecycle::closing(effective),
            EffectGroupLifecycle::Closing { finalized, .. } => EffectGroupLifecycle::Closing {
                disposition: effective,
                finalized,
            },
        };
        let lifecycle = self
            .row_store
            .transition_group_lifecycle(
                group_key,
                &[
                    EffectGroupLifecyclePhase::Live,
                    EffectGroupLifecyclePhase::Closing,
                ],
                &closing,
            )
            .await?;
        let EffectGroupLifecycle::Closing {
            disposition: effective,
            ..
        } = lifecycle
        else {
            // The CAS answered `settled`: a racing finalizer finished first.
            return Ok(());
        };

        let cancelled = matches!(effective, LoserPolicy::Cancel);
        if let Some(state) = self.groups.get(group_key) {
            state.state.lock_recover().closed = true;
            if cancelled {
                for (position, child) in state.children.iter().enumerate() {
                    // Position order is also settlement-rank order among children
                    // decided by the same close, which is the only ordering the
                    // contract promises losers.
                    let error_json =
                        serde_json::to_string(&child_cancelled_error(group_key, position))
                            .map_err(|err| self.vocabulary().encode_error(err))?;
                    self.row_store
                        .decide_cancel(&EffectCancelRequest {
                            group_key: group_key.to_string(),
                            replay_key: child.replay_key.clone(),
                            terminal: EffectTerminal::Failed { error_json },
                            envelope_json: child.envelope_json.clone(),
                            envelope_hash: child.envelope_hash.clone(),
                            completion_fence: child
                                .completion_wait
                                .as_ref()
                                .map(|(scope, wait)| {
                                    self.await_events.cancel_decision_fence(scope, wait)
                                })
                                .transpose()?,
                        })
                        .await?;
                    // The drain budget is measured from the decision, not the
                    // close: the decision is the durable fact and what the
                    // budget bounds is waiting on the body that follows it.
                    let mut inner = state.state.lock_recover();
                    if inner.running.contains(&child.replay_key) {
                        inner
                            .decided_at
                            .entry(child.replay_key.clone())
                            .or_insert_with(Instant::now);
                    }
                }
                state.cancel.cancel();
            }
            state.settled.notify_waiters();
            self.reap_if_complete(group_key, &state).await;
        }

        // Releasing caller interest never waits on finalization — but the
        // finalizer is this host's to drive, spawned here so a `close` that
        // returns cannot leave the recorded `closing` unworked. An erroring
        // finalizer leaves `closing` recorded and discoverable; a `Pending`
        // report means an obligation is owed elsewhere and a resume retries.
        let driver = Arc::clone(self);
        let key = group_key.to_string();
        crate::task::spawn(async move {
            match driver
                .finalize_group_record(&key, &GroupOnlyFinalization)
                .await
            {
                Ok(report) => {
                    tracing::debug!(group_key = %key, ?report, "group finalization report")
                }
                Err(error) => tracing::warn!(
                    group_key = %key,
                    %error,
                    "group finalization failed; `closing` remains recorded for a \
                     resume to retry"
                ),
            }
        });
        Ok(())
    }
}

/// The membership a group offers the journal at open.
///
/// The envelope is serialized whole: it is the reconstruction, and copying
/// fields out of it into columns would be the second-copy defect this table
/// exists without (see [`AcceptedGroupChild`]).
fn accepted_membership(
    group: &RuntimeEffectGroup,
    created_at_ms: u64,
) -> Result<Vec<AcceptedGroupChild>, RuntimeEffectControllerError> {
    let _ = created_at_ms;
    group
        .children()
        .iter()
        .enumerate()
        .map(|(position, child)| {
            Ok(AcceptedGroupChild {
                position,
                replay_key: child.invocation.replay_key().to_string(),
                envelope_json: serde_json::to_string(child).map_err(|error| {
                    group_shape_error(format!(
                        "child {position} of durable effect group {} cannot be retained: {error}",
                        group.group_key()
                    ))
                })?,
                command_version: super::super::TOOL_CHILD_REQUEST_VERSION,
            })
        })
        .collect()
}

/// Rebuild the group from what the journal retained.
///
/// `offered` supplies only the group's header — its invocation, key, wake rule
/// and disposition, all of which the durable fence has already judged against
/// the recorded row. Every *child* comes from `retained`.
///
/// A membership that disagrees with the recorded arity is corruption, not
/// contention: the two are written in one transaction, so a group row without
/// its full membership cannot be produced by any interleaving. It is reported
/// rather than repaired.
fn reconstruct_group(
    offered: &RuntimeEffectGroup,
    mut retained: Vec<AcceptedGroupChild>,
    vocabulary: EffectReplayVocabulary,
) -> Result<RuntimeEffectGroup, RuntimeEffectControllerError> {
    if retained.len() != offered.children().len() {
        return Err(vocabulary.error(
            EffectReplayFailure::CorruptRow,
            format!(
                "durable effect group {} records {} children but retained {} accepted \
                 requests; the group row and its membership are written in one \
                 transaction, so the two cannot disagree",
                offered.group_key(),
                offered.children().len(),
                retained.len()
            ),
        ));
    }
    retained.sort_by_key(|child| child.position);
    let children = retained
        .into_iter()
        .map(|child| {
            // The membership row's command version is checked, not assumed:
            // it records which command encoding minted the retained envelope,
            // and a build that cannot read it refuses rather than
            // reconstructs a guess.
            if child.command_version != super::super::TOOL_CHILD_REQUEST_VERSION {
                return Err(vocabulary.error(
                    EffectReplayFailure::CorruptRow,
                    format!(
                        "child `{}` of durable effect group {} was minted under \
                         command version {} but this build reconstructs version \
                         {}; a request that cannot be read is corruption, not \
                         a guess",
                        child.replay_key,
                        offered.group_key(),
                        child.command_version,
                        super::super::TOOL_CHILD_REQUEST_VERSION,
                    ),
                ));
            }
            serde_json::from_str::<RuntimeEffectEnvelope>(&child.envelope_json)
                .map_err(|error| vocabulary.decode_error(error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    RuntimeEffectGroup::try_new(
        offered.invocation().clone(),
        offered.group_key(),
        children,
        offered.wake(),
        offered.loser_disposition(),
    )
}

/// The durable identity of each child, in position order.
///
/// A child without a replay key could never be claimed, so a group containing
/// one is refused at open rather than at the moment its rank fails to resolve.
///
/// Distinctness is not rechecked here: [`RuntimeEffectGroup::try_new`] is the
/// sole constructor and refuses two children sharing a replay key, so the
/// position map this builds is one entry per journaled child by construction.
fn child_identities_of(
    group: &RuntimeEffectGroup,
) -> Result<Vec<OpenGroupChild>, RuntimeEffectControllerError> {
    group
        .children()
        .iter()
        .map(|child| {
            let canonical = CanonicalRuntimeEffectEnvelope::capture(child)?;
            Ok(OpenGroupChild {
                replay_key: child.invocation.replay_key().to_string(),
                envelope_json: serde_json::to_string(&canonical).map_err(|err| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectEnvelopeHash,
                        format!("failed to serialize canonical effect envelope: {err}"),
                    )
                })?,
                envelope_hash: canonical.hash().to_string(),
                completion_wait: child.command.group_child_completion_wait(),
            })
        })
        .collect()
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
