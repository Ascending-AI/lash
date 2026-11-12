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
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
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
                .append_event_for_testing(&process_id, request)
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
        .append_event_for_testing(&process_id, tick("1", blob.clone()))
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
        .append_event_for_testing(&process_id, tick("1", serde_json::json!({ "n": 0 })))
        .await
        .expect_err("another payload under a released replay key conflicts");
    assert!(
        crate::is_durable_identity_conflict(&conflict),
        "a conflicting replay of a released event is the durable-identity refusal, got {conflict:?}"
    );

    let next = registry
        .append_event_for_testing(&process_id, tick("4", serde_json::json!({ "n": 5 })))
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
            .append_event_for_testing(&process_id, tick("5", serde_json::json!({ "n": 6 })))
            .await
            .expect("append after releasing every event")
            .event
            .sequence,
        next + 1,
        "releasing every event does not rewind the sequence"
    );
}

/// The event that ends a process is an event like any other (FIG-7701): a
/// fact after it takes the next sequence, every page, the recent tail and the
/// replay-key read return it in order, a release through it counts it and its
/// key still coalesces or conflicts afterwards, and a prune counts it. Each
/// sequence and count is fixed by the appends below.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn the_terminal_event_reads_releases_and_prunes_like_any_event(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    use lash_execution_contracts::process::{ProcessCompletionAuthority, ProcessEventKind};

    let process_id = registry
        .register_process(registration("terminal-event-reads"))
        .await
        .expect("register the process whose terminal event is read")
        .id;
    let tick = registry
        .append_event_for_testing(
            &process_id,
            call_wait_event(&process_id, "tick", "1", serde_json::json!({ "n": 1 })),
        )
        .await
        .expect("append an event before the terminal")
        .event;
    let authority = ProcessCompletionAuthority::workflow_key(&process_id);
    let terminal = |value| {
        lash_execution_contracts::process::terminal_append_request(
            &process_id,
            &settled_success(value),
            Some(&authority),
        )
    };
    let ended = registry
        .append_event_for_testing(&process_id, terminal(serde_json::json!({ "done": 1 })))
        .await
        .expect("end the process")
        .event;
    registry
        .add_observer(
            &SessionId::from("terminal-event-observer"),
            &process_id,
            crate::ProcessObserverBy::host("after-terminal"),
        )
        .await
        .expect("an observer joins the finished process");
    assert_eq!(
        (tick.sequence, ended.sequence),
        (1, 2),
        "the tick and the terminal take the first two sequences"
    );
    assert_eq!(
        registry
            .stored_event_sequences_for_testing(&process_id)
            .await
            .expect("read where the events are stored"),
        (Some(2), vec![1, 3]),
        "the terminal is packed on the row and the events either side of it are rows: \
         every read below goes through the packed form"
    );

    let limit = std::num::NonZeroUsize::new(16).unwrap_or(std::num::NonZeroUsize::MIN);
    let full = match page_after(registry.as_ref(), &process_id, 0).await {
        crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
            events: crate::ProcessEventPageEvents::Full(events),
            more: crate::ProcessEventPageMore::Complete,
        }) => events,
        other => panic!("expected one complete retained page, got {other:?}"),
    };
    assert_eq!(
        full.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the event after the terminal takes the next sequence and every event is paged"
    );
    assert_eq!(
        full[1], ended,
        "the paged terminal is the appended one, header and payload"
    );
    let lite = registry
        .event_page_after(&process_id, 0, limit, crate::ProcessEventQueryMode::Lite)
        .await
        .expect("read the lite page");
    let crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
        events: crate::ProcessEventPageEvents::Lite(lite),
        ..
    }) = lite
    else {
        panic!("expected a retained lite page, got {lite:?}");
    };
    assert_eq!(
        lite.iter()
            .map(|event| (event.sequence, event.kind))
            .collect::<Vec<_>>(),
        vec![
            (1, ProcessEventKind::Waiting),
            (2, ProcessEventKind::Completed),
            (3, ProcessEventKind::ObserverAdded),
        ],
        "the lite page names every event's kind"
    );
    assert_eq!(
        registry
            .recent_events(&process_id, 2)
            .await
            .expect("read the recent tail")
            .into_iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "the recent tail orders the terminal before the event after it"
    );
    let key = ended
        .invocation
        .effect_replay_key()
        .expect("a terminal append is replay-keyed")
        .to_owned();
    assert_eq!(
        registry
            .event_by_replay_key(&process_id, &key, 3)
            .await
            .expect("read the terminal by its replay key"),
        crate::ProcessEventReadOutcome::Retained(Some(ended.clone())),
        "the terminal is found by its replay key"
    );

    assert_eq!(
        registry
            .release_process_events(&process_id, 2)
            .await
            .expect("release through the terminal"),
        crate::ProcessEventRelease {
            released_through: 2,
            released_events: 2,
        },
        "a release through the terminal counts it"
    );
    assert_eq!(
        retained_sequences(page_after(registry.as_ref(), &process_id, 2).await),
        vec![3],
        "the event after a released terminal is still read"
    );
    let replayed = registry
        .append_event_for_testing(&process_id, terminal(serde_json::json!({ "done": 1 })))
        .await
        .expect("the released terminal's key coalesces on the same outcome");
    assert_eq!(
        (replayed.event.sequence, replayed.realization),
        (2, crate::StoreRealization::Coalesced),
        "the replay answers the released terminal"
    );
    let conflict = registry
        .append_event_for_testing(&process_id, terminal(serde_json::json!({ "done": 0 })))
        .await
        .expect_err("another outcome under the terminal's key conflicts");
    assert!(
        crate::is_durable_identity_conflict(&conflict),
        "a conflicting terminal replay is the durable-identity refusal, got {conflict:?}"
    );

    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Any,
                identity_label: Some("terminal-event-reads".to_owned()),
                ..ProcessListFilter::default()
            }),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the finished process");
    assert_eq!(
        (report.pruned_processes, report.pruned_events),
        (1, 3),
        "a prune counts the terminal with the other events"
    );
}
