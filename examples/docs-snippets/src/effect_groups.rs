//! Durable effect groups: driving one, and implementing the contract in a host.
//!
//! A *group* is the composition above attempts: a structured set of
//! independently journaled children, a wake rule recorded with the group's
//! identity, and a settlement order that is a durable fact rather than whatever
//! order the scheduler happened to finish things in. Hosts meet it from two
//! sides, and this module covers both.
//!
//! **Driving one** — a caller builds a [`RuntimeEffectGroup`], opens it,
//! consumes settlements by rank, and closes it with the disposition its losers
//! should be subject to. A group carries envelopes and nothing else: what code
//! runs a child is the [`GroupExecutors`] resolver its host was registered with,
//! because three of the four paths that need a child's runner — a retry after a
//! crash, the loser drain, a resuming process reopening the group — happen where
//! no caller is in scope. The rest is checked by a type: a handle refuses a
//! cursor past its own child count, and a close may only narrow the disposition
//! the group declared.
//!
//! **Implementing it** — an effect host overrides
//! [`supports_effect_groups`](lash::runtime::RuntimeEffectController::supports_effect_groups)
//! and the three group methods. The three default to a named refusal rather than
//! to a stub, so a host that has not implemented them fails closed instead of
//! mis-executing a batch, and the flag and the methods are one question.
//!
//! What the *inline* tier claims here is worth stating, because a reader should
//! not take more from it than it offers: it implements every observable
//! semantic — wake, rank order, loser completion, cancellation terminals — in
//! memory, for the life of the runtime. Cross-process settlement durability is
//! the SQL and engine tiers' half, and no flag in this module claims it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash::CancellationToken;
use lash::runtime::{
    EffectGroupHandle, EffectGroupMembership, GroupExecutors, GroupSettlement, GroupWakePolicy,
    LoserDisposition, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeErrorCode, RuntimeInvocation, RuntimeScope,
};
use tokio::sync::oneshot;

const SESSION: &str = "docs-effect-groups";

/// One child of a group: an ordinary effect envelope with an ordinary command.
///
/// Groups introduce no new command variant, because what is new is the
/// composition above attempts and not the attempts themselves.
fn child(group_key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new(SESSION),
            "effect",
            RuntimeEffectKind::Sleep,
            format!("{group_key}:child:{position}"),
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 0 },
    )
}

/// Builds a two-arm group.
///
/// The key carries an occurrence ordinal (`:0` here) because the rest of it is a
/// content hash: two textually identical `race` calls in one iteration hash the
/// same, and two groups sharing one key would share one settlement counter.
fn two_arm_group(
    group_key: &str,
    wake: GroupWakePolicy,
    losers: LoserDisposition,
) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            RuntimeScope::new(SESSION),
            "group",
            RuntimeEffectKind::Sleep,
            format!("{group_key}:group"),
        ),
        group_key,
        vec![child(group_key, 0), child(group_key, 1)],
        wake,
        losers,
    )
    .expect("a group with at least one child assembles")
}

/// An executor that settles as soon as it runs.
fn settles_now() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async { Ok(RuntimeEffectOutcome::Sleep) })
}

/// An executor held on an explicit gate, with a counter that records whether it
/// ever reached its end. Two facts a group test needs: when a child settles, and
/// whether a loser kept running after its caller walked away.
fn gated() -> (
    RuntimeEffectLocalExecutor<'static>,
    oneshot::Sender<()>,
    Arc<AtomicUsize>,
) {
    let (release, released) = oneshot::channel();
    let completions = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&completions);
    let executor = RuntimeEffectLocalExecutor::testing(move |_| async move {
        if released.await.is_ok() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        Ok(RuntimeEffectOutcome::Sleep)
    });
    (executor, release, completions)
}

/// A host's answer to "what code runs this journaled grouped child".
///
/// This is the contract's only executor-resolution seam: the open of a group,
/// a retry, and the loser drain all reach for a child's runner here. A real
/// deployment maps the command onto the runners it was deployed with; an example
/// stages the executors it is about to assert on, keyed by the child's replay
/// key, which is the identity the journal knows a child by.
#[derive(Default)]
struct DocsGroupExecutors {
    staged: std::sync::Mutex<HashMap<String, RuntimeEffectLocalExecutor<'static>>>,
}

impl DocsGroupExecutors {
    /// Stages one executor per child and hands back the group to open.
    fn stage(
        &self,
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    ) -> RuntimeEffectGroup {
        let mut staged = self.staged.lock().expect("staged executors");
        for (child, executor) in group.children().iter().zip(executors) {
            let replay_key = child
                .invocation
                .replay_key()
                .expect("a group child carries its replay key")
                .to_string();
            staged.insert(replay_key, executor);
        }
        group
    }
}

impl GroupExecutors for DocsGroupExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope.invocation.replay_key()?;
        self.staged
            .lock()
            .expect("staged executors")
            .remove(replay_key)
    }
}

/// An inline controller and the resolver it was registered with.
///
/// Registration is what makes the controller support groups at all: a host with
/// no resolver has nowhere to get the `'static` executors a child needs in order
/// to outlive its caller, so it answers `supports_effect_groups` `false` and
/// refuses every group method with `EffectGroupUnsupported` rather than
/// journaling a group nothing can run.
fn inline_host() -> (
    lash::runtime::InlineRuntimeEffectController,
    Arc<DocsGroupExecutors>,
) {
    let executors = Arc::new(DocsGroupExecutors::default());
    let controller = lash::runtime::InlineRuntimeEffectController::default();
    controller
        .register_group_executors(Arc::clone(&executors) as Arc<dyn GroupExecutors>)
        .expect("a fresh controller has no resolver yet");
    (controller, executors)
}

/// Spins until a host-owned task has reached the state under assertion, so an
/// example never encodes how many yields a settlement happens to take.
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}

/// Assembling a group makes its key, wake rule, disposition, and child positions
/// agree by construction.
///
/// Children arrive unstamped and leave carrying the group's identity, so a host
/// never fishes a group's key out of `children[0]` and a child can never hash a
/// wake rule the group does not record.
#[tokio::test]
async fn a_group_stamps_its_children_with_the_identity_they_belong_to() {
    let group = two_arm_group(
        "docs:race:0",
        GroupWakePolicy::First,
        LoserDisposition::RunToCompletion,
    );

    assert_eq!(group.group_key(), "docs:race:0");
    assert_eq!(group.wake(), GroupWakePolicy::First);
    assert_eq!(group.loser_disposition(), LoserDisposition::RunToCompletion);
    assert_eq!(group.children().len(), 2);
    // The group's own invocation is its durable identity; the children derive
    // their replay keys beside it.
    assert_eq!(group.invocation().replay_key(), Some("docs:race:0:group"));

    let second: &EffectGroupMembership = group.children()[1]
        .group
        .as_deref()
        .expect("try_new stamps every child with its membership");
    assert_eq!(second.group_key, "docs:race:0");
    assert_eq!(second.position, 1, "a child's position is its index");
    assert_eq!(second.wake, GroupWakePolicy::First);
    assert_eq!(second.loser_disposition, LoserDisposition::RunToCompletion);

    // Stamping by hand is checked rather than trusted: a child that claims a
    // position it does not hold is refused, because settlement rank assumes
    // position equals index.
    let mispositioned = child("docs:race:0", 0).in_effect_group(
        "docs:race:0",
        7,
        GroupWakePolicy::First,
        LoserDisposition::RunToCompletion,
    );
    let refused = RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            RuntimeScope::new(SESSION),
            "group",
            RuntimeEffectKind::Sleep,
            "docs:race:0:group",
        ),
        "docs:race:0",
        vec![mispositioned],
        GroupWakePolicy::First,
        LoserDisposition::RunToCompletion,
    )
    .expect_err("a child that disagrees with its group is refused");
    assert_eq!(refused.code, RuntimeErrorCode::RuntimeEffectGroupShape);

    // An ungrouped effect carries no membership at all, which is what keeps its
    // canonical encoding — and therefore its recorded envelope hash — identical
    // to what it was before groups existed.
    assert!(child("docs:race:0", 0).group.is_none());

    // Every wake rule and disposition a caller can declare is recorded on the
    // group, because both are journaled identity rather than local intent.
    let all = two_arm_group("docs:all:1", GroupWakePolicy::All, LoserDisposition::Cancel);
    assert_eq!(all.wake(), GroupWakePolicy::All);
    assert_eq!(all.loser_disposition(), LoserDisposition::Cancel);

    // Where a child's runner comes from is not part of the group: a group is
    // envelopes. A host with no registered resolver does not do groups at all —
    // it answers the capability flag `false` and refuses the whole surface with
    // the capability code, which is the same answer deployment validation gets
    // before anything is opened. A *child* this host cannot route is the other
    // fact and carries the routing refusal instead.
    let unwired = lash::runtime::InlineRuntimeEffectController::default();
    assert!(!unwired.supports_effect_groups());
    let unwired_refusal = unwired
        .open_effect_group(all)
        .await
        .expect_err("a host with no resolver does not implement groups");
    assert_eq!(
        unwired_refusal.code,
        RuntimeErrorCode::EffectGroupUnsupported
    );
}

/// The `race` shape: open two arms, resume on the first settlement, leave the
/// loser running.
///
/// This is the whole point of the contract in one function. The caller resumes
/// while the second arm is still gated, and the settlement it resumed on names a
/// position and a durable sequence rather than "whichever future woke us".
#[tokio::test]
async fn a_race_resumes_on_the_first_settlement_and_leaves_the_loser_running() {
    let (controller, executors) = inline_host();
    assert!(
        controller.supports_effect_groups(),
        "a host must answer this before a group is opened against it; it gates \
         admission at deployment, not dispatch per call"
    );

    let group_key = "docs:race:1";
    let (slow, release_loser, loser_completions) = gated();
    let group = two_arm_group(
        group_key,
        GroupWakePolicy::First,
        LoserDisposition::RunToCompletion,
    );
    let mut handle = controller
        .open_effect_group(executors.stage(group, vec![slow, settles_now()]))
        .await
        .expect("open returns once the group is recorded, not when a child settles");
    assert_eq!(handle.group_key(), group_key);
    assert_eq!(handle.children(), 2);
    assert_eq!(handle.consumed(), 0);
    assert!(!handle.is_exhausted());

    let settlement: GroupSettlement = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(
        settlement.position, 1,
        "the arm that settled first is delivered, whatever its input position"
    );
    assert_eq!(settlement.sequence, 1, "the winner holds the first rank");
    assert!(settlement.outcome.is_ok());
    assert_eq!(
        handle.consumed(),
        1,
        "the handle is the cursor of record and advances on exactly the \
         settlements it delivered"
    );
    assert_eq!(
        loser_completions.load(Ordering::SeqCst),
        0,
        "the caller resumed on the winner, so the loser cannot have finished"
    );

    // Releasing the caller's interest under `RunToCompletion` leaves the loser
    // running under host ownership: a losing promise's side effects still
    // happen, exactly as they would in Node.
    controller
        .close_effect_group(handle, LoserDisposition::RunToCompletion)
        .await
        .expect("close releases the caller's interest");
    release_loser.send(()).expect("release the losing arm");
    until(|| loser_completions.load(Ordering::SeqCst) == 1).await;
}

/// Settlement `n` is a fact, so a frame restored from a durable continuation
/// re-reads the same child at it.
///
/// The restored handle is the path a parked VM frame actually arrives on: the
/// host knows how many children settled, and only the caller knows how many it
/// consumed, so the caller's cursor is the one that wins.
#[tokio::test]
async fn a_restored_cursor_re_reads_the_same_settlement() {
    let (controller, executors) = inline_host();
    let group_key = "docs:all:0";
    let group = two_arm_group(
        group_key,
        GroupWakePolicy::All,
        LoserDisposition::RunToCompletion,
    );
    let mut handle = controller
        .open_effect_group(executors.stage(group, vec![settles_now(), settles_now()]))
        .await
        .expect("the group opens");
    let first = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("rank 1 arrives");

    let mut restored =
        EffectGroupHandle::restored(group_key, 2, 0).expect("a saved cursor restores");
    let replayed = controller
        .await_next_settlement(&mut restored, CancellationToken::new())
        .await
        .expect("the replayed frame reads rank 1 again");
    assert_eq!(
        (replayed.position, replayed.sequence),
        (first.position, first.sequence),
        "a decided rank is re-read, never re-raced"
    );

    // A cursor exactly at the child count is the legitimate exhausted state. One
    // past it is a corrupt continuation, and reading that as an exhausted group
    // would silently drop every settlement still owed.
    assert!(EffectGroupHandle::restored(group_key, 2, 2).is_ok());
    let corrupt = EffectGroupHandle::restored(group_key, 2, 3)
        .expect_err("a cursor past the child count is refused");
    assert_eq!(corrupt.code, RuntimeErrorCode::RuntimeEffectGroupShape);

    // Consuming the rest is the caller's own arithmetic: `is_exhausted` says
    // when there is no rank left to ask for.
    while !handle.is_exhausted() {
        controller
            .await_next_settlement(&mut handle, CancellationToken::new())
            .await
            .expect("every child of an `all` group settles");
    }
    assert_eq!(handle.consumed(), 2);
    let past_the_end = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect_err("there is no rank 3 on a two-child group");
    assert_eq!(
        past_the_end.code,
        RuntimeErrorCode::RuntimeEffectGroupShape,
        "awaiting past the last child is refused rather than hanging"
    );
}

/// A cancelled await delivers nothing and costs nothing: the rank it did not
/// take is still the next one served.
#[tokio::test]
async fn a_cancelled_await_leaves_the_rank_owed() {
    let (controller, executors) = inline_host();
    let group_key = "docs:await-cancel:0";
    let (slow, release, _) = gated();
    let group = two_arm_group(
        group_key,
        GroupWakePolicy::First,
        LoserDisposition::RunToCompletion,
    );
    let mut handle = controller
        .open_effect_group(executors.stage(group, vec![slow, settles_now()]))
        .await
        .expect("the group opens");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = controller
        .await_next_settlement(&mut handle, cancel)
        .await
        .expect_err("a cancelled await does not deliver a settlement");
    // Cancellation is its own named outcome, distinct from a group-shape
    // refusal, because the caller may simply ask again.
    let code = cancelled.code;
    assert_eq!(code, RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled);
    assert_eq!(handle.consumed(), 0);

    let settlement = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the settlement the cancelled await did not take is still owed");
    assert_eq!(settlement.sequence, 1);
    release.send(()).ok();
}

/// The deadline shape: a group whose losers are cancelled, and whose
/// cancellation is each loser's terminal rather than a dropped future.
///
/// Declaring `Cancel` at *open* is what makes this survive a crash: a group
/// abandoned between its open and its close is drained under the disposition its
/// caller declared, rather than under one the drain path invented.
#[tokio::test]
async fn a_deadline_arm_cancels_its_losers_and_journals_the_cancellation() {
    let (controller, executors) = inline_host();
    let group_key = "docs:deadline:0";
    let (slow, release, loser_completions) = gated();
    let group = two_arm_group(group_key, GroupWakePolicy::First, LoserDisposition::Cancel);
    let mut handle = controller
        .open_effect_group(executors.stage(group, vec![slow, settles_now()]))
        .await
        .expect("the group opens");
    controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the deadline arm settles first");

    controller
        .close_effect_group(handle, LoserDisposition::Cancel)
        .await
        .expect("closing under the declared disposition cancels the losers");
    release.send(()).ok();
    tokio::task::yield_now().await;
    // A cancelled child does not go on to complete: its cancellation is its
    // terminal, which is the fact a durable host journals for it.
    assert_eq!(loser_completions.load(Ordering::SeqCst), 0);

    // Close may narrow — `RunToCompletion` to `Cancel` — and may never widen,
    // because a crash-drain of the same group applies the declared disposition
    // and the losers' fate must not depend on whether the close was reached.
    let run = LoserDisposition::RunToCompletion;
    let cancel = LoserDisposition::Cancel;
    assert!(LoserDisposition::resolve_close(run, cancel).is_ok());
    assert!(LoserDisposition::resolve_close(cancel, run).is_err());
    let widened = LoserDisposition::resolve_close(cancel, run)
        .expect_err("a declared Cancel may not be closed as RunToCompletion");
    assert_eq!(widened.code, RuntimeErrorCode::RuntimeEffectGroupShape);
}

/// `any` accumulates failures, which works because the host filters nothing: a
/// failed settlement is delivered at its rank with its own error.
#[tokio::test]
async fn a_first_success_group_still_reports_the_failures_before_it() {
    let (controller, executors) = inline_host();
    let group_key = "docs:any:0";
    let (slow, release, _) = gated();
    let rejects = RuntimeEffectLocalExecutor::testing(|_| async {
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
            "this arm rejects",
        ))
    });
    let group = two_arm_group(
        group_key,
        GroupWakePolicy::FirstSuccess,
        LoserDisposition::RunToCompletion,
    );
    assert_eq!(group.wake(), GroupWakePolicy::FirstSuccess);
    let mut handle = controller
        .open_effect_group(executors.stage(group, vec![rejects, slow]))
        .await
        .expect("the group opens");

    let first = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the rejecting arm settles first");
    assert_eq!(first.position, 0);
    let error = first
        .outcome
        .expect_err("a rejection is delivered, not skipped");
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
        "the wake rule is journaled identity, so `any` accumulates its own \
         failures rather than asking the host to hide them"
    );

    release.send(()).expect("release the succeeding arm");
    let second = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the succeeding arm settles at rank 2");
    assert!(second.outcome.is_ok());
    assert_eq!(second.sequence, 2, "sequences are monotonic in rank order");
}

// =============================================================================
// Implementing the contract in a host
// =============================================================================

/// A host that has not implemented groups.
///
/// The three group methods are defaulted, and the defaults refuse with a named
/// code rather than doing something plausible. That is deliberate: a host
/// deployed against a dialect that lowers `Promise.all` onto groups should learn
/// so at admission, not by mis-executing a batch.
#[derive(Default)]
struct HostWithoutGroups;

#[async_trait::async_trait]
impl lash::runtime::AwaitEventResolver for HostWithoutGroups {}

#[async_trait::async_trait]
impl RuntimeEffectController for HostWithoutGroups {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        local_executor.execute(envelope).await
    }
}

/// A minimal host implementation of the contract: run every child, record each
/// settlement as it lands, and serve them by rank.
///
/// It is not durable — it journals nothing and dies with the process — but it is
/// the shape a durable host implements, and the two obligations that make it
/// conformant are visible in twenty lines: one allocation point for the
/// sequence, and a rank read that consults the record before it waits.
struct MinimalGroupHost {
    /// This host's one answer to what runs a journaled grouped child, held
    /// because the paths that need it run where no caller is in scope.
    executors: Arc<dyn GroupExecutors>,
    settled: std::sync::Mutex<Vec<GroupSettlement>>,
    declared: std::sync::Mutex<Option<LoserDisposition>>,
}

impl MinimalGroupHost {
    fn new(executors: Arc<dyn GroupExecutors>) -> Self {
        Self {
            executors,
            settled: std::sync::Mutex::new(Vec::new()),
            declared: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl lash::runtime::AwaitEventResolver for MinimalGroupHost {}

#[async_trait::async_trait]
impl RuntimeEffectController for MinimalGroupHost {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        local_executor.execute(envelope).await
    }

    fn supports_effect_groups(&self) -> bool {
        true
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        // Every child is resolved before anything is recorded: a group whose
        // children this host cannot all run is refused whole, rather than
        // half-opened around a child nothing will ever settle.
        let mut executors = Vec::with_capacity(group.children().len());
        for child in group.children() {
            let Some(executor) = self.executors.executor_for(child) else {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupShape,
                    "this host has no runner for a child of this group",
                ));
            };
            executors.push(executor);
        }
        *self.declared.lock().expect("declared disposition") = Some(group.loser_disposition());
        for (position, (child, executor)) in
            group.children().iter().cloned().zip(executors).enumerate()
        {
            let outcome = executor.execute(child).await;
            let mut settled = self.settled.lock().expect("settlement record");
            // One allocation point, taken at the moment the child settles and
            // never as a max over the siblings already recorded: sequences must
            // be unique within the group even when two children settle at once.
            let sequence = settled.len() as u64 + 1;
            settled.push(GroupSettlement {
                position,
                sequence,
                outcome,
            });
        }
        Ok(EffectGroupHandle::new(&group))
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        _cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let settlement = {
            let settled = self.settled.lock().expect("settlement record");
            // Served by rank — the `(consumed + 1)`-th settlement — rather than
            // by taking the next unread entry, which is what makes consumption
            // exactly-once across a restart.
            let recorded = &settled[handle.consumed()];
            GroupSettlement {
                position: recorded.position,
                sequence: recorded.sequence,
                outcome: recorded.outcome.clone(),
            }
        };
        // Advance on exactly the settlement returned; a failed or cancelled
        // await must leave the cursor where it was.
        handle.advance()?;
        Ok(settlement)
    }

    async fn close_effect_group(
        &self,
        _handle: EffectGroupHandle,
        disposition: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        let declared = self
            .declared
            .lock()
            .expect("declared disposition")
            .unwrap_or(disposition);
        LoserDisposition::resolve_close(declared, disposition)?;
        Ok(())
    }
}

/// The two sides of the capability flag, asserted against a host of each kind.
#[tokio::test]
async fn a_host_either_implements_groups_or_refuses_them_by_name() {
    let executors = Arc::new(DocsGroupExecutors::default());
    let refusing = HostWithoutGroups;
    assert!(
        !refusing.supports_effect_groups(),
        "the flag defaults to false, so a host that has not implemented groups \
         is refused at admission"
    );
    let group = two_arm_group(
        "docs:unsupported:0",
        GroupWakePolicy::All,
        LoserDisposition::RunToCompletion,
    );
    let mut handle = EffectGroupHandle::new(&group);
    let refused = refusing
        .open_effect_group(executors.stage(group, vec![settles_now(), settles_now()]))
        .await
        .expect_err("a host without groups fails closed");
    assert_eq!(
        refused.code,
        RuntimeErrorCode::EffectGroupUnsupported,
        "the refusal names the missing capability rather than failing as a \
         generic effect error"
    );
    let refused_await = refusing
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect_err("every group method fails closed, not just open");
    assert_eq!(refused_await.code, RuntimeErrorCode::EffectGroupUnsupported);
    assert_eq!(handle.consumed(), 0);
    let refused_close = refusing
        .close_effect_group(handle, LoserDisposition::RunToCompletion)
        .await
        .expect_err("closing on a host without groups fails closed too");
    assert_eq!(refused_close.code, RuntimeErrorCode::EffectGroupUnsupported);

    let implementing = MinimalGroupHost::new(Arc::clone(&executors) as Arc<dyn GroupExecutors>);
    assert!(implementing.supports_effect_groups());
    let group = two_arm_group(
        "docs:minimal:0",
        GroupWakePolicy::All,
        LoserDisposition::RunToCompletion,
    );
    let mut handle = implementing
        .open_effect_group(executors.stage(group, vec![settles_now(), settles_now()]))
        .await
        .expect("the minimal host opens the group");
    let first = implementing
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("rank 1");
    let second = implementing
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("rank 2");
    assert_eq!(
        (first.sequence, second.sequence),
        (1, 2),
        "a conformant host allocates one sequence per child, monotonic within \
         the group"
    );
    assert!(handle.is_exhausted());
    implementing
        .close_effect_group(handle, LoserDisposition::RunToCompletion)
        .await
        .expect("the minimal host closes under its declared disposition");
}
