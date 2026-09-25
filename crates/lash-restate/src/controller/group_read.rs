//! The cursorless rank read the §6 incorporation record is built on (ADR 0099
//! §8, FIG-3411 phase 2c).
//!
//! Split out of `mod.rs` only for the production file-size budget: this is the
//! body of
//! [`RuntimeEffectController::read_group_settlement`](lash_core::RuntimeEffectController::read_group_settlement)
//! for the handler-context controller — the same index `read_rank` + payload
//! `get` pair the consuming await uses, minus the cursor, the wait and the
//! position map. The rank read names the settled child by its retained replay
//! key, which is the identity `SettlementSource::GroupRank` records.

use lash_core::{RankedGroupSettlement, RuntimeEffectControllerError};

use crate::effect_group::{
    EffectGroupPayloadGetResponse, EffectGroupReadRankRequest, EffectGroupReadRankResponse,
    EffectGroupSettlementTerminal, group_shape_error, payload_key, settlement_from_payload,
};

use super::{RestateControllerContext, effect_group_engine_error};

/// Serves `group_key` rank `rank` without touching any caller cursor: `Some`
/// for a settled rank, `None` when the rank holds no settlement — including a
/// rank a close cancelled, which the index reports through the same
/// `NotSettled`/`Closed` pair.
pub(super) async fn read_group_settlement<'ctx, C>(
    context: &C,
    group_key: &str,
    rank: u64,
) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx>,
{
    let read = context
        .effect_group_read_rank(
            group_key.to_string(),
            EffectGroupReadRankRequest {
                rank,
                for_caller: false,
            },
        )
        .await
        .map_err(|error| effect_group_engine_error("EffectGroupState/read_rank", error))?;
    let (record, child_replay_key) = match read {
        EffectGroupReadRankResponse::Settled {
            settlement,
            child_replay_key,
        } => (settlement, child_replay_key),
        EffectGroupReadRankResponse::NotSettled | EffectGroupReadRankResponse::Closed => {
            return Ok(None);
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
    let payload = if matches!(
        record.terminal,
        EffectGroupSettlementTerminal::StoredPayload
    ) {
        match context
            .effect_group_payload_get(payload_key(group_key, record.position))
            .await
            .map_err(|error| effect_group_engine_error("EffectGroupPayload/get", error))?
        {
            EffectGroupPayloadGetResponse::Stored { bytes } => Some(bytes),
            EffectGroupPayloadGetResponse::Missing => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} rank {rank} refers to a missing payload"
                )));
            }
            EffectGroupPayloadGetResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} payload was retired"
                )));
            }
        }
    } else {
        None
    };
    let settlement = settlement_from_payload(record, payload)?;
    Ok(Some(RankedGroupSettlement {
        sequence: settlement.sequence,
        child_replay_key,
        outcome: settlement.outcome,
    }))
}
