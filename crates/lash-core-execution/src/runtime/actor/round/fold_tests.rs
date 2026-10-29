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
use lash_kernel_value::identity::SessionId;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use lash_protocol_types::ExecutionLimit;
use lash_vocabulary::identity::TurnId;

use super::records::{Record, StagedRecord};
use super::{
    AdmittedExecution, CompletedCall, ExecutionDraft, FoldRefusal, PinnedWaits, PolicyView,
    Recovery, RunFold, RunRecords, admit, completed_material, fold, settle, settle_retry,
    settle_staged, start_retry,
};
use crate::ModelToolReturn;
use crate::ToolCallId;
use crate::ToolCallOutput;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_vocabulary::tool_contract::ToolId;

const RUN: RunSeq = RunSeq(1);

/// FIG-7255: an admitted tool identity never suppresses its answered call
/// from the terminal transcript, including the retired cell-host spelling.
#[test]
fn every_legal_tool_identity_keeps_its_answer_in_the_transcript() {
    let mut tx = opened();
    let members = admitted(
        &mut tx,
        vec![
            draft("a", "cell-host:customer", ExecutionPolicy::Once),
            draft("b", "ordinary", ExecutionPolicy::Once),
        ],
    );
    for member in members.iter().rev() {
        let output = ToolCallOutput::success(serde_json::json!({"found": true}));
        let answer = CompletedCall {
            call_id: member.draft().call().clone(),
            provider_call_id: None,
            tool_name: "tools.lookup".to_owned(),
            args: serde_json::json!({}),
            model_return: ModelToolReturn::from_output("tools.lookup".to_owned(), &output),
            output,
            control_origin: lash_sansio::ToolControlOrigin::OwnCall,
            intent_outcomes: Vec::new(),
            replay: None,
        };
        let material = completed_material(&EffectOpener::turn("s", "t"), answer).unwrap();
        settle(
            &mut tx,
            member,
            SettledOutput::Completed(material),
            Vec::new(),
            None,
        )
        .unwrap();
    }
    let facts = rows(&tx);
    let transcript = super::TurnTranscript::of(&facts).unwrap().unwrap();
    let mut decoder =
        lash_core_store::checkpoint_graph::GraphDecoder::new(&facts, facts.read_limits);
    let records = transcript
        .records(&mut decoder, &Default::default())
        .unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.call_id.clone())
            .collect::<Vec<_>>(),
        vec![ToolCallId::fixture("a"), ToolCallId::fixture("b")],
        "every answered call remains visible in admission order"
    );
}

/// V24: a presentation cannot precede any admitted member's settlement.
#[test]
fn presentation_before_all_members_settle_is_refused() {
    let mut tx = opened();
    admitted(&mut tx, vec![draft("a", "once", ExecutionPolicy::Once)]);
    present(&mut tx, Ordinal(2));
    assert!(fold(&rows(&tx), &PolicyView::default(), &PinnedWaits::default()).is_err());
}

/// Appends the run's presentation marker at `ordinal`.
fn present(tx: &mut ActorTx, ordinal: Ordinal) {
    super::records::append(
        tx,
        &owner(),
        RUN,
        ordinal,
        None,
        &super::records::Record::Present(Vec::new()),
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
        lash_durable::DurableInstant(0),
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

/// ACKNOWLEDGED-DELTA (FIG-7056): each record an owner stages for its fold
/// is the record a read of its committed row gives. A typed completion, a
/// payload kept as text, a park and a retry are each held; the record read
/// from the row stages as the same node, and its material is held as the
/// staged one is: the typed answer as its value, its JSON text byte for
/// byte, the text payload as its text. A fold over the staged records
/// agrees with a fold that reads every row.
#[test]
fn an_acknowledged_commits_staged_records_are_the_records_its_rows_read_as() {
    let mut tx = opened();
    let members = admitted(
        &mut tx,
        vec![
            draft("a", "once", ExecutionPolicy::Once),
            draft("b", "once", ExecutionPolicy::Once),
            draft("c", "once", ExecutionPolicy::Once),
            draft("d", "repeats", repeatable()),
        ],
    );
    let mut records = RunRecords::new(rows(&tx));
    let output = ToolCallOutput::success(serde_json::json!({"answer": "\u{1}\"\\".repeat(500)}));
    let answer = CompletedCall {
        control_origin: lash_sansio::ToolControlOrigin::OwnCall,
        call_id: ToolCallId::fixture("a"),
        provider_call_id: Some("provider-a".to_owned()),
        tool_name: "once".to_owned(),
        args: serde_json::json!({"q": "rows", "n": -0.0}),
        model_return: ModelToolReturn::from_output("once".to_owned(), &output),
        output,
        intent_outcomes: Vec::new(),
        replay: None,
    };
    let text = serde_json::to_string(&answer).unwrap();
    let typed = completed_material(
        &EffectOpener::turn(
            SessionId::try_from("s".to_owned()).unwrap(),
            TurnId::try_from("t".to_owned()).unwrap(),
        ),
        answer,
    )
    .unwrap();

    let mut tx = opened();
    let outputs = [
        SettledOutput::Completed(typed),
        SettledOutput::Completed(journal("text")),
        SettledOutput::Waiting(journal("parked").parked("wait".to_owned())),
    ];
    let mut staged: Vec<StagedRecord> = members
        .iter()
        .zip(outputs)
        .map(|(member, output)| settle_staged(&mut tx, member, output, Vec::new(), None).unwrap())
        .collect();
    staged.push(settle_retry(&mut tx, &members[3], failed(), DurableInstant(50)).unwrap());
    let appended = rows(&tx);

    for (index, staged) in staged.iter().enumerate() {
        let held = staged.record.as_deref().expect("the staged record is held");
        let row = appended
            .rows
            .iter()
            .find(|row| row.record == staged.node)
            .expect("the staged node is a row's");
        let read = super::records::read(&appended, row).unwrap();
        let mut scratch = opened();
        let again = super::records::append_staged(
            &mut scratch,
            &owner(),
            RUN,
            row.ordinal,
            row.call.as_ref(),
            read,
        );
        assert_eq!(again.node, staged.node, "record {index} reads as staged");
        let read = again.record.expect("the read record is held");
        let (
            Record::Outcome(held) | Record::Wait(held),
            Record::Outcome(read) | Record::Wait(read),
        ) = (held, &*read)
        else {
            let (Record::Retry(held), Record::Retry(read)) = (held, &*read) else {
                panic!("record {index} changed its kind");
            };
            assert_eq!(held.output.payload(), read.output.payload());
            continue;
        };
        assert_eq!(held.output.payload(), read.output.payload());
        assert_eq!(held.ended_at_ms, read.ended_at_ms);
        match super::completed_call(&held.output) {
            Some(held) => {
                let read = super::completed_call(&read.output).expect("the answer reads typed");
                assert_eq!(serde_json::to_string(read).unwrap(), text);
                assert_eq!(serde_json::to_string(held).unwrap(), text);
            }
            None => assert!(super::completed_call(&read.output).is_none()),
        }
    }

    let mut read = RunRecords::new(records.facts().clone());
    read.extend(appended.clone(), Vec::new());
    records.extend(appended, staged);
    let policies = PolicyView::new([
        (ToolId::new("once"), ExecutionPolicy::Once),
        (ToolId::new("repeats"), repeatable()),
    ]);
    let (staged, read) = (
        records.fold(&policies, &PinnedWaits::default()).unwrap(),
        read.fold(&policies, &PinnedWaits::default()).unwrap(),
    );
    assert_eq!(recoveries(&staged), recoveries(&read));
    let outcomes = |fold: &RunFold| -> Vec<Option<String>> {
        fold.round(RUN)
            .unwrap()
            .members()
            .iter()
            .map(|member| {
                member
                    .outcome()
                    .and_then(|output| output.payload().map(str::to_owned))
            })
            .collect()
    };
    assert_eq!(outcomes(&staged), outcomes(&read));
}
