//! Semantic identity checks within the retained window and a publication batch.
use super::activity_spans::Claim;
use super::*;

pub(super) fn filter(
    buffer: &LiveReplaySessionBuffer,
    drafts: Vec<LiveReplayEventDraft>,
    session_id: &SessionId,
) -> Result<(Vec<LiveReplayEventDraft>, bool), LiveReplayStoreError> {
    let mut languages = HashMap::new();
    for stored in &buffer.events {
        if let SessionObservationEventPayload::LanguageExecution(observation) =
            &stored.event.payload
        {
            languages.insert(observation.execution.event_key.as_str(), observation);
        }
    }
    let mut activities = Vec::new();
    let mut overlapping = false;
    let mut fresh = Vec::with_capacity(drafts.len());
    for draft in &drafts {
        let admitted = match &draft.payload {
            SessionObservationEventPayload::LanguageExecution(observation) => {
                let key = observation.execution.event_key.as_str();
                if let Some(previous) = languages.get(key) {
                    if !observation.same_fact(previous) {
                        return Err(LiveReplayStoreError::ConflictingLanguageRedelivery {
                            session_id: session_id.clone(),
                            event_key: key.into(),
                        });
                    }
                    false
                } else {
                    languages.insert(key, observation);
                    true
                }
            }
            SessionObservationEventPayload::TurnActivity(activity) => {
                let claim = buffer
                    .delivered_activities
                    .claim(&activity.id, activities.iter().copied());
                activities.push(&activity.id);
                overlapping |= claim == Claim::Overlapping;
                claim == Claim::Fresh
            }
            _ => true,
        };
        fresh.push(admitted);
    }
    Ok((
        drafts
            .into_iter()
            .zip(fresh)
            .filter_map(|(draft, fresh)| fresh.then_some(draft))
            .collect(),
        overlapping,
    ))
}
