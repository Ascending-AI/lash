use super::*;
use pretty_assertions::assert_eq;

async fn page_after(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
    after_sequence: u64,
) -> crate::ProcessEventReadOutcome<crate::ProcessEventPage> {
    let limit = std::num::NonZeroUsize::new(16).unwrap_or(std::num::NonZeroUsize::MIN);
    match registry
        .event_page_after(
            process_id,
            after_sequence,
            limit,
            crate::ProcessEventQueryMode::Full,
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => panic!("read events after {after_sequence}: {error}"),
    }
}

fn retained_sequences(
    outcome: crate::ProcessEventReadOutcome<crate::ProcessEventPage>,
) -> Vec<u64> {
    match outcome {
        crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
            events: crate::ProcessEventPageEvents::Full(events),
            more: crate::ProcessEventPageMore::Complete,
        }) => events.into_iter().map(|event| event.sequence).collect(),
        other => panic!("expected one complete retained page, got {other:?}"),
    }
}

/// A host release of a running process's event prefix (FIG-3482) strips the
/// payloads at or below the horizon and nothing else: reads below it are
/// refused typed, reads after it are unchanged, sequences and invocation ordinals
/// keep counting the released events, and a re-presented replay key still
/// coalesces on the released payload or conflicts on another one. The horizon
/// is clamped to the last event and never moves back, so repeated cleanup
/// releases nothing new.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn releasing_an_event_prefix_keeps_sequences_ordinals_and_replay_identity(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = registry
        .register_process(registration("release-event-prefix"))
        .await
        .expect("register a process whose prefix the host releases")
        .id;
    let blob = serde_json::json!({ "blob": "x".repeat(4096) });
    let tick = |replay: &str, payload: serde_json::Value| {
        call_wait_event(&process_id, "tick", replay, payload)
    };
    let mut sequences = Vec::new();
    for request in [
        tick("1", blob.clone()),
        call_wait_event(
            &process_id,
            "producer.note",
            "producer.note",
            serde_json::json!({ "n": 2 }),
        ),
        tick("2", serde_json::json!({ "n": 3 })),
        tick("3", serde_json::json!({ "n": 4 })),
    ] {
        sequences.push(
            registry
                .append_event(&process_id, request)
                .await
                .expect("append an event before the release")
                .event
                .sequence,
        );
    }
    let [first, _, horizon, kept] = sequences[..] else {
        panic!("four appends answer four sequences");
    };

    let release = |through| {
        let registry = Arc::clone(&registry);
        let process_id = process_id.clone();
        async move {
            registry
                .release_process_events(&process_id, through)
                .await
                .expect("release the event prefix")
        }
    };
    assert_eq!(
        release(horizon).await,
        crate::ProcessEventRelease {
            released_through: horizon,
            released_events: 3,
        },
        "a release reports the horizon and every event at or below it"
    );
    for through in [horizon, first] {
        assert_eq!(
            release(through).await,
            crate::ProcessEventRelease {
                released_through: horizon,
                released_events: 0,
            },
            "a repeated or lower release keeps the horizon and releases nothing"
        );
    }

    for after in [0, first, horizon - 1] {
        assert_eq!(
            page_after(registry.as_ref(), &process_id, after).await,
            crate::ProcessEventReadOutcome::NoLongerRetained(
                crate::ProcessEventHistoryRetention::Released {
                    released_through: horizon,
                }
            ),
            "a read starting below the horizon is refused typed, never served short"
        );
    }
    assert_eq!(
        retained_sequences(page_after(registry.as_ref(), &process_id, horizon).await),
        vec![kept],
        "a read after the horizon is unchanged"
    );
    assert_eq!(
        registry
            .recent_events(&process_id, 16)
            .await
            .expect("read the recent tail")
            .into_iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![kept],
        "the recent tail never returns a released event"
    );
    let replayed = registry
        .append_event(&process_id, tick("1", blob.clone()))
        .await
        .expect("a released event's replay key coalesces on the same payload");
    assert_eq!(
        (
            replayed.event.sequence,
            replayed.realization,
            replayed.event.fact.payload()
        ),
        (
            first,
            crate::StoreRealization::Coalesced,
            tick("1", blob.clone()).fact.payload()
        ),
        "the replay answers the released event with the re-presented payload"
    );
    let conflict = registry
        .append_event(&process_id, tick("1", serde_json::json!({ "n": 0 })))
        .await
        .expect_err("another payload under a released replay key conflicts");
    assert!(
        crate::is_durable_identity_conflict(&conflict),
        "a conflicting replay of a released event is the durable-identity refusal, got {conflict:?}"
    );

    let next = registry
        .append_event(&process_id, tick("4", serde_json::json!({ "n": 5 })))
        .await
        .expect("append after the release")
        .event
        .sequence;
    assert_eq!(
        next,
        kept + 1,
        "sequence allocation continues past the released events"
    );
    assert_eq!(
        release(u64::MAX).await,
        crate::ProcessEventRelease {
            released_through: next,
            released_events: 2,
        },
        "a release is clamped to the process's last event"
    );
    assert_eq!(
        retained_sequences(page_after(registry.as_ref(), &process_id, next).await),
        Vec::<u64>::new(),
        "a fully released history reads empty after its horizon"
    );
    assert_eq!(
        registry
            .append_event(&process_id, tick("5", serde_json::json!({ "n": 6 })))
            .await
            .expect("append after releasing every event")
            .event
            .sequence,
        next + 1,
        "releasing every event does not rewind the sequence"
    );
}
