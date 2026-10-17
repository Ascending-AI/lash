//! A tool round on the primitive: its admission inside `model.done`, its
//! retries, and its presentation inside `round.present+model.start`.

use std::sync::Arc;

use lash_durable::domain::AdmittedId;
use lash_durable::{ActorTx, DurableInstant};

use super::records::{Record, RetryBody, append, start_record};
use super::{
    AdmissionRefusal, AdmittedExecution, AdmittedRound, PinnedWait, Presentation, RoundDraft,
    RunFold, SettleRefusal, check_drafts,
};
use crate::runtime::actor::waits;
use crate::runtime::actor::waits::WaitPurpose;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::waits::WaitDeadline;
use lash_execution_contracts::waits::WaitSpec;

/// Admit a tool round inside the `model.done` transaction: its membership,
/// pinned policies and limits, and an `x_start` for every member. A member
/// that may park gets its tool completion wait pinned in the same
/// transaction, revoked with `scope`, so its key exists before its body can
/// hand it out, and a rerun of the body is handed the same key. That wait's
/// row holds the park's deadline; the admission record names the wait and
/// repeats nothing of it. No member's body starts before that transaction
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
    pin_round(tx, scope, round)?.admit(tx, Vec::new())
}

/// A round whose members' completion waits are pinned in a transaction,
/// before its admission record: what a park that admits each member under
/// an Admission node of its own reads the pins from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedRound {
    owner: lash_durable::domain::OwnerKey,
    run: lash_durable::domain::RunSeq,
    members: Vec<super::ExecutionDraft>,
}

impl PinnedRound {
    /// Each member's ordinal, as its [`AdmittedId`](lash_durable::domain::AdmittedId)
    /// takes it, and the pin its admission records.
    #[must_use]
    pub fn pins(&self) -> Vec<(lash_durable::domain::Ordinal, super::graph::PinnedMember)> {
        (0_u64..)
            .zip(&self.members)
            .map(|(member, draft)| {
                (
                    super::member_ordinal(member),
                    super::graph::PinnedMember::of(draft),
                )
            })
            .collect()
    }

    /// Records the round's admission and its members' `x_start` rows in
    /// `tx`, naming each member by its Admission in `admissions` (or
    /// pinning it inline when that is empty).
    ///
    /// # Errors
    ///
    /// [`AdmissionRefusal::Admissions`] for a count that is not the
    /// members'.
    pub fn admit(
        self,
        tx: &mut ActorTx,
        admissions: Vec<lash_core_store::checkpoint_graph::NodeId>,
    ) -> Result<AdmittedRound, AdmissionRefusal> {
        let members = super::admit_by(tx, &self.owner, self.run, self.members, admissions)?;
        Ok(AdmittedRound::admitted(self.run, members))
    }
}

/// Pins `round`'s completion waits in `tx`; its admission is recorded by
/// [`PinnedRound::admit`].
///
/// # Errors
///
/// [`AdmissionRefusal`] for an empty round or one call twice.
pub fn pin_round(
    tx: &mut ActorTx,
    scope: &lash_durable::domain::ScopeKey,
    round: RoundDraft,
) -> Result<PinnedRound, AdmissionRefusal> {
    check_drafts(&round.members)?;
    let mut members = Vec::with_capacity(round.members.len());
    for draft in round.members {
        let pinned = match draft.park() {
            Some(park) => {
                let (wait, _) = waits::pin(
                    tx,
                    WaitSpec {
                        scope: scope.clone(),
                        purpose: WaitPurpose::ToolCompletion {
                            call: draft.call().clone(),
                            tool: draft.tool().clone(),
                            deadline: park.deadline().map(WaitDeadline::at),
                        },
                    },
                );
                Some(PinnedWait { id: wait.id() })
            }
            None => None,
        };
        members.push(draft.with_pinned_wait(pinned));
    }
    Ok(PinnedRound {
        owner: round.owner,
        run: round.run,
        members,
    })
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
        record.write(tx);
    }
    presentation
}

/// A round's `present` record, to commit with the transaction that hands
/// its presentation on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentRecord {
    owner: lash_durable::domain::OwnerKey,
    run: lash_durable::domain::RunSeq,
    ordinal: lash_durable::domain::Ordinal,
}

impl PresentRecord {
    /// Writes the record into `tx`.
    pub fn write(&self, tx: &mut ActorTx) {
        append(
            tx,
            &self.owner,
            self.run,
            self.ordinal,
            None,
            &Record::Present,
        );
    }
}

/// A round's presentation from its committed records, in declared order,
/// and the `present` record to commit with the transaction that hands it
/// on: none when it is already recorded or a member has no outcome yet.
/// What [`present`] writes; a phase runner that commits the record with a
/// later transaction of its own takes it from here.
#[must_use]
pub fn presentation(
    round: &AdmittedRound,
    fold: &RunFold,
) -> (Presentation, Option<PresentRecord>) {
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
    let record = (view.presented().is_none() && view.settled()).then(|| PresentRecord {
        owner: view.owner().clone(),
        run: view.run(),
        ordinal: view.cursor().take(),
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
    output: SettledOutput,
    due: DurableInstant,
) -> Result<(), SettleRefusal> {
    if !failed
        .policy()
        .permits_repeat(failed.policy(), failed.attempt())
        || !output.may_repeat()
    {
        return Err(SettleRefusal::NotRetryable(failed.call().clone()));
    }
    let id = failed.id();
    append(
        tx,
        &id.owner,
        id.run,
        failed.cursor().take(),
        Some(failed.call()),
        &Record::Retry(RetryBody {
            start: id.ordinal.0,
            output,
            due_at_ms: due.0,
        }),
    );
    Ok(())
}

/// Record the `x_start` of the attempt after `failed`, whose retry is due,
/// at its run's next ordinal: the new attempt's identity. Its body starts
/// only after this transaction commits.
pub fn start_retry(tx: &mut ActorTx, failed: &AdmittedExecution) -> AdmittedExecution {
    let id = failed.id();
    let ordinal = failed.cursor().take();
    let attempt = failed.attempt() + 1;
    start_record(
        tx,
        &id.owner,
        id.run,
        ordinal,
        failed.draft(),
        failed.member(),
        attempt,
    );
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
