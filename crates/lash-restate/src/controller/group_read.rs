//! The handler-context controller's rank reads: the consuming await behind
//! [`RuntimeEffectController::await_next_settlement`](lash_core::RuntimeEffectController::await_next_settlement)
//! and the cursorless read the §6 incorporation record is built on,
//! [`RuntimeEffectController::read_group_settlement`](lash_core::RuntimeEffectController::read_group_settlement)
//! (ADR 0099 §8, FIG-3411 phase 2c). Split out of `mod.rs` for the production
//! file-size budget. The cursorless read names the settled child by its
//! retained replay key, which is the identity `SettlementSource::GroupRank`
//! records.
//!
//! Both read by the run (FIG-4088). A read of a seated rank asks the index for
//! every rank seated consecutively from it, payloads included, in one call.
//! Reading one rank per call cost a read and a payload get per rank, and on an
//! engine that replays the journal at every resumption, each of those
//! suspensions replayed the opener's whole journal, which grows with the group:
//! quadratic in the width. What a run served is kept by the controller, so the
//! consuming await takes the ranks after its cursor without another call, and
//! the incorporation reads the ranks the await consumed without reading them
//! again. The kept ranks come only from journaled answers, so a replay keeps
//! exactly the same ones and serves the same ranks from them.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use lash_core::{
    EffectGroupHandle, ExecutionScope, GroupSettlement, RankedGroupSettlement,
    RuntimeEffectControllerError, RuntimeErrorCode,
};

use crate::durable_wait::RestateTurnCancelRaceOutcome;
use crate::effect_group::{
    EffectGroupPayloadGetResponse, EffectGroupReadRankRequest, EffectGroupReadRankResponse,
    EffectGroupServedRank, EffectGroupWaitResolution, decode_wait_resolution, group_shape_error,
    rank_wait_request, settlement_from_payload,
};

use super::{
    RestateControllerContext, RestateRuntimeEffectController, effect_group_engine_error,
    restate_group_turn_cancel_wait_request,
};

/// The ranks this controller's run reads served, by group.
///
/// `seen` holds every one of them for the cursorless read, which sees every
/// recorded rank: a seated rank is immutable, so a kept copy answers what the
/// index would. `ahead` holds the ones past the consuming await's cursor that
/// it has not taken yet. A caller's read is refused a closed group, so a close
/// drops `ahead` and the await's next read goes to the index.
#[derive(Default)]
pub(super) struct GroupReadAhead {
    groups: Mutex<BTreeMap<String, KeptRanks>>,
}

#[derive(Default)]
struct KeptRanks {
    seen: BTreeMap<u64, EffectGroupServedRank>,
    ahead: BTreeMap<u64, EffectGroupServedRank>,
}

impl GroupReadAhead {
    fn with<R>(&self, group_key: &str, body: impl FnOnce(&mut KeptRanks) -> R) -> R {
        let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        body(groups.entry(group_key.to_owned()).or_default())
    }

    /// Keeps a run read from `first_rank`; `ahead` keeps the ranks after the
    /// first when the consuming await read it.
    fn keep(&self, group_key: &str, first_rank: u64, run: &[EffectGroupServedRank], ahead: bool) {
        self.with(group_key, |kept| {
            for (rank, served) in (first_rank..).zip(run) {
                kept.seen.insert(rank, served.clone());
                if ahead && rank > first_rank {
                    kept.ahead.insert(rank, served.clone());
                }
            }
        });
    }

    /// The group's next rank for the consuming await, taken from `ahead`.
    fn take_ahead(&self, group_key: &str, rank: u64) -> Option<EffectGroupServedRank> {
        self.with(group_key, |kept| kept.ahead.remove(&rank))
    }

    fn seen(&self, group_key: &str, rank: u64) -> Option<EffectGroupServedRank> {
        self.with(group_key, |kept| kept.seen.get(&rank).cloned())
    }

    /// A close: the consuming await's next read of `group_key` goes to the
    /// index.
    pub(super) fn closed(&self, group_key: &str) {
        self.with(group_key, |kept| kept.ahead.clear());
    }

    /// An open, or a reopen: nothing is kept for `group_key`.
    pub(super) fn opened(&self, group_key: &str) {
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(group_key);
    }
}

/// The payload bytes of a served rank, or the typed failure a missing or
/// retired payload is.
fn served_payload(
    group_key: &str,
    rank: u64,
    payload: Option<EffectGroupPayloadGetResponse>,
) -> Result<Option<Vec<u8>>, RuntimeEffectControllerError> {
    match payload {
        None => Ok(None),
        Some(EffectGroupPayloadGetResponse::Stored { bytes }) => Ok(Some(bytes)),
        Some(EffectGroupPayloadGetResponse::Missing) => Err(group_shape_error(format!(
            "effect group {group_key} rank {rank} refers to a missing payload"
        ))),
        Some(EffectGroupPayloadGetResponse::Retired) => Err(group_shape_error(format!(
            "effect group {group_key} payload was retired"
        ))),
    }
}

/// Serves `group_key` rank `rank` without touching any caller cursor: `Some`
/// for a settled rank, `None` when the rank holds no settlement — including a
/// rank a close cancelled, which the index reports through the same
/// `NotSettled`/`Closed` pair.
pub(super) async fn read_group_settlement<'ctx, C>(
    controller: &RestateRuntimeEffectController<'ctx, C>,
    group_key: &str,
    rank: u64,
) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let served = match controller.read_ahead.seen(group_key, rank) {
        Some(served) => served,
        None => {
            let read = controller
                .context
                .effect_group_read_rank(
                    &controller.namespace,
                    group_key.to_string(),
                    EffectGroupReadRankRequest {
                        rank,
                        for_caller: false,
                        run: true,
                    },
                )
                .await
                .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
            let run = match read {
                EffectGroupReadRankResponse::SettledRun { ranks } => ranks,
                EffectGroupReadRankResponse::NotSettled | EffectGroupReadRankResponse::Closed => {
                    return Ok(None);
                }
                EffectGroupReadRankResponse::Settled { .. } => {
                    return Err(group_shape_error(format!(
                        "effect group {group_key} answered a run read of rank {rank} with one rank"
                    )));
                }
                EffectGroupReadRankResponse::UnknownGroup => {
                    return Err(group_shape_error(format!(
                        "effect group {group_key} is unknown"
                    )));
                }
                EffectGroupReadRankResponse::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {group_key} is retired"
                    )));
                }
            };
            controller.read_ahead.keep(group_key, rank, &run, false);
            first_of_run(group_key, rank, run)?
        }
    };
    let payload = served_payload(group_key, rank, served.payload)?;
    let settlement = settlement_from_payload(served.settlement, payload)?;
    Ok(Some(RankedGroupSettlement {
        sequence: settlement.sequence,
        child_replay_key: served.child_replay_key,
        outcome: settlement.outcome,
    }))
}

fn first_of_run(
    group_key: &str,
    rank: u64,
    run: Vec<EffectGroupServedRank>,
) -> Result<EffectGroupServedRank, RuntimeEffectControllerError> {
    run.into_iter().next().ok_or_else(|| {
        group_shape_error(format!(
            "effect group {group_key} answered rank {rank} with an empty run"
        ))
    })
}

/// The consuming await: the settlement at the handle's cursor, from what an
/// earlier run read served ahead or from a fresh read that parks on the rank's
/// wait until it is seated.
pub(super) async fn await_next_settlement<'ctx, C>(
    controller: &RestateRuntimeEffectController<'ctx, C>,
    handle: &mut EffectGroupHandle,
    cancel: lash_core::TurnCancelWait,
) -> Result<GroupSettlement, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    if handle.is_exhausted() {
        return Err(group_shape_error(format!(
            "effect group {} has no settlement after its {} children",
            handle.group_key(),
            handle.children()
        )));
    }
    let rank = u64::try_from(handle.consumed() + 1).map_err(|error| {
        group_shape_error(format!("effect group rank does not fit u64: {error}"))
    })?;
    let group_key = handle.group_key().to_string();
    let served = match controller.read_ahead.take_ahead(&group_key, rank) {
        Some(served) => served,
        None => read_from_rank(controller, &group_key, rank, cancel).await?,
    };
    let payload = served_payload(&group_key, rank, served.payload)?;
    let settlement = settlement_from_payload(served.settlement, payload)?;
    handle.advance()?;
    Ok(settlement)
}

/// Reads the run from `rank` for the caller, parking on the rank's wait when
/// it is not yet seated. `rank` is returned; the ranks after it are kept
/// ahead.
async fn read_from_rank<'ctx, C>(
    controller: &RestateRuntimeEffectController<'ctx, C>,
    group_key: &str,
    rank: u64,
    cancel: lash_core::TurnCancelWait,
) -> Result<EffectGroupServedRank, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let read_run = || {
        controller.context.effect_group_read_rank(
            &controller.namespace,
            group_key.to_string(),
            EffectGroupReadRankRequest {
                rank,
                for_caller: true,
                run: true,
            },
        )
    };
    let mut read = read_run()
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
    if matches!(read, EffectGroupReadRankResponse::NotSettled) {
        let scope = ExecutionScope::runtime_operation(group_key);
        let request = rank_wait_request(&scope, group_key, rank)?;
        // A turn-observing rank wait races the turn's durable cancellation
        // gate, and a process drive's rank wait that observes no turn races
        // the segment's durable cancel promise; never a live token. The
        // journal records which completed first (FIG-3672 P9, FIG-3673).
        let turn_cancel =
            restate_group_turn_cancel_wait_request(&controller.authority_id, &cancel)?;
        let resolution = match controller
            .context
            .await_effect_group_wait(
                &controller.namespace,
                request,
                group_key.to_string(),
                turn_cancel,
                controller.options.process_cancel,
            )
            .await
            .map_err(|error| {
                effect_group_engine_error("LashDurableWaitWorkflow/await_resolution(RANK)", error)
            })? {
            RestateTurnCancelRaceOutcome::Completed(resolution) => resolution,
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::ProcessCancelled => {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!("awaiting effect group {group_key} rank {rank} was cancelled"),
                ));
            }
            RestateTurnCancelRaceOutcome::SessionRevoked { session_id } => {
                return Err(RuntimeEffectControllerError::from(
                    lash_core::StoreError::SessionDeleted { session_id },
                ));
            }
        };
        match decode_wait_resolution(resolution)? {
            EffectGroupWaitResolution::Rank => {}
            EffectGroupWaitResolution::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} was retired while awaiting rank {rank}"
                )));
            }
            other => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} rank {rank} wait resolved as {other:?}"
                )));
            }
        }
        read = read_run()
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupIndex/read_rank", error))?;
    }
    let run = match read {
        EffectGroupReadRankResponse::SettledRun { ranks } => ranks,
        EffectGroupReadRankResponse::Settled { .. } => {
            return Err(group_shape_error(format!(
                "effect group {group_key} answered a run read of rank {rank} with one rank"
            )));
        }
        EffectGroupReadRankResponse::NotSettled => {
            return Err(group_shape_error(format!(
                "effect group {group_key} rank {rank} remained unsettled after its notification"
            )));
        }
        EffectGroupReadRankResponse::Closed => {
            return Err(group_shape_error(format!(
                "effect group {group_key} is closed to this caller"
            )));
        }
        EffectGroupReadRankResponse::UnknownGroup => {
            return Err(group_shape_error(format!(
                "effect group {group_key} is unknown"
            )));
        }
        EffectGroupReadRankResponse::Retired => {
            return Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            )));
        }
    };
    controller.read_ahead.keep(group_key, rank, &run, true);
    first_of_run(group_key, rank, run)
}
