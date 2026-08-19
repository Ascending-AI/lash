//! The loser drain: the reclamation a closed effect group's journal rows are
//! already a queue for (FIG-1536, ADR 0065 §2b).
//!
//! Layer 2.5 gave the SQL tiers a durable group host and, with it, a named
//! bounded leak: a group closed under
//! [`LoserDisposition::RunToCompletion`](super::group::LoserDisposition::RunToCompletion)
//! releases its caller while children keep running, and children the closing
//! process was *not* running — because it crashed, or because another process
//! claimed them — are left `in_progress` in the journal with nobody driving
//! them. This module is the seam that finishes them.
//!
//! # The queue is the journal
//!
//! Nothing is enqueued. The drain's work list is
//! [`EffectReplayPersistence::read_unsettled_group_children`](super::effect_replay_driver::EffectReplayPersistence::read_unsettled_group_children)
//! — the children of one group that hold no settlement rank, which for a
//! grouped child is the same set as "no terminal", because N1 writes the rank
//! and the terminal in one transaction. No synthetic queued-work item, no
//! `work_kind`, and no table exists for this: a second queue would be a second
//! copy of a fact the effect journal already holds exactly, and the two would
//! disagree the first time one of them was written without the other.
//!
//! # Three guards, in the order they apply
//!
//! **The claim fence is the authority.** A drained child goes through
//! [`execute_effect`](super::effect_replay_driver::EffectReplayDriver::execute_effect)
//! like any other effect, so a child whose terminal is already recorded is
//! replayed, a child under a live lease is waited for and then replayed, and
//! only a child nobody owns is executed. Exactly-once across a crash is not a
//! property this module adds; it is the property the journal already has, and
//! the drain is written so as not to step outside it.
//!
//! **A live lease is a scheduling skip, and so is a busy claim.** A recorded
//! lease that has not expired says some executor owns the child *now*, so the
//! drain reports it and moves on rather than parking a pass on it; a claim that
//! comes back busy says the same thing one moment later, and is reported the
//! same way. Both are hints, judged on the draining host's clock rather than the
//! substrate's, and they are allowed to be hints precisely because the fence
//! above is not: the worst a stale hint can do is make a pass do more or less
//! work, never make a child run twice. What the drain must not do is *wait* on
//! them — a pass that queues behind a live renewer is an unbounded pass, and the
//! rest of its queue is the work it was actually called for.
//!
//! **A group this process is still working is refused outright.** The drain
//! reclaims groups whose caller is gone. A group open here has a caller entitled
//! to its settlements, and a group closed here whose outstanding count has not
//! reached zero still has losers this host is running under
//! `RunToCompletion` — draining either would race the host against itself and,
//! in the second case, let a host steal a child from its own stalled executor.
//!
//! These guards see only what this process can see. A group open in *another*
//! process is not distinguishable from a closed one here, which is why
//! closedness is the caller's knowledge and why
//! [`GroupDrainReport::is_complete`] states that residual rather than implying a
//! durable answer.
//!
//! # What the drain does not decide
//!
//! Not the disposition: it is read from the group row, which is where the
//! opening caller declared it and which a reopen may not restate. A
//! [`Cancel`](super::group::LoserDisposition::Cancel) group's children are
//! cancelled inside their own claims by the process running them, and each
//! journals that cancellation as its own terminal at close; the drain
//! re-executes nothing there and says so, child by child, rather than silently
//! reporting an empty pass.
//!
//! Not which effects this host can run: a journaled envelope names a command,
//! and whether this process can execute that command is the host's own fact.
//! [`GroupExecutors`] is where the host answers, and answering "not mine"
//! is a first-class answer rather than a fabricated outcome. That seam is no
//! longer the drain's alone — see its own documentation.
//!
//! Not *when* to run: the drain is a driver a host calls, not a background
//! sweeper that decides its own cadence. A never-closed group stays the named
//! bounded leak layer 2.5 documented, because no one has told the drain the
//! caller is gone.

use tokio_util::sync::CancellationToken;

use super::envelope::RuntimeEffectEnvelope;
use super::executor::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor};
use super::group::LoserDisposition;

/// How a host says what runs a grouped child, from that child's envelope alone.
///
/// **This is the contract's only executor-resolution seam** (ADR 0065, amended).
/// An envelope names a command; it is not an execution. A `Process` command
/// needs the process runner, a tool attempt needs the host's tool surface, and
/// neither is a fact the effect journal records or a caller could ship across a
/// crash. The host owns those runners, so the host answers — once, wired at the
/// host rather than reached for out of whatever session happens to be in scope,
/// because a resolver assembled from ambient state would run a losing child
/// against a session that is not the one that opened it.
///
/// # Every path resolves here
///
/// [`open_effect_group`](super::RuntimeEffectController::open_effect_group)
/// resolves each child through this seam before journaling the group, a host
/// retrying a child resolves it again through this seam, and the loser drain
/// resolves the children of a group whose caller is gone through this seam. One
/// host therefore has exactly one answer to "what code runs this journaled
/// child", whichever path is asking, which is the property that made the earlier
/// caller-supplied executor vec redundant: three of the four paths already
/// bypassed it, and the fourth accepted it only to drop it.
///
/// This is also where the ratified `'static` property lives. A grouped child
/// must be able to outlive the caller that opened it, because
/// [`LoserDisposition::RunToCompletion`] says a losing promise keeps running;
/// the executors handed out here are `'static`, so
/// [`supports_effect_groups`](super::RuntimeEffectController::supports_effect_groups)
/// may answer `true` exactly where a host has registered one of these and not
/// otherwise.
///
/// # `None` is a routing fact, not an outcome
///
/// Returning `None` is a supported answer and the honest one when this host
/// cannot run the command. It never becomes a settlement: an executor that
/// fabricated an outcome would journal a terminal no effect ever produced. What
/// it becomes depends on who asked. An **open** refuses the whole group with a
/// typed group-shape error before anything is journaled, because a group whose
/// child can never settle is a group whose ranks above that child can never be
/// served. A **drain pass** leaves the child alone and reports
/// [`ChildDrainOutcome::NoExecutor`], because there the group is already
/// journaled and a queue another host can finish is the accurate description.
pub trait GroupExecutors: Send + Sync {
    /// The executor for `envelope`, or `None` when this host cannot run it.
    ///
    /// Called once per child per resolution, with the envelope as the journal
    /// recorded it — or, at open, as the group carries it — including its
    /// [`EffectGroupMembership`](super::group::EffectGroupMembership), so a host
    /// can route on the group as well as on the command.
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>>;
}

/// The host-owned driver that runs a closed group's remaining children to
/// settlement.
///
/// Object-safe and implemented once over the shared effect-replay driver, so
/// both SQL tiers answer the same laws through one type. A host obtains one from
/// its effect host and calls [`drain_group`](Self::drain_group) when it knows a
/// group's caller is gone and this host has nothing left of it — on recovery for
/// a scope whose owner did not return, or after a close it observed *and* whose
/// losers it has seen settle.
///
/// Closing is not by itself the moment to call. A `RunToCompletion` close
/// releases the caller while this host's own executors keep running the losers,
/// and draining then would mean reclaiming a child from a live local executor,
/// so the pass is refused with
/// [`RuntimeEffectGroupDrainDeferred`](crate::RuntimeErrorCode::RuntimeEffectGroupDrainDeferred)
/// — a retryable code, distinct from the shape refusal a group the journal does
/// not hold gets, so a host can tell "come back" from "never" without reading
/// prose.
#[async_trait::async_trait]
pub trait EffectGroupDrain: Send + Sync {
    /// Run one drain pass over `group_key` and report what it did.
    ///
    /// Idempotent by construction: a second pass over a fully drained group
    /// finds no unsettled child and reports an empty pass, and a pass that races
    /// another executor replays that executor's terminal rather than writing a
    /// second one.
    ///
    /// Errors are refusals to *decide*, never partial work reported as success:
    /// an unrecorded group, a group this process still holds open, or a row this
    /// build cannot decode all end the pass rather than being skipped past.
    ///
    /// # The pass is bounded
    ///
    /// A pass runs its children one at a time and a child can run for as long as
    /// the effect it names does, so `cancel` is not optional decoration: it is
    /// how an operator, a shutdown, or a supervising timer takes a pass back.
    /// Cancelling stops at the child in flight and reports it and every child
    /// behind it as [`ChildDrainOutcome::Interrupted`], so the caller can see
    /// exactly how far the pass got. Nothing is lost by stopping — an
    /// interrupted child's claim simply stops being renewed, which is the state
    /// a crashed executor leaves and which the next pass reclaims.
    ///
    /// The token is taken by reference and mirrors
    /// [`await_next_settlement`](super::RuntimeEffectController::await_next_settlement),
    /// the other long-running group call, so a host that already threads a
    /// shutdown token through one threads it through both.
    async fn drain_group(
        &self,
        group_key: &str,
        cancel: &CancellationToken,
    ) -> Result<GroupDrainReport, RuntimeEffectControllerError>;
}

/// What one drain pass did, child by child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupDrainReport {
    /// The group the pass ran over.
    pub group_key: String,
    /// The disposition read from the group row, which the pass applied. Reported
    /// rather than assumed by the caller, because a caller that closed the group
    /// under a *narrowed* disposition still finds the declared one here — the
    /// narrowing is process-local and the journal holds what was declared.
    pub disposition: LoserDisposition,
    /// One entry per child that held no rank when the pass read the journal, in
    /// the order the pass handled them. A group with nothing left to settle
    /// reports none, which is what makes it reclaimable.
    pub children: Vec<DrainedChild>,
}

impl GroupDrainReport {
    /// How many children this pass drove to a terminal.
    #[must_use]
    pub fn settled(&self) -> usize {
        self.children
            .iter()
            .filter(|child| child.outcome == ChildDrainOutcome::Settled)
            .count()
    }

    /// Whether the group held nothing unsettled when the pass read it.
    ///
    /// The retention question, in the form the group host asks it: a *closed*
    /// group with no unsettled child is complete, and completing it is what
    /// retires its process-local state.
    ///
    /// # Conditional on the caller's knowledge that the group is closed
    ///
    /// This is not, and cannot be, a durable statement that the group will
    /// never have another child. Closedness is host knowledge by design —
    /// nothing durable records it, because recording it would mean a schema the
    /// SQLite tier cannot gain without deleting effect databases — and the
    /// drain's guards catch only the two cases this process can see: a group
    /// open to a caller *here*, and a group whose children this process is
    /// still running.
    ///
    /// The residual, stated plainly, in the same spirit as the never-closed-group
    /// leak the group host names: a group still open in **another** process,
    /// whose children have not yet claimed their rows, holds nothing unsettled,
    /// so a pass over it reports no children and this answers `true` for a group
    /// that is very much alive. `true` therefore means "closed **and** complete"
    /// only for a caller that knows the first half; a caller that does not know
    /// it is asking the wrong driver. This is not closed durably, on purpose,
    /// and the boundary is named here rather than papered over with a guard that
    /// would be a guess.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.children.is_empty()
    }
}

/// One child a pass considered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainedChild {
    /// The child's replay key, its durable identity within its scope. Position
    /// is deliberately absent: it is not a journal column, and the drain — which
    /// never held the caller's children — has nothing to map the key back
    /// through.
    pub replay_key: String,
    /// What the pass did with it.
    pub outcome: ChildDrainOutcome,
}

/// What a pass did with one unsettled child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildDrainOutcome {
    /// The pass drove the child through the claim fence to a terminal, so it now
    /// holds a settlement rank like any other child.
    ///
    /// Whether that terminal is a success or a failure is the child's own
    /// outcome and is read back by rank through the group surface, not reported
    /// here: a drain that summarized outcomes would be a second, lossier copy of
    /// the settlement record.
    Settled,
    /// Another executor owns the child, so the pass moved past it.
    ///
    /// Two ways to arrive here, and they are the same fact seen at two moments.
    /// Either the claim came back busy — the lease the pass read as expired had
    /// been taken by the time it reached for it — or the pass attempted the
    /// child and the journal still held no rank for it when the pass re-read,
    /// which is what a finalize fenced out mid-flight looks like from here.
    ///
    /// Distinct from [`LeaseLive`](Self::LeaseLive), which is the same
    /// ownership seen *before* reaching for the claim, and reported separately
    /// because only one of the two carries a lease boundary an operator can
    /// wait on.
    ///
    /// Not a failure of the pass and not a loss of work — the child stays queued
    /// and the next pass, here or on the host that took it, finishes it. It is
    /// reported because the alternative is calling it settled, and a drain that
    /// reports settlements it cannot see is exactly the drain an operator cannot
    /// use to decide whether a group is done.
    Contested,
    /// Left alone: the recorded lease had not expired, so an executor owns the
    /// child now and the pass would only have queued behind it.
    LeaseLive {
        /// The lease boundary the pass read, on the substrate's clock.
        expires_at_ms: u64,
    },
    /// Left alone: the group declared [`LoserDisposition::Cancel`], whose
    /// terminals are synthesized inside each child's own claim by the process
    /// running it. There is nothing for a re-execution to add, and cancelling
    /// from here would be the drain inventing a terminal for a child it never
    /// ran.
    CancelDeclared,
    /// Left alone: [`GroupExecutors`] had no executor for this command, so
    /// this host cannot run it. Reported rather than skipped silently, because a
    /// child no host will run is a queue that never empties and an operator has
    /// to be able to see it.
    NoExecutor,
    /// The pass's cancellation token fired: not reached, or reached and
    /// abandoned, or — narrowly — finished at the same moment. The child in
    /// flight when the token fired and every child behind it report this, so
    /// the report says how far the pass got rather than implying the untouched
    /// tail was examined and found clean.
    ///
    /// Deliberately not a claim about the child's state. Cancellation is
    /// checked first when the two are ready together, so a child whose terminal
    /// had just been written can still land here; the journal is what answers
    /// whether it settled, as it is for every other outcome. Biasing the other
    /// way would only move the ambiguity, since the token can fire one
    /// instruction after the check.
    ///
    /// An interrupted child is not damaged either way. If the pass had claimed
    /// it and did not finish, the claim stops being renewed and expires, which
    /// is the same state a crashed executor leaves and the state this whole
    /// driver exists to reclaim.
    Interrupted,
    /// Left alone: the journal row is torn — it carries a terminal status with
    /// no settlement rank, which the one-transaction pairing behind those two
    /// facts says cannot happen.
    ///
    /// Reported per child rather than failing the pass, because a torn row is
    /// one row and its healthy siblings still drain. The group stays
    /// unreclaimable, which is correct and is the state an operator has to be
    /// able to see: this is corruption, not contention, and no amount of
    /// retrying resolves it.
    Corrupt {
        /// The status the row actually held, so the report names the corruption
        /// rather than merely asserting it.
        status: String,
    },
}
