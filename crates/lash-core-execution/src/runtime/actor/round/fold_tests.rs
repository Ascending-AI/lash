//! The fold's laws over hand-built rows: L-B3 (a current declaration vetoes
//! a repeat and never upgrades `Once`), gaps and a second final refused.

use std::num::NonZeroU32;
use std::time::Duration;

use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    KnownFailureReason, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::{Ordinal, OwnerKey, RunFacts, RunSeq};
use lash_durable::{ActorKey, ActorTx, DurableInstant, Epoch, MailSeq, OpenedActor, StateRevision};
use lash_protocol_types::{ExecutionLimit, ExecutionPolicy, SessionId, TurnId};

use super::{
    AdmittedExecution, ExecutionDraft, FoldRefusal, PinnedWaits, PolicyView, Recovery, RunFold,
    admit, fold, settle, settle_retry, start_retry,
};
use crate::{ToolCallId, ToolId};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;

const RUN: RunSeq = RunSeq(1);

/// V24: a presentation cannot precede any admitted member's settlement.
#[test]
fn presentation_before_all_members_settle_is_refused() {
    let mut tx = opened();
    admitted(&mut tx, vec![draft("a", "once", ExecutionPolicy::Once)]);
    present(&mut tx, Ordinal(2));
    assert!(fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).is_err());
}

/// V24: the marker derives every presented call from ordered admission.
#[test]
fn a_presentation_marker_derives_its_members_in_admission_order() {
    let mut tx = opened();
    let members = admitted(
        &mut tx,
        vec![
            draft("a", "once", ExecutionPolicy::Once),
            draft("b", "once", ExecutionPolicy::Once),
        ],
    );
    for member in members.iter().rev() {
        settle(
            &mut tx,
            member,
            SettledOutput::Interrupted,
            Vec::new(),
            None,
        )
        .unwrap();
    }
    let next = Ordinal(rows(&tx).rows.len() as u64);
    present(&mut tx, next);
    let folded = fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).unwrap();
    assert_eq!(
        folded.round(RUN).unwrap().presented(),
        Some(vec![ToolCallId::fixture("a"), ToolCallId::fixture("b")])
    );
}

/// Appends the run's presentation marker at `ordinal`.
fn present(tx: &mut ActorTx, ordinal: Ordinal) {
    super::records::append(
        tx,
        &owner(),
        RUN,
        ordinal,
        None,
        &super::records::Record::Present,
    );
}

fn owner() -> OwnerKey {
    OwnerKey::Turn(
        SessionId::try_from("s".to_owned()).unwrap(),
        TurnId::try_from("t".to_owned()).unwrap(),
    )
}

fn opened() -> ActorTx {
    ActorTx::opened(OpenedActor {
        actor: ActorKey::session("s").unwrap(),
        epoch: Epoch(1),
        revision: StateRevision(0),
        acked: MailSeq(0),
        seen: MailSeq(0),
        mail: Vec::new(),
        at: DurableInstant(0),
        turn_cancel: None,
    })
}

/// The facts `tx` appends, as the store would hold them.
fn rows(tx: &ActorTx) -> RunFacts {
    RunFacts::appended(tx.domain(), tx.graph(), Epoch(1))
}

fn run_owner() -> MaterialOwner {
    MaterialOwner::Run {
        opener: EffectOpener::turn(
            SessionId::try_from("s".to_owned()).unwrap(),
            TurnId::try_from("t".to_owned()).unwrap(),
        ),
    }
}

/// A request reference: the fold never reads its bytes.
fn material(payload: &str) -> MaterialRef {
    MaterialRef {
        owner: run_owner(),
        role: MaterialRole::PreparedRequest,
        location: MaterialLocation::JournalLocal,
        digest: MaterialDigest::parse(&format!("{:064x}", payload.len())).unwrap(),
    }
}

fn repeatable() -> ExecutionPolicy {
    ExecutionPolicy::repeatable(NonZeroU32::new(3).unwrap(), 10, 100)
}

fn draft(name: &str, tool: &str, policy: ExecutionPolicy) -> ExecutionDraft {
    ExecutionDraft::new(
        ToolCallId::fixture(name),
        ToolId::new(tool),
        material(name),
        policy,
        ExecutionLimit {
            expires_at: 1_000_000,
            max_slice: Duration::from_secs(60),
        },
        None,
    )
}

/// `payload` as journal-local material of the turn's run, minted by the
/// material codec.
fn journal(payload: &str) -> Material {
    Material::journal_local(run_owner(), MaterialRole::AttemptOutput, payload.to_owned())
}

/// A known failure, with the bytes its output names.
fn failed() -> SettledOutput {
    SettledOutput::Failed(journal("failed").failure(KnownFailureReason::Reported, None))
}

fn admitted(tx: &mut ActorTx, drafts: Vec<ExecutionDraft>) -> Vec<AdmittedExecution> {
    admit(tx, &owner(), RUN, drafts).unwrap()
}

fn recoveries(fold: &RunFold) -> Vec<Recovery> {
    fold.recoveries()
        .iter()
        .map(|(_, recovery)| recovery.clone())
        .collect()
}

/// L-B3: a started `Repeatable` without an outcome reruns at its ordinal,
/// unless the tool now declares `Once`; a started `Once` is interrupted
/// even when the tool now declares `Repeatable`.
#[test]
fn a_current_once_vetoes_a_stored_repeat_and_never_upgrades_a_stored_once() {
    let mut tx = opened();
    admitted(
        &mut tx,
        vec![
            draft("a", "repeats", repeatable()),
            draft("b", "once", ExecutionPolicy::Once),
        ],
    );
    let rows = rows(&tx);

    let as_pinned = PolicyView::new([
        (ToolId::new("repeats"), repeatable()),
        (ToolId::new("once"), ExecutionPolicy::Once),
    ]);
    assert_eq!(
        recoveries(&fold(&rows, &as_pinned, &PinnedWaits::default()).unwrap()),
        vec![Recovery::RerunAtOrdinal(Ordinal(1)), Recovery::Interrupt]
    );

    let flipped = PolicyView::new([
        (ToolId::new("repeats"), ExecutionPolicy::Once),
        (ToolId::new("once"), repeatable()),
    ]);
    assert_eq!(
        recoveries(&fold(&rows, &flipped, &PinnedWaits::default()).unwrap()),
        vec![Recovery::Interrupt, Recovery::Interrupt]
    );
}

/// L-B3 for a recorded retry: a current `Once` vetoes the due retry, and the
/// failed attempt's outcome becomes the call's final one.
#[test]
fn a_current_once_vetoes_a_due_retry() {
    let mut tx = opened();
    let members = admitted(&mut tx, vec![draft("a", "repeats", repeatable())]);
    settle_retry(&mut tx, &members[0], failed(), DurableInstant(50)).unwrap();
    let rows = rows(&tx);
    assert_eq!(
        recoveries(&fold(&rows, &PolicyView::default(), &PinnedWaits::default()).unwrap()),
        vec![Recovery::RetryDue {
            at: DurableInstant(50),
            attempt: 2
        }]
    );
    let vetoed = PolicyView::new([(ToolId::new("repeats"), ExecutionPolicy::Once)]);
    assert_eq!(
        recoveries(&fold(&rows, &vetoed, &PinnedWaits::default()).unwrap()),
        vec![Recovery::Vetoed(failed())]
    );
}

/// The `RunLedger` rule the rows keep: a run's ordinals have no gap.
#[test]
fn a_gap_in_a_runs_ordinals_is_refused() {
    let mut tx = opened();
    admitted(
        &mut tx,
        vec![
            draft("a", "once", ExecutionPolicy::Once),
            draft("b", "once", ExecutionPolicy::Once),
        ],
    );
    let mut rows = rows(&tx);
    rows.rows.remove(1);
    assert_eq!(
        fold(&rows, &PolicyView::default(), &PinnedWaits::default()),
        Err(FoldRefusal::OrdinalGap {
            run: RUN,
            missing: Ordinal(1)
        })
    );
}

/// The `RunLedger` rule the rows keep: a call has one final.
#[test]
fn a_second_final_of_a_call_is_refused() {
    let mut tx = opened();
    let members = admitted(&mut tx, vec![draft("a", "once", ExecutionPolicy::Once)]);
    settle(
        &mut tx,
        &members[0],
        SettledOutput::Interrupted,
        Vec::new(),
        None,
    )
    .unwrap();
    settle(
        &mut tx,
        &members[0],
        SettledOutput::Interrupted,
        Vec::new(),
        None,
    )
    .unwrap();
    assert_eq!(
        fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()),
        Err(FoldRefusal::SecondFinal(ToolCallId::fixture("a")))
    );
}

/// Retry ownership: an attempt starts only after its call's retry, and the
/// retry's attempt takes the run's next ordinal.
#[test]
fn a_retried_attempt_takes_the_next_ordinal_only_after_its_retry() {
    let mut tx = opened();
    let members = admitted(&mut tx, vec![draft("a", "repeats", repeatable())]);
    let early = rows(&tx);
    settle_retry(&mut tx, &members[0], failed(), DurableInstant(50)).unwrap();
    let next = start_retry(&mut tx, &members[0]);
    assert_eq!(next.ordinal(), Ordinal(3));
    assert_eq!(next.attempt(), 2);
    let folded = fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).unwrap();
    assert_eq!(
        recoveries(&folded),
        vec![Recovery::RerunAtOrdinal(Ordinal(3))]
    );

    // An x_start that follows no retry is out of its call's order.
    let resumed = fold(&early, &PolicyView::default(), &PinnedWaits::default()).unwrap();
    let mut stray = opened();
    start_retry(&mut stray, &resumed.admitted(members[0].id()).unwrap());
    let mut rows = early;
    let stray = super::fold_tests::rows(&stray);
    rows.rows.extend(stray.rows);
    rows.nodes.extend(stray.nodes);
    assert_eq!(
        fold(&rows, &PolicyView::default(), &PinnedWaits::default()),
        Err(FoldRefusal::OutOfOrder {
            run: RUN,
            ordinal: Ordinal(2)
        })
    );
}

/// A call whose retry is due may end without its next attempt: a turn
/// cancel or a current `Once`'s veto records its final at the failed
/// attempt, and the rows still fold.
#[test]
fn a_call_whose_retry_is_due_ends_at_its_failed_attempt() {
    let mut tx = opened();
    let members = admitted(&mut tx, vec![draft("a", "repeats", repeatable())]);
    settle_retry(&mut tx, &members[0], failed(), DurableInstant(50)).unwrap();
    let due = fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).unwrap();
    let view = due.round(RUN).unwrap();
    let execution = view.execution(&view.members()[0]);
    settle(&mut tx, &execution, failed(), Vec::new(), None).unwrap();
    assert_eq!(
        recoveries(&fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).unwrap()),
        vec![Recovery::Settled(failed())]
    );
}
