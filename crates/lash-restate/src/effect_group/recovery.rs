//! The re-send of committed children whose seat is still owed (ADR 0099 §5,
//! W7; FIG-4454).
//!
//! A child's committed final is the group's obligation of record: the index
//! retains it, beside the rank its commit reserved, until the child seats.
//! Its drain has one authority, the child's own driver, running in the
//! child's invocation. When that invocation ends before the seat — an
//! operator's kill, a terminal protocol error — nothing else schedules the
//! drain, so the index re-sends the child whenever its opener reopens the
//! group, and whenever a dispatcher is started for a group that is already
//! ready or closed.
//!
//! The re-send is the dispatch's own child call again: the retained
//! membership's envelope, the recorded shape, the child's replay key as its
//! idempotency key, on the group's recorded lane. While the committing
//! invocation is retained the key attaches to it, and nothing new runs; once
//! its retention has expired the key mints a successor, which the index
//! answers `AttachExpired`, and which drains the retained final and seats it
//! at its reserved rank. No second drainer ever exists for one final.

use super::*;

/// Re-sends every committed child of `record` whose seat is owed, on the
/// group's recorded dispatch lane. A group owing none sends nothing.
pub(super) async fn resend_owed_children(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    record: &EffectGroupStateRecord,
) -> Result<(), TerminalError> {
    let group_key = ctx.key().to_string();
    let live = record.live()?;
    let owed = owed_positions(live);
    if owed.is_empty() {
        return Ok(());
    }
    let route = namespace.parse(&record.dispatch_route).ok_or_else(|| {
        TerminalError::new(format!(
            "effect group {group_key} recorded dispatch route `{}`, which names no lash \
             service of this namespace",
            record.dispatch_route
        ))
    })?;
    let membership = load_membership(ctx).await?;
    for position in owed {
        let replay_key = live.shape.member_replay_key(position)?.to_string();
        let request = dispatch::EffectGroupChildRequest {
            group_key: group_key.clone(),
            shape: live.shape.clone(),
            position,
            envelope: membership.envelope(&group_key, position)?,
        };
        // Fire and forget: the child records its own settlement in the
        // index, and a re-send attached to a retained invocation has nothing
        // to report.
        let _sent = crate::services::routed_workflow::<_, _, ()>(
            ctx,
            &route,
            group_key.clone(),
            "child",
            request,
        )
        .idempotency_key(replay_key.clone())
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
        .send();
    }
    Ok(())
}

/// The positions whose §4 point holds a committed final and whose rank is not
/// seated yet, in position order.
fn owed_positions(live: &EffectGroupStateLiveRecord) -> Vec<usize> {
    let mut owed = live
        .owed()
        .map(|(_, position)| position)
        .collect::<Vec<_>>();
    owed.sort_unstable();
    owed
}

/// The object-state key an index's record lives under, for an operator's
/// scan of every group (FIG-4454).
pub(crate) const INDEX_RECORD_KEY: &str = INDEX_STATE_KEY;

/// How many committed children of the group whose retained index record is
/// `raw` owe their seat on a lane of `generation` (FIG-4454): the retirement
/// evidence a generation's drain waits for. A retired group, and a group on
/// another lane, owe none there.
pub(crate) fn undrained_children_on(
    group_key: &str,
    raw: serde_json::Value,
    generation: &lash_core::engine::BuildGeneration,
) -> Result<u64, TerminalError> {
    let record: EffectGroupStateRecord =
        object_state::decode_stamped_value(group_key, raw, &protocol::EFFECT_GROUP_STATE_FORMATS)?;
    if crate::services::generation_lane_of(&record.dispatch_route).as_ref() != Some(generation) {
        return Ok(0);
    }
    Ok(match &record.lifecycle {
        EffectGroupLifecycle::Retired { .. } => 0,
        EffectGroupLifecycle::Preparing { live, .. }
        | EffectGroupLifecycle::Ready { live, .. }
        | EffectGroupLifecycle::Closed { live, .. } => owed_positions(live).len() as u64,
    })
}
