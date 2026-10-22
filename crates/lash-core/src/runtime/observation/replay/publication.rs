//! Semantic identity checks within the retained window and a publication batch.
use super::activity_spans::Claim;
use super::*;

#[cfg(test)]
thread_local! {
    pub(super) static LANGUAGE_HISTORY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn filter(
    buffer: &LiveReplaySessionBuffer,
    drafts: Vec<LiveReplayEventDraft>,
    session_id: &SessionId,
) -> Result<(Vec<LiveReplayEventDraft>, bool), LiveReplayStoreError> {
    let mut languages = None;
    let mut activities = Vec::new();
    let mut overlapping = false;
    let mut fresh = Vec::with_capacity(drafts.len());
    for draft in &drafts {
        let admitted = match &draft.payload {
            SessionObservationEventPayload::LanguageExecution(observation) => {
                // Ordinary activity and committed-delta publications need no
                // language-redelivery lookup. Build it only for a batch that
                // actually contains a language fact, under the same buffer lock.
                let languages = languages.get_or_insert_with(|| {
                    let mut languages = HashMap::new();
                    for stored in &buffer.events {
                        #[cfg(test)]
                        LANGUAGE_HISTORY_VISITS.with(|count| count.set(count.get() + 1));
                        if let SessionObservationEventPayload::LanguageExecution(observation) =
                            &stored.event.payload
                        {
                            languages.insert(observation.execution.event_key.as_str(), observation);
                        }
                    }
                    languages
                });
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
