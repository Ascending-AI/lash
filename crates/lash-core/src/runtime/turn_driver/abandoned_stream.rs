//! What a re-sent model call's earlier attempts streamed (ADR 0132 §4).
//!
//! A takeover re-sends a pinned call as its next attempt. The provider
//! streams it afresh, so its text need not repeat what an abandoned attempt
//! streamed: the live stream retracts the earlier attempts' prose and
//! reasoning with one `ModelAttemptReset`, read back from the session's live
//! replay after the cursor the call pinned before its first attempt
//! streamed (FIG-5399), and the re-sent attempt streams under an observation
//! key of its own, which the store never takes for a redelivery of an
//! earlier attempt's activity (FIG-5098).
//!
//! An attempt's blocks are named by its stream key whether they streamed or
//! not: a derivation publishes the text an answer did not stream as whole
//! blocks under that key too. A receipt held for the turn's commit derives
//! before anything durable holds it (FIG-6955), so a re-sent call retracts
//! those blocks with the streamed ones (FIG-6969).

use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
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
/// key is `base` published in `events`, streamed or whole: the session's
/// live replay after the cursor the call pinned before its first attempt
/// streamed, which the replay holds whole, in position order (FIG-5399).
pub(super) fn attempt_reset(
    events: &[Arc<SessionObservationEvent>],
    turn: &lash_vocabulary::identity::TurnId,
    base: &str,
) -> TurnEvent {
    let first = model_stream_key(base, 1);
    let mut prose: Vec<TurnActivityId> = Vec::new();
    let mut reasoning: Vec<TurnActivityId> = Vec::new();
    for event in events {
        let SessionObservationEventPayload::TurnActivity(activity) = &event.payload else {
            continue;
        };
        if event.turn_id.as_ref() != Some(turn) {
            continue;
        }
        // `{key}/{block}`, the key `{first}` or `{first}@{attempt}`.
        let attempted = activity
            .correlation_id
            .0
            .strip_prefix(first.as_str())
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('@'));
        if !attempted {
            continue;
        }
        let retracted = match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                ..
            }) => &mut prose,
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                ..
            }) => &mut reasoning,
            _ => continue,
        };
        if !retracted.contains(&activity.correlation_id) {
            retracted.push(activity.correlation_id.clone());
        }
    }
    TurnEvent::ModelAttemptReset {
        assistant_prose_correlation_ids: prose,
        reasoning_correlation_ids: reasoning,
    }
}
