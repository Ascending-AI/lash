//! A turn's tool round on the durable path (ADR 0132 §5). Owned by L4
//! (FIG-5174); the phase runner (L3) calls it from its tool arm and commits
//! the presentation it carries.
//!
//! - **Admission.** A round the rows do not hold yet is admitted in the
//!   `model.done` transaction: the turn's `Tools` phase with the checkpoint
//!   that re-delivers the round's calls, and `round::admit_round` with an
//!   `x_start` for every member. No body runs before that commit, so a model
//!   stream that never committed can never launch a tool.
//! - **Refusal.** A round whose admission the catalog refuses is admitted
//!   with every member settled on its typed refusal in the same commit: no
//!   body runs, and a resume answers from those outcomes.
//! - **Resume.** A round the rows hold is resumed from its fold: the
//!   re-delivered calls must be the ones it admitted, a started `Once`
//!   without an outcome is `Interrupted`, a started `Repeatable` reruns at
//!   its ordinal, and nothing it recorded runs again.
//! - **Cancel.** An `Immediate` cancel the turn accepted before the round's
//!   admission admits none of it, and one it accepts while the round runs
//!   ends its unfinished members `Cancelled`; the turn then ends as every
//!   phase does on a cancel ([`RoundExit::CancelRequested`]). An `AfterStep`
//!   request lets the round run to its end.
//! - **Suspension.** A round whose unsettled members are all parked on a
//!   wait or waiting out a retry's backoff runs nothing: once it stayed hot
//!   for `idle_evict` it suspends ([`RoundExit::Suspended`]), and the
//!   session releases as `waiting` until the earliest due, holding no claim
//!   slot. The wait's resolution, or the due time, re-claims it, and the
//!   restored turn resumes the round from its fold.
//! - **Presentation.** The machine is answered with each member's committed
//!   outcome, in declared order. The round's `present` record rides the
//!   turn's next commit: `round.present+model.start` when the machine calls
//!   the model next, `turn.commit` when it finishes.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::{
    self, AdmittedRound, RoundCalls, RoundDraft, RoundEnd, RoundError, RoundRunner, RoundTools,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_durable::domain::{DomainWrite, OwnerKey, RunSeq, TurnWrite};
use lash_durable::{CommitLabel, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::session::{TurnDrive, TurnError, TurnRow, UnfinishedPhase};
use super::turn_cancel;
use crate::sansio::PendingToolCall;
use crate::{ActorContext, EffectId, Response};

/// How a tool round left the turn.
#[derive(Debug)]
pub(super) enum RoundExit {
    /// The machine is answered. The round's presentation commits with the
    /// turn's next commit.
    Answered(Option<DomainWrite>),
    /// The turn accepted a cancel: the round's unfinished members settled
    /// `Cancelled`, and the next pass finalizes the turn.
    CancelRequested,
    /// Nothing in the round runs and only parked waits or retry dues remain:
    /// the session releases as `waiting` until `due`.
    Suspended {
        /// The earliest retry due time or parked wait deadline.
        due: Option<DurableInstant>,
    },
}

/// The label of the turn's next model call: `round.present+model.start` when
/// it carries a round's presentation.
pub(super) fn model_start_label(carry: &Option<DomainWrite>) -> CommitLabel {
    if carry.is_some() {
        CommitLabel::ROUND_PRESENT_MODEL_START
    } else {
        CommitLabel::MODEL_START
    }
}

fn exec(error: impl std::fmt::Display) -> TurnError {
    TurnError::Exec(error.to_string())
}

fn round_error(error: RoundError) -> TurnError {
    match error {
        RoundError::Durable(error) => TurnError::Durable(error),
        RoundError::Stopped => TurnError::Exec("the activation stopped during a round".to_owned()),
        other => exec(other),
    }
}

/// Run the tool round of effect `id` over `calls` to its members' outcomes,
/// admitting it in `model.done` unless the rows already hold it, and answer
/// the machine. `checkpoint` is the turn's checkpoint, re-delivering this
/// round, that `model.done` commits.
///
/// # Errors
///
/// [`TurnError`]: ownership lost, or the round's rows or calls refused.
pub(super) async fn run(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    row: &TurnRow,
    id: EffectId,
    calls: Vec<PendingToolCall>,
    checkpoint: String,
    iteration: u32,
) -> Result<RoundExit, TurnError> {
    let session = row.session.clone();
    let owner = OwnerKey::Turn(session.clone(), row.run.clone());
    let opener = EffectOpener::turn(session.clone(), row.run.clone());
    let run = RunSeq(id.0);
    let tools: Arc<dyn RoundTools> = drive.tools()?;
    let policies = tools.policies();
    if calls.is_empty() {
        drive.machine().handle_response(Response::ToolResults {
            id,
            results: Vec::new(),
        });
        return Ok(RoundExit::Answered(None));
    }
    let rows: Vec<_> = cx
        .durable_reads()?
        .run_records(&owner)
        .await?
        .into_iter()
        .filter(|stored| stored.run == run)
        .collect();
    let folded = round::fold(&rows, &policies).map_err(exec)?;
    let bodies = Arc::new(RoundCalls::new(Arc::clone(&tools), &calls));
    let runner = match folded.round(run) {
        Some(view) => {
            let drafts: Vec<_> = view.members().iter().map(|member| member.draft()).collect();
            round::require_admitted(&opener, run, &drafts, &calls).map_err(exec)?;
            RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
        }
        None => {
            // An `AfterStep` request lets the step's round run; the turn
            // honours it before its next model call.
            if turn_cancel::immediate(cx, &session).await? {
                return Ok(RoundExit::CancelRequested);
            }
            let now_ms = u64::try_from(cx.durable_now().await?.0).unwrap_or(0);
            let members = calls
                .iter()
                .map(|call| round::call_draft(&opener, call, tools.pin(call, now_ms)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(exec)?;
            let refused = tools.refusal(&calls);
            let mut tx = cx.begin().await?;
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session.clone(),
                run: row.run.clone(),
                phase: UnfinishedPhase::Tools { run, checkpoint },
                iteration,
            }));
            let admitted: AdmittedRound = round::admit_round(
                &mut tx,
                &lash_core_execution::runtime::actor::waits::wait_scope(cx),
                RoundDraft {
                    owner: owner.clone(),
                    run,
                    members,
                },
            )
            .map_err(exec)?;
            if let Some(refused) = &refused {
                for (member, answer) in admitted.members().iter().zip(refused) {
                    let output = round::completed_material(&opener, answer).map_err(exec)?;
                    round::settle(
                        &mut tx,
                        member,
                        round::SettledOutput::Completed(output),
                        Vec::new(),
                    )
                    .map_err(exec)?;
                }
            }
            cx.commit(tx, CommitLabel::MODEL_DONE).await?;
            if refused.is_some() {
                RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
            } else {
                RoundRunner::admitted(cx, &admitted, policies, bodies)
                    .ok_or_else(|| exec("an admitted round has no members"))?
            }
        }
    };

    // Run the round, watching the turn's row for a cancel whenever mail
    // may have arrived: an accepted `Immediate` request cancels the
    // unfinished members, which then settle.
    let cancel = CancellationToken::new();
    let running = runner.cancelled_by(cancel.clone()).run();
    tokio::pin!(running);
    let end = loop {
        tokio::select! {
            biased;
            end = &mut running => break end.map_err(round_error)?,
            () = cx.wait_for_mail(), if !cancel.is_cancelled() => {
                if turn_cancel::immediate(cx, &session).await? {
                    cancel.cancel();
                }
            }
        }
    };
    if cancel.is_cancelled() {
        return Ok(RoundExit::CancelRequested);
    }
    let end = match end {
        RoundEnd::Settled(end) => end,
        RoundEnd::Suspended { due } => return Ok(RoundExit::Suspended { due }),
    };

    let view = end.round();
    let results = calls
        .iter()
        .zip(view.members())
        .map(|(call, member)| {
            let output = member
                .outcome()
                .ok_or_else(|| exec(format!("call {} ended without an outcome", call.call_id)))?;
            Ok(tools.completed(call, output))
        })
        .collect::<Result<Vec<_>, TurnError>>()?;
    let admitted = end
        .fold()
        .admitted_round(run)
        .ok_or_else(|| exec(DurableError::Store(missing_round(run))))?;
    let carry = round::presentation(&admitted, end.fold()).1;
    drive
        .machine()
        .handle_response(Response::ToolResults { id, results });
    Ok(RoundExit::Answered(carry))
}

fn missing_round(run: RunSeq) -> lash_durable::StoreFailure {
    lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: format!("the settled round {run:?} is not in its fold"),
    }
}

/// Settle the open members of `row`'s round `Cancelled` on `tx`, when the
/// turn is in its `Tools` phase: what a turn cancel leaves of a round whose
/// owner stopped, or crashed, before its members settled.
///
/// # Errors
///
/// [`TurnError`] when the round's rows cannot be read or do not fold.
pub(super) async fn cancel_open_round(
    cx: &ActorContext,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
) -> Result<(), TurnError> {
    let UnfinishedPhase::Tools { run, .. } = row.phase else {
        return Ok(());
    };
    let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
    round::settle_cancelled(cx, tx, &owner, run)
        .await
        .map_err(round_error)
}
