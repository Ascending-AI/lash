//! A tool round on the primitive: its admission inside `model.done`, its
//! retries, and its presentation inside `round.present+model.start`.

use std::sync::Arc;

use lash_durable::domain::{AdmittedId, RunRecordKind};
use lash_durable::{ActorTx, DomainWrite, DurableInstant};

use super::records::{PresentBody, RetryBody, append, encode, start_record};
use super::{
    AdmissionRefusal, AdmittedExecution, AdmittedRound, BodyOutput, PinnedWait, Presentation,
    RoundDraft, RunFold, SettleRefusal, admit, check_drafts,
};
use crate::runtime::actor::waits::{self, WaitKind, WaitSpec};

/// Admit a tool round inside the `model.done` transaction: its membership,
/// pinned policies, limits and wait deadlines, and an `x_start` for every
/// member. A member that may park gets its tool completion wait pinned in
/// the same transaction, revoked with `scope`, so its
/// key exists before its body can hand it out, and a rerun of the body is
/// handed the same key. No member's body starts before that transaction
/// commits.
///
/// # Errors
///
/// [`AdmissionRefusal`]; nothing is recorded.
pub fn admit_round(
    tx: &mut ActorTx,
    scope: &lash_durable::domain::ScopeKey,
    round: RoundDraft,
) -> Result<AdmittedRound, AdmissionRefusal> {
    check_drafts(&round.members)?;
    let mut members = Vec::with_capacity(round.members.len());
    for draft in round.members {
        let pinned = match draft.wait() {
            Some(deadline) => {
                let (wait, _) = waits::pin(
                    tx,
                    WaitSpec {
                        kind: WaitKind::ToolCompletion,
                        scope: scope.clone(),
                        target_process: None,
                        deadline: Some(deadline),
                    },
                )?;
                Some(PinnedWait { id: wait.id() })
            }
            None => None,
        };
        members.push(draft.with_pinned_wait(pinned));
    }
    let members = admit(tx, &round.owner, round.run, members)?;
    Ok(AdmittedRound::admitted(round.run, members))
}

/// Record a round's presentation inside the `round.present+model.start`
/// transaction, from its committed records, in declared order.
///
/// The presentation is a pure function of `fold`: each member's committed
/// final outcome, in the order the admission declared, whatever order the
/// outcomes committed in. It is recorded once, when every member has its
/// outcome; a presentation already recorded, or one of a round with an
/// unsettled member, records nothing.
pub fn present(tx: &mut ActorTx, round: &AdmittedRound, fold: &RunFold) -> Presentation {
    let (presentation, record) = presentation(round, fold);
    if let Some(record) = record {
        tx.write(record);
    }
    presentation
}

/// A round's presentation from its committed records, in declared order,
/// and the `present` record to commit with the transaction that hands it
/// on: none when it is already recorded or a member has no outcome yet.
/// What [`present`] writes; a phase runner that commits the record with a
/// later transaction of its own takes it from here.
#[must_use]
pub fn presentation(round: &AdmittedRound, fold: &RunFold) -> (Presentation, Option<DomainWrite>) {
    let Some(view) = fold.round(round.run()) else {
        return (
            Presentation::of(
                round
                    .members()
                    .iter()
                    .map(|member| (member.call().clone(), None))
                    .collect(),
            ),
            None,
        );
    };
    let entries: Vec<_> = view
        .members()
        .iter()
        .map(|member| (member.call().clone(), member.outcome().cloned()))
        .collect();
    let record = (view.presented().is_none() && view.settled()).then(|| {
        append(
            view.owner(),
            view.run(),
            view.cursor().take(),
            RunRecordKind::Present,
            None,
            encode(&PresentBody {
                calls: entries.iter().map(|(call, _)| call.clone()).collect(),
            }),
        )
    });
    (Presentation::of(entries), record)
}

/// Record that `failed`, an attempt of a `Repeatable` call, failed with
/// `output` in a way its pinned contract repeats, and that its next attempt
/// is due at `due`. The call stays open; its next attempt starts through
/// [`start_retry`] once the retry is due.
///
/// # Errors
///
/// [`SettleRefusal::NotRetryable`] when the pinned policy is `Once`, the
/// outcome may not repeat, or `failed` is the last attempt the policy
/// admits; nothing is recorded.
pub fn settle_retry(
    tx: &mut ActorTx,
    failed: &AdmittedExecution,
    output: BodyOutput,
    due: DurableInstant,
) -> Result<(), SettleRefusal> {
    if !failed
        .policy()
        .permits_repeat(failed.policy(), failed.attempt())
        || !output.outcome.may_repeat()
    {
        return Err(SettleRefusal::NotRetryable(failed.call().clone()));
    }
    let id = failed.id();
    tx.write(append(
        &id.owner,
        id.run,
        failed.cursor().take(),
        RunRecordKind::Retry,
        Some(failed.call()),
        encode(&RetryBody {
            start: id.ordinal.0,
            outcome: output.outcome,
            material: output.material,
            due_at_ms: due.0,
        }),
    ));
    Ok(())
}

/// Record the `x_start` of the attempt after `failed`, whose retry is due,
/// at its run's next ordinal: the new attempt's identity. Its body starts
/// only after this transaction commits.
pub fn start_retry(tx: &mut ActorTx, failed: &AdmittedExecution) -> AdmittedExecution {
    let id = failed.id();
    let ordinal = failed.cursor().take();
    let attempt = failed.attempt() + 1;
    tx.write(start_record(
        &id.owner,
        id.run,
        ordinal,
        failed.draft(),
        failed.member(),
        attempt,
    ));
    AdmittedExecution::admitted(
        AdmittedId {
            owner: id.owner.clone(),
            run: id.run,
            ordinal,
        },
        failed.draft().clone(),
        failed.member(),
        attempt,
        Arc::clone(failed.cursor()),
    )
}
