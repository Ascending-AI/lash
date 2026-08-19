//! The durable effect-group contract, as a suite every host answers the same
//! way (FIG-1564, ADR 0065).
//!
//! FIG-1535 wrote these semantics against the in-memory reference host, in
//! lash-core's own tests, where they could reach that host's internals. This
//! module is the same matrix expressed through the *contract surface alone* —
//! `EffectHost::scoped`, the four `RuntimeEffectController` group methods, and
//! the `GroupExecutors` resolver the host is registered with — so the inline
//! tier and the two SQL tiers can be held to one set of laws instead of three
//! copies that drift.
//!
//! # What is asserted here and what is not
//!
//! Every law below is observable through the contract on any host. The claims
//! that are *durable* rather than observable — a child row carrying its
//! `group_key`, the unsettled-children read being the exact complement of the
//! rank read — are asserted in each store's own effect-replay tests, against
//! the journal, because that is where the fact lives. A suite that tried to
//! assert them through the contract would be asserting them against the inline
//! tier too, which journals nothing and is honest about it. The one durable law
//! that *is* expressible through the contract, because a second host instance
//! can be asked to read it back, lives here as
//! [`effect_group_cancelled_child_terminal_is_durable`] — run by the stores,
//! not by every host.
//!
//! Each law takes its own host from the factory and namespaces its own group
//! keys, so the suite is safe against a shared substrate.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    EffectGroupHandle, GroupExecutors, GroupSettlement, GroupWakePolicy, LoserDisposition,
    RuntimeEffectGroup,
};

/// A caller's whole interaction with one group: open, await, close.
type Host = Arc<dyn EffectHost>;

/// Run the durable effect-group host suite.
///
/// `make` is handed the suite's [`GroupExecutors`] resolver and must register it
/// on the host it builds; a host built without it cannot run a child, and the
/// first law says so. It returns a host object over **one** substrate: two calls must reach the
/// same journal, because a second host is how a law says "the process that
/// resumes this continuation". On a store-backed tier that is a second host over
/// the same database; on the inline tier, whose substrate is the process, it is
/// the same host object handed out again. Laws namespace their own group keys
/// and scopes, so sharing costs them nothing.
pub async fn effect_group_host_conformance<F>(make: F)
where
    F: Fn(Arc<dyn GroupExecutors>) -> Host,
{
    let make = || make(suite_executors() as Arc<dyn GroupExecutors>);
    let prefix = format!("group-conformance-{}", uuid::Uuid::new_v4().simple());
    the_capability_flag_and_the_group_surface_agree(&make, &prefix).await;
    duplicate_replay_keys_are_refused_before_a_host_sees_them(&make, &prefix).await;
    the_first_settlement_wakes_the_caller_while_the_loser_still_runs(&make, &prefix).await;
    settlement_n_is_stable_across_re_reads(&make, &prefix).await;
    every_child_is_delivered_once_in_rank_order(&make, &prefix).await;
    awaiting_past_the_last_child_is_refused(&make, &prefix).await;
    a_cancelled_await_leaves_the_rank_to_be_read_again(&make, &prefix).await;
    run_to_completion_losers_settle_after_the_caller_is_gone(&make, &prefix).await;
    cancel_stops_the_losers_and_closes_the_caller_out(&make, &prefix).await;
    a_close_may_narrow_but_never_widen(&make, &prefix).await;
    closing_twice_under_one_disposition_succeeds(&make, &prefix).await;
    a_reopen_is_fenced_on_shape_and_runs_no_child_twice(&make, &prefix).await;
    a_second_host_instance_reads_the_ranks_the_first_recorded(&make, &prefix).await;
}

/// The durable-tier law: a cancelled child's terminal is a *journaled* fact.
///
/// Separate from [`effect_group_host_conformance`] because it is not answerable
/// by every host. The inline tier journals nothing, so "the cancellation
/// survived the host that issued it" has no meaning there; a durable tier owes
/// it, and owes it in the only way that proves it — a second host instance,
/// carrying none of the first's memory, reading the terminal back at its rank.
///
/// Run it from each store's own tests, alongside that store's journal
/// assertions.
pub async fn effect_group_cancelled_child_terminal_is_durable<F>(make: F)
where
    F: Fn(Arc<dyn GroupExecutors>) -> Host,
{
    let make = || make(suite_executors() as Arc<dyn GroupExecutors>);
    let prefix = format!("group-cancel-terminal-{}", uuid::Uuid::new_v4().simple());
    let host = make();
    let scoped = host
        .scoped(scope(&prefix, "cancel-terminal"))
        .expect("a scope binds");
    let key = group_key(&prefix, "cancel-terminal");
    let handle = open(
        &scoped,
        &key,
        1,
        GroupWakePolicy::First,
        LoserDisposition::Cancel,
        vec![never()],
    )
    .await;
    close(&scoped, handle, LoserDisposition::Cancel)
        .await
        .expect("the caller closes under the declared disposition");

    // The reader has no memory of the cancellation — it was not running when the
    // close happened — so anything it reads at rank 1 came out of the journal.
    let reader = make();
    let resumed = reader
        .scoped(scope(&prefix, "cancel-terminal"))
        .expect("a scope binds on the reading host");
    let mut reopened = resumed
        .controller()
        .open_effect_group(staged(
            group(&key, 1, GroupWakePolicy::First, LoserDisposition::Cancel),
            vec![never()],
        ))
        .await
        .expect("the reading host reopens the group");
    let settlement = next(&resumed, &mut reopened)
        .await
        .expect("the cancelled child holds a rank like any other settlement");
    assert_eq!(settlement.position, 0);
    assert_eq!(
        settlement.sequence, 1,
        "a cancellation is a terminal, so it allocates a rank rather than \
         leaving the child unsettled forever"
    );
    let error = settlement
        .outcome
        .expect_err("a cancelled child's terminal is its cancellation");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        "the journaled terminal must name the cancellation, not some later \
         reader's guess at why the child stopped"
    );
    close(&resumed, reopened, LoserDisposition::Cancel)
        .await
        .expect("the reading host closes the group");
}

// =============================================================================
// The laws
// =============================================================================

/// The capability flag is the admission gate, and the scoped view a host is
/// actually reached through must answer it the same way.
///
/// A host that forwarded the flag but left the methods on their fail-closed
/// defaults would advertise support and then refuse every group, so the flag is
/// asserted through the same object that runs the group.
async fn the_capability_flag_and_the_group_surface_agree<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scope = scope(prefix, "flag");
    let scoped = host.scoped(scope.clone()).expect("a scope binds");
    assert!(
        scoped.controller().supports_effect_groups(),
        "a host whose group methods work must say so: the flag gates admission, \
         and a `false` here means no batch path at all"
    );

    let key = group_key(prefix, "flag");
    let mut handle = open(
        &scoped,
        &key,
        1,
        GroupWakePolicy::All,
        RUN,
        vec![settles(0)],
    )
    .await;
    let settlement = next(&scoped, &mut handle).await.expect("rank 1 is served");
    assert_eq!(settlement.position, 0);
    assert_eq!(settlement.sequence, 1, "the first settlement holds rank 1");
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// Two children sharing a replay key never reach a host, and the refusal costs
/// the group key nothing.
///
/// A replay key is the journal's identity for a child, so a duplicate is one
/// row, one claim and one rank: the second child would replay the first's
/// terminal, the last rank would never be allocated, and the caller would park
/// on it forever. That is a silent permanent hang, which is exactly the shape
/// this seam refuses rather than trusts, so the law is answered by every host —
/// including the inline tier, where the same lookup would mis-attribute the one
/// settlement that did land.
///
/// The second half is what makes this a law about hosts rather than about a
/// constructor: the same key opens cleanly afterwards, so the refusal happened
/// before anything was written under it.
async fn duplicate_replay_keys_are_refused_before_a_host_sees_them<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "dup")).expect("a scope binds");
    let key = group_key(prefix, "dup");

    let error = RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            RuntimeScope::new(key.as_str()),
            "group",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("{key}:group"),
        ),
        key.as_str(),
        // Both children mint position 0's replay key.
        vec![child(&key, 0), child(&key, 0)],
        GroupWakePolicy::All,
        RUN,
    )
    .expect_err("two children under one replay key are one journaled child");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "the collision is a shape refusal, typed like every other disagreement \
         between a group and its children"
    );

    let mut handle = open(
        &scoped,
        &key,
        1,
        GroupWakePolicy::All,
        RUN,
        vec![settles(0)],
    )
    .await;
    let settlement = next(&scoped, &mut handle)
        .await
        .expect("the refused group left the key untouched, so it still opens");
    assert_eq!(settlement.sequence, 1, "rank 1 is still unallocated");
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// First-settlement wake: the caller resumes on the winner while the loser is
/// still running.
///
/// Asserted on the loser's own gate rather than on wall time — "the caller did
/// not wait for it" is then a fact about the host, not about the scheduler.
async fn the_first_settlement_wakes_the_caller_while_the_loser_still_runs<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "race")).expect("a scope binds");
    let key = group_key(prefix, "race");
    let (slow, loser) = gated(0);
    let mut handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::First,
        RUN,
        vec![slow, settles(1)],
    )
    .await;

    let settlement = next(&scoped, &mut handle)
        .await
        .expect("the first settlement arrives");
    assert_eq!(
        settlement.position, 1,
        "the child that settled first must be the one delivered, whatever its \
         input position"
    );
    assert_eq!(settlement.sequence, 1, "the winner holds the first rank");
    assert_eq!(handle.consumed(), 1);
    assert_eq!(
        loser.finished(),
        0,
        "the caller resumed on the winner, so the loser cannot have completed"
    );
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// The settlement at rank `n` is a record, not a race: reading it again — as a
/// replayed frame does — yields the same child.
///
/// The re-read arrives through [`EffectGroupHandle::restored`], the path a
/// durable continuation actually takes, which pins the reopen rule too: the
/// caller's cursor wins and the host keeps no consumption state of its own.
async fn settlement_n_is_stable_across_re_reads<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "stable")).expect("a scope binds");
    let key = group_key(prefix, "stable");
    let (slow, loser) = gated(0);
    let mut handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::First,
        RUN,
        vec![slow, settles(1)],
    )
    .await;
    let first = next(&scoped, &mut handle)
        .await
        .expect("the first settlement arrives");

    // Let the loser settle, so a host serving "the next unread settlement"
    // rather than rank `consumed + 1` has a different answer available.
    loser.release();
    let second = next(&scoped, &mut handle)
        .await
        .expect("the second settlement arrives");
    assert_eq!(
        second.sequence, 2,
        "sequences are monotonic in rank order, so rank 2 follows rank 1"
    );
    assert_ne!(second.position, first.position);

    let mut replayed =
        EffectGroupHandle::restored(key.as_str(), 2, 0).expect("a cursor at 0 restores");
    let re_read = next(&scoped, &mut replayed)
        .await
        .expect("the replayed frame re-reads rank 1");
    assert_eq!(
        (re_read.position, re_read.sequence),
        (first.position, first.sequence),
        "rank 1 is a decided fact; a replay must observe the same child at it"
    );
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// Every child settles exactly once, is delivered in strictly increasing rank
/// order, and carries its own outcome — success or failure.
///
/// The failing child is what makes the last clause more than an assertion about
/// arity: a host that reported a loser's failure as the group's failure, or
/// swallowed it, disagrees here.
async fn every_child_is_delivered_once_in_rank_order<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "order")).expect("a scope binds");
    let key = group_key(prefix, "order");
    let width = 4;
    let mut handle = open(
        &scoped,
        &key,
        width,
        GroupWakePolicy::All,
        RUN,
        vec![settles(0), fails(1), settles(2), fails(3)],
    )
    .await;

    let mut sequences = Vec::new();
    let mut positions = Vec::new();
    while !handle.is_exhausted() {
        let settlement = next(&scoped, &mut handle)
            .await
            .expect("every child settles");
        assert_eq!(
            settlement.outcome.is_ok(),
            settlement.position % 2 == 0,
            "each settlement carries its own child's outcome, not the group's"
        );
        sequences.push(settlement.sequence);
        positions.push(settlement.position);
    }
    assert_eq!(sequences.len(), width);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "delivering by rank must yield strictly increasing sequences, not {sequences:?}"
    );
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions,
        (0..width).collect::<Vec<_>>(),
        "every child settles exactly once, and no child settles twice"
    );
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// Exhaustion is the caller's arithmetic; awaiting past it is a shape refusal
/// rather than a hang.
async fn awaiting_past_the_last_child_is_refused<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "past")).expect("a scope binds");
    let key = group_key(prefix, "past");
    let mut handle = open(
        &scoped,
        &key,
        1,
        GroupWakePolicy::All,
        RUN,
        vec![settles(0)],
    )
    .await;
    next(&scoped, &mut handle).await.expect("rank 1 is served");
    assert!(handle.is_exhausted());

    let error = next(&scoped, &mut handle)
        .await
        .expect_err("awaiting past the last child must be refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "awaiting past the group is a shape refusal, not a wait for a rank that \
         cannot exist"
    );
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// A cancelled await leaves the cursor and the durable rank untouched, so a
/// later await resumes at the same settlement.
async fn a_cancelled_await_leaves_the_rank_to_be_read_again<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host
        .scoped(scope(prefix, "cancelled"))
        .expect("a scope binds");
    let key = group_key(prefix, "cancelled");
    let (slow, gate) = gated(0);
    let mut handle = open(&scoped, &key, 1, GroupWakePolicy::First, RUN, vec![slow]).await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = scoped
        .controller()
        .await_next_settlement(&mut handle, cancel)
        .await
        .expect_err("a cancelled await must not deliver a settlement");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled
    );
    assert_eq!(
        handle.consumed(),
        0,
        "a cancelled await must not advance the cursor, or rank 1 is lost"
    );

    gate.release();
    let settlement = next(&scoped, &mut handle)
        .await
        .expect("the same rank is served to the later await");
    assert_eq!((settlement.position, settlement.sequence), (0, 1));
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// `RunToCompletion`: a loser keeps running under host ownership after the
/// caller has closed and moved on.
///
/// The structural claim underneath is that children do not run inside the
/// caller's future — a host that owned its leaves there would drop this loser at
/// the close.
async fn run_to_completion_losers_settle_after_the_caller_is_gone<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "run")).expect("a scope binds");
    let key = group_key(prefix, "run");
    let (slow, loser) = gated(1);
    let mut handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::First,
        RUN,
        vec![settles(0), slow],
    )
    .await;
    let winner = next(&scoped, &mut handle)
        .await
        .expect("the winner settles");
    assert_eq!(winner.position, 0);

    close(&scoped, handle, RUN)
        .await
        .expect("the caller closes and moves on");

    loser.release();
    until(|| loser.finished() == 1).await;
}

/// `Cancel`: the losers stop, and the caller that closed may observe nothing
/// further.
///
/// The journaled half — each cancelled child holding its cancellation as its
/// *terminal* — is a durable fact, asserted by
/// [`effect_group_cancelled_child_terminal_is_durable`] from each store's own
/// tests. What every tier owes here is that the loser does not go
/// on to complete and that a closed group is closed to its caller.
async fn cancel_stops_the_losers_and_closes_the_caller_out<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "cancel")).expect("a scope binds");
    let key = group_key(prefix, "cancel");
    let (slow, loser) = gated(1);
    let mut handle = open(
        &scoped,
        &key,
        3,
        GroupWakePolicy::First,
        LoserDisposition::Cancel,
        vec![settles(0), slow, never()],
    )
    .await;
    next(&scoped, &mut handle)
        .await
        .expect("the winner settles");
    let replayed = EffectGroupHandle::restored(key.as_str(), 3, 1).expect("the cursor restores");
    close(&scoped, handle, LoserDisposition::Cancel)
        .await
        .expect("the caller closes under the declared disposition");

    // The release arrives after the cancellation and must not resurrect the
    // child: a cancelled child's terminal is its cancellation.
    loser.release();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        loser.finished(),
        0,
        "a cancelled child must not go on to complete"
    );

    let mut replayed = replayed;
    let error = next(&scoped, &mut replayed)
        .await
        .expect_err("a closed group must serve its caller no further settlements");
    assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
}

/// Close may narrow the declared disposition and may never widen it: a
/// crash-drain applies the *declared* one, so permitting a widening close would
/// make the losers' fate depend on whether the caller reached its close at all.
async fn a_close_may_narrow_but_never_widen<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "narrow")).expect("a scope binds");

    let widened_key = group_key(prefix, "narrow-widened");
    let handle = open(
        &scoped,
        &widened_key,
        1,
        GroupWakePolicy::First,
        LoserDisposition::Cancel,
        vec![never()],
    )
    .await;
    let error = close(&scoped, handle, RUN)
        .await
        .expect_err("a declared Cancel may not be closed as RunToCompletion");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "widening is a shape refusal"
    );

    // Narrowing runs the other way, and is resolved against the disposition in
    // force: a group closed once as declared is still narrowable, and the
    // narrowing stops the child the first close left running.
    let narrowed_key = group_key(prefix, "narrow-narrowed");
    let (slow, loser) = gated(0);
    let handle = open(
        &scoped,
        &narrowed_key,
        1,
        GroupWakePolicy::First,
        RUN,
        vec![slow],
    )
    .await;
    close(&scoped, handle, RUN)
        .await
        .expect("closing as declared releases the caller's interest");
    let replayed =
        EffectGroupHandle::restored(narrowed_key.as_str(), 1, 0).expect("a handle restores");
    close(&scoped, replayed, LoserDisposition::Cancel)
        .await
        .expect("a caller that has learned it no longer wants the losers may narrow");
    loser.release();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        loser.finished(),
        0,
        "narrowing to Cancel must stop the child the earlier close left running"
    );
}

/// Close is idempotent, because a crash between a successful close and the
/// continuation commit replays it.
async fn closing_twice_under_one_disposition_succeeds<F: Fn() -> Host>(make: &F, prefix: &str) {
    let host = make();
    let scoped = host
        .scoped(scope(prefix, "idempotent"))
        .expect("a scope binds");
    let key = group_key(prefix, "idempotent");
    let handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::First,
        LoserDisposition::Cancel,
        vec![never(), never()],
    )
    .await;
    close(&scoped, handle, LoserDisposition::Cancel)
        .await
        .expect("the first close applies the disposition");
    let replayed = EffectGroupHandle::restored(key.as_str(), 2, 0).expect("a handle restores");
    close(&scoped, replayed, LoserDisposition::Cancel)
        .await
        .expect("a replayed close under the same disposition must succeed");
}

/// A reopen is fenced on the group's shape, and reopening a group this host is
/// already running dispatches nothing.
///
/// Both halves matter for the same reason: a shrunk child vec under one key
/// renumbers every rank above the truncation, and a second dispatch doubles
/// every side effect the first is still producing.
async fn a_reopen_is_fenced_on_shape_and_runs_no_child_twice<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host.scoped(scope(prefix, "reopen")).expect("a scope binds");
    let key = group_key(prefix, "reopen");
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = |runs: &Arc<AtomicUsize>, position: usize| {
        let runs = Arc::clone(runs);
        RuntimeEffectLocalExecutor::testing(move |_| async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(outcome_of(position))
        })
    };

    let mut handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::All,
        RUN,
        vec![counted(&runs, 0), counted(&runs, 1)],
    )
    .await;
    next(&scoped, &mut handle).await.expect("rank 1 is served");
    next(&scoped, &mut handle).await.expect("rank 2 is served");
    assert_eq!(runs.load(Ordering::SeqCst), 2, "each child ran once");

    // A reopen of the same shape is legal and must not run anything again.
    let reopened = scoped
        .controller()
        .open_effect_group(staged(
            group(&key, 2, GroupWakePolicy::All, RUN),
            vec![counted(&runs, 0), counted(&runs, 1)],
        ))
        .await
        .expect("a reopen of the same shape is legal");
    assert_eq!(reopened.consumed(), 0, "a reopened handle starts at zero");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "a reopen must not run a child a second time"
    );

    // A reopen under a different child count is refused: the fence is on shape.
    let error = scoped
        .controller()
        .open_effect_group(staged(
            group(&key, 1, GroupWakePolicy::All, RUN),
            vec![counted(&runs, 0)],
        ))
        .await
        .expect_err("a reopen with a different child count must be refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "a changed child count renumbers every rank above the change"
    );
    close(&scoped, reopened, RUN)
        .await
        .expect("the group closes");
    close(&scoped, handle, RUN)
        .await
        .expect("closing an already-closed group is idempotent");
}

/// A rank recorded by one host instance is readable by another over the same
/// substrate, and the handover runs no child twice.
///
/// This is the headline durability claim, stated as a law rather than as a
/// module comment: the settlement order a caller observed before it went away
/// survives in the substrate, not in the memory of the host that watched it
/// happen. A resuming host reopens the group, re-derives what it needs from the
/// group it was handed, and reads rank 1 back — including a rank recorded by a
/// child the *first* host is still running.
async fn a_second_host_instance_reads_the_ranks_the_first_recorded<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let first = make();
    let scoped = first
        .scoped(scope(prefix, "handoff"))
        .expect("a scope binds");
    let key = group_key(prefix, "handoff");
    let (slow, loser) = gated(1);
    let mut handle = open(
        &scoped,
        &key,
        2,
        GroupWakePolicy::First,
        RUN,
        vec![settles(0), slow],
    )
    .await;
    let winner = next(&scoped, &mut handle)
        .await
        .expect("the first host observes rank 1");
    assert_eq!(winner.position, 0);

    // The resuming host: a second instance over the same substrate, which knows
    // only what a durable continuation carries — the group and its own cursor.
    let second = make();
    let resumed = second
        .scoped(scope(prefix, "handoff"))
        .expect("a scope binds on the resuming host");
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = |position: usize| {
        let runs = Arc::clone(&runs);
        RuntimeEffectLocalExecutor::testing(move |_| async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(outcome_of(position))
        })
    };
    let mut reopened = resumed
        .controller()
        .open_effect_group(staged(
            group(&key, 2, GroupWakePolicy::First, RUN),
            vec![counted(0), counted(1)],
        ))
        .await
        .expect("the resuming host reopens the group");

    let re_read = next(&resumed, &mut reopened)
        .await
        .expect("the resuming host reads rank 1");
    assert_eq!(
        (re_read.position, re_read.sequence),
        (winner.position, winner.sequence),
        "rank 1 is a fact about the group, not about the host that first read it"
    );

    // A rank the resuming host never saw allocated is served to it too.
    loser.release();
    let second_settlement = next(&resumed, &mut reopened)
        .await
        .expect("the resuming host reads rank 2");
    assert_eq!(second_settlement.position, 1);
    assert_eq!(second_settlement.sequence, 2);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a handover replays recorded terminals; it must not re-run a child the \
         first host already ran or is still running"
    );

    close(&resumed, reopened, RUN)
        .await
        .expect("the resuming host closes the group");
    close(&scoped, handle, RUN)
        .await
        .expect("the first host's close is idempotent");
}

// =============================================================================
// Fixtures
// =============================================================================

const RUN: LoserDisposition = LoserDisposition::RunToCompletion;

/// Every await is bounded: a host that never serves a rank must fail this suite
/// as a timeout at the law that waited, not as a hung test binary.
const AWAIT_BUDGET: Duration = Duration::from_secs(10);

fn scope(prefix: &str, label: &str) -> ExecutionScope {
    ExecutionScope::runtime_operation(format!("{prefix}-{label}"))
}

fn group_key(prefix: &str, label: &str) -> String {
    format!("{prefix}:group:{label}:0")
}

fn child(group_key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new(group_key),
            "effect",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("{group_key}:child:{position}"),
        ),
        // Deliberately not `Sleep`, `AwaitEvent`, `PeekAwaitEvent` or `Process`:
        // the durable driver serves those from its own path rather than from the
        // caller's executor, and a law about *which child settled first* needs
        // the executor the law supplied to be the thing that runs.
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: format!("group-child-{position}"),
        },
    )
}

fn group(
    key: &str,
    children: usize,
    wake: GroupWakePolicy,
    disposition: LoserDisposition,
) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            RuntimeScope::new(key),
            "group",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("{key}:group"),
        ),
        key,
        (0..children).map(|position| child(key, position)).collect(),
        wake,
        disposition,
    )
    .expect("a group with at least one child assembles")
}

/// A test-side [`GroupExecutors`] resolver, keyed by replay key.
///
/// FIG-1578 moved execution off the open call: a host resolves each child of a
/// journaled group through the resolver it was registered with, so a test can no
/// longer hand executors in beside the group. Instead it *stages* them here
/// under the children's replay keys, registers this resolver on the host it
/// builds, and opens the group the staging returned.
///
/// Crate-visible because every tier's tests inside lash-core need the same seam
/// — the shared suite below and the inline reference tests — while a store's own
/// tests get the resolver handed to them by the suite factory.
pub(crate) struct StagedGroupExecutors {
    staged: std::sync::Mutex<HashMap<String, RuntimeEffectLocalExecutor<'static>>>,
}

impl StagedGroupExecutors {
    /// An empty resolver: every child is a routing miss until it is staged.
    pub(crate) fn new() -> Self {
        Self {
            staged: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Stages one executor per child under the children's replay keys, and hands
    /// back the group to open.
    ///
    /// An executor is taken by the first resolution that asks for it: a child
    /// runs once per open, and a replayed child is served from its record
    /// without an ask.
    pub(crate) fn stage(
        &self,
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    ) -> RuntimeEffectGroup {
        assert_eq!(
            group.children().len(),
            executors.len(),
            "a test stages one executor per child"
        );
        let mut staged = self.staged.lock_recover();
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

impl Default for StagedGroupExecutors {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupExecutors for StagedGroupExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope.invocation.replay_key()?;
        self.staged.lock_recover().remove(replay_key)
    }
}

/// The one resolver every host in this suite is built with.
///
/// Process-wide rather than per-law because a law's *second* host — the one that
/// stands in for the process that resumes a continuation — must answer the same
/// routing question as the first, and it was never handed the first's memory.
/// Group keys carry a per-run uuid prefix, so two laws can never stage the same
/// child.
fn suite_executors() -> Arc<StagedGroupExecutors> {
    static EXECUTORS: std::sync::OnceLock<Arc<StagedGroupExecutors>> = std::sync::OnceLock::new();
    Arc::clone(EXECUTORS.get_or_init(|| Arc::new(StagedGroupExecutors::new())))
}

fn staged(
    group: RuntimeEffectGroup,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> RuntimeEffectGroup {
    suite_executors().stage(group, executors)
}

/// The outcome a settling child produces, carrying its own position so a
/// delivered settlement can be tied back to the child that produced it.
fn outcome_of(position: usize) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::LanguageRuntimeValue {
        value: serde_json::json!({ "position": position }),
    }
}

/// An executor that settles as soon as it is polled.
fn settles(position: usize) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(outcome_of(position)) })
}

/// An executor that fails as soon as it is polled, so a settlement's `outcome`
/// can be read as well as its rank.
fn fails(position: usize) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
            format!("child {position} rejects"),
        ))
    })
}

/// An executor that never settles, used to keep a group incomplete while a law
/// reads it.
fn never() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async {
        std::future::pending::<()>().await;
        unreachable!("a never-settling child is never polled to completion")
    })
}

/// An executor gated on a release signal, plus the handle a law releases it and
/// observes it through.
struct Gate {
    release: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    finished: Arc<AtomicUsize>,
}

impl Gate {
    fn release(&self) {
        if let Some(sender) = self.release.lock_recover().take() {
            // A closed receiver means the child was cancelled before its
            // release, which is exactly what a `Cancel` law asserts.
            let _ = sender.send(());
        }
    }

    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

fn gated(position: usize) -> (RuntimeEffectLocalExecutor<'static>, Arc<Gate>) {
    let (release, released) = oneshot::channel();
    let finished = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&finished);
    let executor = RuntimeEffectLocalExecutor::testing(move |_| async move {
        // A dropped sender means the child was cancelled before its release,
        // which must not be reported as a completion.
        if released.await.is_err() {
            std::future::pending::<()>().await;
        }
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(outcome_of(position))
    });
    (
        executor,
        Arc::new(Gate {
            release: std::sync::Mutex::new(Some(release)),
            finished,
        }),
    )
}

async fn open(
    scoped: &ScopedEffectController<'_>,
    key: &str,
    children: usize,
    wake: GroupWakePolicy,
    disposition: LoserDisposition,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> EffectGroupHandle {
    scoped
        .controller()
        .open_effect_group(staged(group(key, children, wake, disposition), executors))
        .await
        .expect("the group opens")
}

async fn next(
    scoped: &ScopedEffectController<'_>,
    handle: &mut EffectGroupHandle,
) -> Result<GroupSettlement, RuntimeEffectControllerError> {
    tokio::time::timeout(
        AWAIT_BUDGET,
        scoped
            .controller()
            .await_next_settlement(handle, CancellationToken::new()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "a settlement at rank {} of durable effect group {} never arrived",
            handle.consumed() + 1,
            handle.group_key()
        )
    })
}

async fn close(
    scoped: &ScopedEffectController<'_>,
    handle: EffectGroupHandle,
    disposition: LoserDisposition,
) -> Result<(), RuntimeEffectControllerError> {
    scoped
        .controller()
        .close_effect_group(handle, disposition)
        .await
}

/// Waits for a condition a host reaches on its own tasks, so a law never
/// depends on how many yields a settlement happens to take.
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(AWAIT_BUDGET, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}
