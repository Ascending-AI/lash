//! The run a caller's rank read serves (FIG-4088): every rank seated
//! consecutively from the one asked for, each with its stored payload.
//!
//! A reader that took one rank per call journaled a read and a payload get per
//! rank, and on an engine that replays the journal at every resumption each
//! suspension replayed the reader's whole journal, which grows with the group:
//! quadratic in the width. The index serves the run from one call instead. The
//! payload gets it issues run inside this shared handler, whose own journal
//! holds only them, so a suspension between them replays almost nothing.

use super::*;

pub(super) async fn served_run(
    ctx: &SharedObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    group_key: &str,
    live: &EffectGroupStateLiveRecord,
    from: u64,
) -> Result<Vec<EffectGroupServedRank>, TerminalError> {
    // Seated ranks are immutable, so the run is the same facts a read of
    // each rank would serve.
    let records = (from..)
        .map_while(|rank| live.settlements.get(&rank).cloned())
        .collect::<Vec<_>>();
    // Every get is issued before any is awaited, in rank order, so the
    // journal is the same on every replay.
    let gets = records
        .iter()
        .map(|record| {
            matches!(
                record.terminal,
                EffectGroupSettlementTerminal::StoredPayload
            )
            .then(|| {
                namespace
                    .effect_group_payload(ctx, payload_key(group_key, record.position))
                    .get()
                    .call()
            })
        })
        .collect::<Vec<_>>();
    let mut ranks = Vec::with_capacity(records.len());
    for (settlement, get) in records.into_iter().zip(gets) {
        let payload = match get {
            Some(get) => Some(get.await?.into_body()),
            None => None,
        };
        let child_replay_key = live
            .shape
            .member_replay_key(settlement.position)?
            .to_string();
        ranks.push(EffectGroupServedRank {
            settlement,
            child_replay_key,
            payload,
        });
    }
    Ok(ranks)
}
