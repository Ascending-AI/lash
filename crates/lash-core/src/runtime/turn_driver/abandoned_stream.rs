//! What a re-sent model call's earlier attempts streamed (ADR 0132 §4).
//!
//! A takeover re-sends a pinned call as its next attempt. The provider
//! streams it afresh, so its text need not repeat what an abandoned attempt
//! streamed: the live stream retracts the earlier attempts' prose and
//! reasoning with one `ModelAttemptReset`, read back from the session's live
//! replay, and the re-sent attempt streams under an observation key of its
//! own, which the store never takes for a redelivery of an earlier
//! attempt's activity (FIG-5098).

use std::sync::Arc;

use crate::{SessionObservationEvent, SessionObservationEventPayload, TurnActivityId, TurnEvent};

/// The observation key attempt `attempt` of a model call streams under,
/// `base` being the call's effect replay key: `{base}:stream` for the first
/// attempt, `{base}:stream@{attempt}` for a re-sent one.
pub(super) fn model_stream_key(base: &str, attempt: u32) -> String {
    if attempt <= 1 {
        format!("{base}:stream")
    } else {
        format!("{base}:stream@{attempt}")
    }
}

/// The reset that retracts what the attempts of the call whose effect replay
/// key is `base` streamed in `events`, the turn's live replay from before
/// its activity began, in position order.
///
/// `None` when `events` do not prove they hold all of it: no marker of the
/// turn (`TurnStarted` or a `CheckpointRecorded`) precedes the call's first
/// streamed activity, so retention may have dropped some, or the turn's
/// activity is not there at all. Everything after a retained marker is
/// retained, and a call's stream follows the turn's markers.
pub(super) fn attempt_reset(
    events: &[Arc<SessionObservationEvent>],
    turn: &crate::TurnId,
    base: &str,
) -> Option<TurnEvent> {
    let first = model_stream_key(base, 1);
    let resent = format!("{first}@");
    let mut marked = false;
    let mut prose: Vec<TurnActivityId> = Vec::new();
    let mut reasoning: Vec<TurnActivityId> = Vec::new();
    for event in events {
        let SessionObservationEventPayload::TurnActivity(activity) = &event.payload else {
            continue;
        };
        if event.turn_id.as_ref() != Some(turn) {
            continue;
        }
        let streamed = activity
            .id
            .observed_span()
            .is_some_and(|(key, _)| key == first || key.starts_with(&resent));
        if !streamed {
            marked |= matches!(
                activity.event,
                TurnEvent::TurnStarted { .. } | TurnEvent::CheckpointRecorded { .. }
            );
            continue;
        }
        if !marked {
            return None;
        }
        let retracted = match &activity.event {
            TurnEvent::AssistantProseDelta { .. } => &mut prose,
            TurnEvent::ReasoningDelta { .. } => &mut reasoning,
            _ => continue,
        };
        if !retracted.contains(&activity.correlation_id) {
            retracted.push(activity.correlation_id.clone());
        }
    }
    marked.then_some(TurnEvent::ModelAttemptReset {
        assistant_prose_correlation_ids: prose,
        reasoning_correlation_ids: reasoning,
    })
}
