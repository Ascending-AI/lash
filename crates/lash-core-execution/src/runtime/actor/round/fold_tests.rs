//! The fold's laws over hand-built rows: L-B3 (a current declaration vetoes
//! a repeat and never upgrades `Once`), gaps and a second final refused.

use std::num::NonZeroU32;
use std::time::Duration;

use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AttemptOutcome, KnownFailure, KnownFailureReason, MaterialDigest, MaterialLocation,
    MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::{Ordinal, OwnerKey, RunRecordRow, RunRecordWrite, RunSeq};
use lash_durable::{
    ActorKey, ActorTx, DomainWrite, DurableInstant, Epoch, MailSeq, OpenedActor, StateRevision,
};
use lash_sansio::{ExecutionLimit, ExecutionPolicy, SessionId, TurnId};

use super::{
    AdmittedExecution, ExecutionDraft, FoldRefusal, PolicyView, Recovery, RunFold, admit, fold,
    settle, settle_retry, start_retry,
};
use crate::{ToolCallId, ToolId};

const RUN: RunSeq = RunSeq(1);

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
    })
}

/// The rows `tx` appends, as the store would hold them.
fn rows(tx: &ActorTx) -> Vec<RunRecordRow> {
    tx.domain()
        .iter()
        .filter_map(|write| match write {
            DomainWrite::RunRecord(RunRecordWrite::Append {
                owner,
                run,
                ordinal,
                kind,
                call,
                record_json,
            }) => Some(RunRecordRow {
                owner: owner.clone(),
                run: *run,
                ordinal: *ordinal,
                kind: *kind,
                call: call.clone(),
                record_json: record_json.clone(),
                written_epoch: Epoch(1),
            }),
            _ => None,
        })
        .collect()
}

fn material(payload: &str) -> MaterialRef {
    MaterialRef {
        owner: MaterialOwner::Run {
            opener: EffectOpener::turn(
                SessionId::try_from("s".to_owned()).unwrap(),
                TurnId::try_from("t".to_owned()).unwrap(),
            ),
        },
        role: MaterialRole::AttemptOutput,
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

fn failed() -> AttemptOutcome {
    AttemptOutcome::Failed(KnownFailure {
        output: material("failed"),
        reason: KnownFailureReason::Reported,
        suggested_delay_ms: None,
    })
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
        recoveries(&fold(&rows, &as_pinned).unwrap()),
        vec![Recovery::RerunAtOrdinal(Ordinal(1)), Recovery::Interrupt]
    );

    let flipped = PolicyView::new([
        (ToolId::new("repeats"), ExecutionPolicy::Once),
        (ToolId::new("once"), repeatable()),
    ]);
    assert_eq!(
        recoveries(&fold(&rows, &flipped).unwrap()),
        vec![Recovery::Interrupt, Recovery::Interrupt]
    );
}

/// L-B3 for a recorded retry: a current `Once` vetoes the due retry, and the
/// failed attempt's outcome becomes the call's final one.
#[test]
fn a_current_once_vetoes_a_due_retry() {
    let mut tx = opened();
    let members = admitted(&mut tx, vec![draft("a", "repeats", repeatable())]);
    settle_retry(&mut tx, &members[0], failed().into(), DurableInstant(50)).unwrap();
    let rows = rows(&tx);
    assert_eq!(
        recoveries(&fold(&rows, &PolicyView::default()).unwrap()),
        vec![Recovery::RetryDue {
            at: DurableInstant(50),
            attempt: 2
        }]
    );
    let vetoed = PolicyView::new([(ToolId::new("repeats"), ExecutionPolicy::Once)]);
    assert_eq!(
        recoveries(&fold(&rows, &vetoed).unwrap()),
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
    rows.remove(1);
    assert_eq!(
        fold(&rows, &PolicyView::default()),
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
    settle(&mut tx, &members[0], AttemptOutcome::Interrupted, None).unwrap();
    settle(&mut tx, &members[0], AttemptOutcome::Interrupted, None).unwrap();
    assert_eq!(
        fold(&rows(&tx), &PolicyView::default()),
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
    settle_retry(&mut tx, &members[0], failed().into(), DurableInstant(50)).unwrap();
    let next = start_retry(&mut tx, &members[0]);
    assert_eq!(next.ordinal(), Ordinal(3));
    assert_eq!(next.attempt(), 2);
    let folded = fold(&rows(&tx), &PolicyView::default()).unwrap();
    assert_eq!(
        recoveries(&folded),
        vec![Recovery::RerunAtOrdinal(Ordinal(3))]
    );

    // An x_start that follows no retry is out of its call's order.
    let resumed = fold(&early, &PolicyView::default()).unwrap();
    let mut stray = opened();
    start_retry(&mut stray, &resumed.admitted(members[0].id()).unwrap());
    let mut rows = early;
    rows.extend(super::fold_tests::rows(&stray));
    assert_eq!(
        fold(&rows, &PolicyView::default()),
        Err(FoldRefusal::OutOfOrder {
            run: RUN,
            ordinal: Ordinal(2)
        })
    );
}
