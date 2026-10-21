//! FUSED-RECEIPT (FIG-6955, FIG-6969): a final answer whose receipt owes no
//! callback, in a turn no checkpoint or after-turn callback runs in, commits
//! its receipt with the turn's commit, and recovers at every cut of that
//! commit as the reference protocol does at a crash before `model.answer`.
//! The merge gate: a turn whose session registers an after-turn callback
//! commits its receipt on its own.

use super::*;
use lash_durable::{DomainRefusal, DurableError};

/// FUSED-RECEIPT (FIG-6955): a final answer whose receipt owes no callback
/// (a stream hook needs no finish), on a turn no checkpoint or after-turn
/// callback runs in, commits its receipt with the turn's commit: the uncut
/// run commits no `model.answer`. Cut at that `turn.commit`, the turn
/// recovers to the outcome the reference protocol gives for a crash before
/// its `model.answer` (the logical point; at the same physical point, a
/// node lost after the answer, the reference had committed the receipt and
/// bought once). A commit that failed (a store failure) commits the receipt
/// on its own and the call is never bought again. One that landed (its
/// reply lost, or its node killed after it) is the turn's, and the receipt
/// that then commits on its own is refused `TurnNotOpen`: a landed
/// `turn.commit` pruned the model phase the receipt needs (FIG-6969). One
/// that never entered the store (its node killed or reaped first) leaves
/// the call unanswered at `model.start`, as a crash before `model.answer`
/// does, and the successor buys it again. Every cut commits one
/// `turn.commit`, one assistant answer (the purchase that commit carried)
/// and one retained model-call record.
async fn a_hook_free_answer_cut_at_its_turn_commit(fault: Option<Fault>) {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::StreamOnly), |script| {
        if let Some(fault) = fault {
            script.cut_on("a", CommitLabel::TURN_COMMIT, 1, fault);
        }
    })
    .await;
    let _handle = deployment.send().await;
    match fault {
        Some(Fault::Zombie) => {
            deployment
                .until("A pauses before its turn commit", |d| {
                    d.nodes.paused().contains(&"a".to_owned())
                })
                .await;
            deployment.nodes.start("b");
        }
        Some(fault) if fault.kills() => {
            deployment
                .until("A dies at its turn commit", |d| {
                    d.cut(CommitLabel::TURN_COMMIT)
                })
                .await;
            deployment.take_over().await;
        }
        _ => {}
    }
    deployment.settle().await;
    let trace = deployment.nodes.script().rendered_trace();
    let bought_again = matches!(fault, Some(Fault::Abort | Fault::Zombie));
    assert_eq!(
        deployment.world.sends(),
        if bought_again { 2 } else { 1 },
        "{fault:?}: a receipt is bought again only when nothing durable held it:\n{trace}"
    );
    assert_eq!(
        deployment.committed(CommitLabel::MODEL_ANSWER),
        usize::from(fault == Some(Fault::FailBefore)),
        "{fault:?}: the receipt commits on its own only after a turn commit that wrote nothing:\n{trace}"
    );
    assert_eq!(
        deployment.committed(CommitLabel::TURN_COMMIT),
        1,
        "{fault:?}: the turn commits once:\n{trace}"
    );
    if fault == Some(Fault::AckHidden) {
        let fenced = deployment.nodes.script().trace().iter().any(|write| {
            write.point.label == CommitLabel::MODEL_ANSWER
                && matches!(
                    write.stored,
                    Stored::Refused(DurableError::Domain(DomainRefusal::TurnNotOpen { .. }))
                )
        });
        assert!(
            fenced,
            "the receipt after a turn commit whose outcome was hidden is refused TurnNotOpen:\n{trace}"
        );
    }
    let transcript = deployment.transcript().await;
    assert_eq!(
        transcript
            .iter()
            .filter(|(role, _)| role == "Assistant")
            .count(),
        1,
        "{fault:?}: one answer: {transcript:?}"
    );
    assert_eq!(
        answered(&transcript),
        ANSWERS[usize::from(bought_again)],
        "{fault:?}: the turn answers the purchase its commit carried"
    );
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(rows.len(), 1, "{fault:?}: one retained model-call record");
    let usage = if bought_again {
        USAGE.checked_add(&LlmUsage::unknown_call()).unwrap()
    } else {
        USAGE
    };
    assert_eq!(deployment.usage().await, usage, "{fault:?}: charged once");
    deployment.end();
}

#[tokio::test]
async fn a_hook_free_answer_commits_its_receipt_with_its_turn() {
    a_hook_free_answer_cut_at_its_turn_commit(None).await;
}

#[tokio::test]
async fn a_hook_free_answer_cut_at_its_turn_commit_recovers_as_the_reference_protocol() {
    for fault in [
        Fault::FailBefore,
        Fault::AckHidden,
        Fault::CommitThenAbort,
        Fault::Abort,
        Fault::Zombie,
    ] {
        a_hook_free_answer_cut_at_its_turn_commit(Some(fault)).await;
    }
}

/// FUSED-RECEIPT, the merge gate (FIG-6969): a session that registers an
/// after-turn callback never carries its receipt in the turn's commit. The
/// callback reads the finished turn from the store, so the toolless
/// answer's receipt commits on its own, exactly once, before it runs.
#[tokio::test]
async fn an_answer_in_a_turn_with_an_after_turn_callback_commits_its_receipt_on_its_own() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::AfterTurn), |_| {}).await;
    let handle = deployment.send().await;
    deployment.settle().await;
    let trace = deployment.nodes.script().rendered_trace();
    handle.output().await.expect("the turn settles");
    assert_eq!(
        deployment.world.after_turns.load(Ordering::SeqCst),
        1,
        "the after-turn callback runs once:\n{trace}"
    );
    assert_eq!(
        deployment.committed(CommitLabel::MODEL_ANSWER),
        1,
        "the receipt commits on its own, once:\n{trace}"
    );
    assert_eq!(
        deployment.committed(CommitLabel::TURN_COMMIT),
        1,
        "the turn commits once:\n{trace}"
    );
    assert_eq!(deployment.world.sends(), 1, "the answer is bought once");
    assert_eq!(answered(&deployment.transcript().await), ANSWERS[0]);
    deployment.end();
}

/// NATIVE-RECEIPT-BOUNDARY (FIG-7083): a callback-free native round
/// commits its paid answer with its admission, before any tool body runs.
/// A failed admission preserves the answer on its own; a lost admission
/// acknowledgement cannot buy the answer or execute its Once tool again.
async fn a_native_answer_cut_at_its_admission(fault: Option<Fault>) {
    let deployment = Deployment::new(Setup::new(Answer::Tool, Hooks::StreamOnly), |script| {
        if let Some(fault) = fault {
            script.cut_on("a", CommitLabel::MODEL_DONE, 1, fault);
        }
    })
    .await;
    let handle = deployment.send().await;
    if matches!(fault, Some(Fault::Zombie)) {
        deployment
            .until("A pauses before its round admission", |d| {
                d.nodes.paused().contains(&"a".to_owned())
            })
            .await;
        deployment.nodes.start("b");
    } else if fault.is_some_and(Fault::kills) {
        deployment
            .until("A dies at its round admission", |d| {
                d.cut(CommitLabel::MODEL_DONE)
            })
            .await;
        deployment.take_over().await;
    }
    deployment.settle().await;
    handle.output().await.expect("the native turn settles");
    let trace = deployment.nodes.script().rendered_trace();
    assert_eq!(
        deployment.committed(CommitLabel::MODEL_ANSWER),
        usize::from(fault == Some(Fault::FailBefore)),
        "the answer needs no separate commit before native admission: {trace}"
    );
    assert_eq!(deployment.committed(CommitLabel::MODEL_DONE), 1, "{trace}");
    assert_eq!(
        deployment.world.sends(),
        if matches!(fault, Some(Fault::Abort | Fault::Zombie)) {
            2
        } else {
            1
        },
        "only an uncommitted answer can be purchased again: {trace}"
    );
    assert!(
        deployment.world.writes().len() <= 1,
        "Once body repeated: {trace}"
    );
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(
        rows.len(),
        2,
        "one retained record per logical call: {trace}"
    );
    let usage = rows
        .into_iter()
        .map(|row| serde_json::from_value::<lash::turn::ModelCallRecord>(row).unwrap())
        .try_fold(LlmUsage::default(), |sum, call| {
            sum.checked_add(&call.usage()?)
        })
        .unwrap();
    assert_eq!(
        usage,
        deployment.usage().await,
        "the sealed calls account for the committed usage: {trace}"
    );
    deployment.end();
}

#[tokio::test]
async fn a_native_answer_needs_no_commit_between_receipt_and_admission() {
    a_native_answer_cut_at_its_admission(None).await;
}

#[tokio::test]
async fn a_native_answer_recovers_at_every_cut_of_its_admission() {
    for fault in [
        Fault::FailBefore,
        Fault::AckHidden,
        Fault::CommitThenAbort,
        Fault::Abort,
        Fault::Zombie,
    ] {
        a_native_answer_cut_at_its_admission(Some(fault)).await;
    }
}

/// COMMITTED-FOLLOW (FIG-7083): only acknowledged settlement publishes a
/// subject's answer; another input must never inherit a known run's outcome.
#[tokio::test]
async fn an_observer_receives_only_its_inputs_acknowledged_terminal() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::StreamOnly), |script| {
        script.cut_on(
            "a",
            CommitLabel::TURN_COMMIT,
            1,
            Fault::DelayedAck(std::time::Duration::from_secs(1)),
        );
    })
    .await;
    let sink = deployment.backend.stores().wake_sink();
    let watch = sink.observe(actor(), None).await;
    assert!(
        watch
            .committed_turn(&lash_core::store::FollowSubject::Input(
                lash_vocabulary::identity::InputId::from("unrelated-input")
            ))
            .is_none()
    );
    let handle = deployment.send().await;
    let subject = lash_core::store::FollowSubject::Input(handle.input_id().clone());
    deployment
        .until(
            "the terminal is durable but its acknowledgement is held",
            |d| d.committed(CommitLabel::TURN_COMMIT) == 1,
        )
        .await;
    assert!(
        watch.committed_turn(&subject).is_none(),
        "a durable write without its acknowledgement publishes no facts"
    );
    deployment
        .until("the writer hands over its acknowledged terminal", |_| {
            watch.committed_turn(&subject).is_some()
        })
        .await;
    assert!(
        watch
            .committed_turn(&lash_core::store::FollowSubject::Input(
                lash_vocabulary::identity::InputId::from("unrelated-input")
            ))
            .is_none()
    );
    let (run, _) = watch
        .committed_turn(&subject)
        .expect("the commit handed over its run");
    assert!(
        watch
            .committed_turn(&lash_core::store::FollowSubject::Run(run))
            .is_some()
    );
    handle
        .output()
        .await
        .expect("the subject observes its answer");
    deployment.end();
}
