use std::future::Future;
use std::task::{Context, Poll};

use super::{LAG_BUDGET, ObservationSource, RuntimeStreamEvent, TurnObservations, TurnObserver};
use crate::engine::{
    DriveObservation, ObservationCursor, ObservationSink, ObservedEvent, ReplayKey,
};
use crate::llm::types::StreamBlockIdentity;
use crate::session_model::SessionStreamEvent;
use crate::{
    TurnActivity, TurnActivityId, TurnCancellationEvidence, TurnEvent, TurnOutcome, TurnStop,
};

fn delta(
    observer: &TurnObserver,
    cursor: &mut ObservationCursor,
    reasoning: bool,
    block: &str,
    text: &str,
) {
    let identity = StreamBlockIdentity::new(block, 0);
    let (session, turn) = if reasoning {
        (
            SessionStreamEvent::ReasoningDelta {
                content: text.to_string(),
                block: identity.clone(),
            },
            TurnEvent::ReasoningDelta {
                text: text.into(),
                block: identity,
            },
        )
    } else {
        (
            SessionStreamEvent::TextDelta {
                content: text.to_string(),
                block: identity.clone(),
            },
            TurnEvent::AssistantProseDelta {
                text: text.into(),
                block: identity,
            },
        )
    };
    observer.publish(RuntimeStreamEvent::Session(session));
    cursor.observe(
        observer,
        ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new(block)),
            event: turn,
        },
    );
}

/// Each queued event as `(lane, block, text)`; any other event as its lane
/// and a label.
fn drain(observations: &mut TurnObservations) -> Vec<(String, String, String)> {
    std::iter::from_fn(|| observations.try_take())
        .map(|event| match event {
            RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { content, block }) => {
                ("session_text".into(), block.id, content)
            }
            RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { content, block }) => {
                ("session_reasoning".into(), block.id, content)
            }
            RuntimeStreamEvent::Turn(TurnActivity {
                correlation_id,
                event: TurnEvent::AssistantProseDelta { text, .. },
                ..
            }) => (
                "turn_text".into(),
                correlation_id.0.to_string(),
                text.to_string(),
            ),
            RuntimeStreamEvent::Turn(TurnActivity {
                correlation_id,
                event: TurnEvent::ReasoningDelta { text, .. },
                ..
            }) => (
                "turn_reasoning".into(),
                correlation_id.0.to_string(),
                text.to_string(),
            ),
            RuntimeStreamEvent::Session(other) => {
                ("session".into(), String::new(), format!("{other:?}"))
            }
            RuntimeStreamEvent::Turn(other) => {
                ("turn".into(), String::new(), format!("{:?}", other.event))
            }
        })
        .collect()
}

fn row(lane: &str, block: &str, text: &str) -> (String, String, String) {
    (lane.to_string(), block.to_string(), text.to_string())
}

fn marker(observer: &TurnObserver, cursor: &mut ObservationCursor, label: &str) {
    cursor.observe(
        observer,
        ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new(label)),
            event: TurnEvent::Error {
                message: label.to_string(),
            },
        },
    );
}

#[test]
fn a_host_that_keeps_up_receives_every_delta_as_published() {
    let (observer, mut observations) = TurnObserver::unread();
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for chunk in ["Hello", " world"] {
        delta(&observer, &mut cursor, false, "A", chunk);
    }
    delta(&observer, &mut cursor, true, "R", "why");
    assert_eq!(
        drain(&mut observations),
        vec![
            row("session_text", "A", "Hello"),
            row("turn_text", "A", "Hello"),
            row("session_text", "A", " world"),
            row("turn_text", "A", " world"),
            row("session_reasoning", "R", "why"),
            row("turn_reasoning", "R", "why"),
        ]
    );
}

#[test]
fn a_lagging_single_lane_host_gets_merged_deltas() {
    // An activity-only host: session events are never queued, so no pairs.
    let (observer, mut observations) = TurnObserver::with_quiet_lanes(true, false);
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for index in 0..LAG_BUDGET {
        marker(&observer, &mut cursor, &index.to_string());
    }
    for index in 0..50 {
        delta(&observer, &mut cursor, false, "A", &format!("<{index}>"));
    }
    let rows = drain(&mut observations);
    let text = (0..50)
        .map(|index| format!("<{index}>"))
        .collect::<String>();
    assert_eq!(rows.len(), LAG_BUDGET + 1);
    assert_eq!(rows[LAG_BUDGET], row("turn_text", "A", &text));
}

#[test]
fn each_lane_merges_on_its_own_and_never_across_blocks_kinds_or_events() {
    let (observer, mut observations) = TurnObserver::unread();
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for index in 0..LAG_BUDGET {
        marker(&observer, &mut cursor, &index.to_string());
    }
    // The first event beyond the budget is queued as it is; later ones merge.
    delta(&observer, &mut cursor, false, "A", "a1");
    delta(&observer, &mut cursor, false, "A", "a2");
    delta(&observer, &mut cursor, false, "B", "b");
    delta(&observer, &mut cursor, true, "B", "r1");
    delta(&observer, &mut cursor, true, "B", "r2");
    marker(&observer, &mut cursor, "semantic");
    delta(&observer, &mut cursor, true, "B", "s");
    observer.publish(RuntimeStreamEvent::Session(SessionStreamEvent::Done));
    delta(&observer, &mut cursor, true, "B", "t");
    let rows = drain(&mut observations);
    let semantic = format!(
        "{:?}",
        TurnEvent::Error {
            message: "semantic".into()
        }
    );
    assert_eq!(
        rows[LAG_BUDGET..].to_vec(),
        vec![
            row("session_text", "A", "a1a2"),
            row("turn_text", "A", "a1a2"),
            row("session_text", "B", "b"),
            row("turn_text", "B", "b"),
            // The activity lane's own event stops its merge; the session
            // lane has none in between, so it keeps merging until `Done`.
            row("session_reasoning", "B", "r1r2s"),
            row("turn_reasoning", "B", "r1r2"),
            row("turn", "", &semantic),
            row("turn_reasoning", "B", "st"),
            row("session", "", "Done"),
            row("session_reasoning", "B", "t"),
        ]
    );
}

#[test]
fn a_cancellation_discards_the_lagging_deltas_and_keeps_every_other_event() {
    let (observer, mut observations) = TurnObserver::unread();
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for index in 0..LAG_BUDGET / 2 {
        delta(&observer, &mut cursor, false, "A", &index.to_string());
    }
    marker(&observer, &mut cursor, "tool");
    for index in 0..40 {
        delta(&observer, &mut cursor, false, "A", &format!("late{index}"));
    }
    cursor.observe(
        &observer,
        ObservedEvent::Session(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: TurnCancellationEvidence::internal("observer-test"),
            }),
        }),
    );
    cursor.observe(&observer, ObservedEvent::Session(SessionStreamEvent::Done));
    let rows = drain(&mut observations);

    assert_eq!(rows.len(), LAG_BUDGET + 3, "{:?}", &rows[LAG_BUDGET..]);
    assert!(
        rows[..LAG_BUDGET]
            .iter()
            .all(|(lane, _, _)| lane.ends_with("_text")),
        "the deltas next in line for the host are kept"
    );
    let beyond = &rows[LAG_BUDGET..];
    assert!(beyond[0].2.contains("\"tool\""), "{beyond:?}");
    assert!(beyond[1].2.contains("Cancelled"), "{beyond:?}");
    assert_eq!(beyond[2].2, "Done");
}

#[test]
fn keyed_observations_take_their_ids_from_key_and_ordinal() {
    let (observer, mut observations) = TurnObserver::unread();
    observer.observe(DriveObservation {
        key: ReplayKey::new("root:t1:1:0:llm_call:1"),
        ordinal: 3,
        event: ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::Error {
                message: "independent".into(),
            },
        },
    });
    observer.observe(DriveObservation {
        key: ReplayKey::new("root:t1:1:0:llm_call:1"),
        ordinal: 4,
        event: ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new("block:7")),
            event: TurnEvent::Error {
                message: "correlated".into(),
            },
        },
    });
    observer.observe(DriveObservation {
        key: ReplayKey::new("root:t1:1:0:tool:2"),
        ordinal: 0,
        event: ObservedEvent::Session(SessionStreamEvent::Error {
            message: "projected".into(),
            envelope: None,
        }),
    });
    let ids = std::iter::from_fn(|| observations.try_take())
        .filter_map(|event| match event {
            RuntimeStreamEvent::Turn(activity) => Some((
                activity.id.0.to_string(),
                activity.correlation_id.0.to_string(),
            )),
            RuntimeStreamEvent::Session(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            (
                "root:t1:1:0:llm_call:1#3".into(),
                "root:t1:1:0:llm_call:1#3".into()
            ),
            ("root:t1:1:0:llm_call:1#4".into(), "block:7".into()),
            ("root:t1:1:0:tool:2#0".into(), "root:t1:1:0:tool:2#0".into()),
        ]
    );
}

#[test]
fn published_waits_until_the_host_has_taken_and_published_everything() {
    let (observer, mut observations) = TurnObserver::unread();
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    delta(&observer, &mut cursor, false, "A", "tail");
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut published = std::pin::pin!(observer.published());

    assert_eq!(published.as_mut().poll(&mut context), Poll::Pending);
    assert!(matches!(
        observations.poll_next(&mut context),
        Poll::Ready(Some(_))
    ));
    observations.published_one();
    assert_eq!(
        published.as_mut().poll(&mut context),
        Poll::Pending,
        "one event still queued"
    );
    assert!(matches!(
        observations.poll_next(&mut context),
        Poll::Ready(Some(_))
    ));
    assert_eq!(
        published.as_mut().poll(&mut context),
        Poll::Pending,
        "taken is not yet published"
    );
    observations.published_one();
    assert_eq!(published.as_mut().poll(&mut context), Poll::Ready(()));
}
