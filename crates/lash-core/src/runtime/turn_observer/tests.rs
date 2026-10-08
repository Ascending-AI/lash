use crate::ReportedFailure;
use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use lash_sansio::sync::MutexExt;

use super::{DeltaFraming, ObservationSource, RuntimeStreamEvent, TurnObservations, TurnObserver};
use crate::engine::{
    ObservationCursor, ObservationSink, ObservedEvent, ReplayKey, ShiftObservation,
};
use crate::llm::types::StreamBlockIdentity;
use crate::runtime::{DeltaCoalescing, DeltaCoalescingError};
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
            SessionStreamEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                text: text.to_string(),
                block: identity.clone(),
            }),
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                text: text.into(),
                block: identity,
            }),
        )
    } else {
        (
            SessionStreamEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text: text.to_string(),
                block: identity.clone(),
            }),
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text: text.into(),
                block: identity,
            }),
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
        .map(describe)
        .collect()
}

fn describe(event: RuntimeStreamEvent) -> (String, String, String) {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            text: content,
            block,
        })) => ("session_text".into(), block.id, content),
        RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::Reasoning,
            text: content,
            block,
        })) => ("session_reasoning".into(), block.id, content),
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event:
                TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    text,
                    ..
                }),
            ..
        }) => (
            "turn_text".into(),
            correlation_id.0.to_string(),
            text.to_string(),
        ),
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event:
                TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::Reasoning,
                    text,
                    ..
                }),
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
    }
}

fn row(lane: &str, block: &str, text: &str) -> (String, String, String) {
    (lane.to_string(), block.to_string(), text.to_string())
}

fn marker(observer: &TurnObserver, cursor: &mut ObservationCursor, label: &str) {
    cursor.observe(
        observer,
        ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new(label)),
            event: TurnEvent::Error(ReportedFailure {
                message: label.to_string(),
                envelope: None,
            }),
        },
    );
}

/// A clock the test moves by hand. A frame timer never fires on its own:
/// the publisher sees a frame fall due only once the clock is advanced.
#[derive(Debug)]
struct HandClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl HandClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        })
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock_recover() += by;
    }
}

#[async_trait::async_trait]
impl crate::Clock for HandClock {
    fn now(&self) -> Instant {
        self.start + *self.elapsed.lock_recover()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::UNIX_EPOCH
    }

    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }

    async fn sleep_until(&self, _deadline: Instant) {
        std::future::pending::<()>().await;
    }
}

const INTERVAL: Duration = DeltaCoalescing::RECOMMENDED_INTERVAL;
const MAX_FRAME_BYTES: usize = DeltaCoalescing::RECOMMENDED_MAX_FRAME_BYTES;

/// An observer whose host listens to both lanes, framing by default on
/// `clock`.
fn framed(clock: &Arc<HandClock>) -> (TurnObserver, TurnObservations) {
    framed_with(clock, DeltaCoalescing::recommended(), false)
}

fn framed_with(
    clock: &Arc<HandClock>,
    coalescing: DeltaCoalescing,
    quiet_sessions: bool,
) -> (TurnObserver, TurnObservations) {
    TurnObserver::with_quiet_lanes(
        quiet_sessions,
        false,
        DeltaFraming {
            clock: Arc::clone(clock) as Arc<dyn crate::Clock>,
            coalescing,
        },
    )
}

/// What the publisher takes now, as `(lane, block, text)` rows, marking each
/// taken event published.
fn publish_ready(observations: &mut TurnObservations) -> Vec<(String, String, String)> {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut taken = Vec::new();
    while let Poll::Ready(Some(observation)) = observations.poll_next(&mut context) {
        observations.published_one();
        taken.push(observation.event);
    }
    taken.into_iter().map(describe).collect()
}

/// Each activity the queue holds, as its id and text.
fn activity_ids(observations: &mut TurnObservations) -> Vec<(String, String)> {
    std::iter::from_fn(|| observations.try_take())
        .filter_map(|event| match event {
            RuntimeStreamEvent::Turn(TurnActivity {
                id,
                event:
                    TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        text,
                        ..
                    }),
                ..
            }) => Some((id.0.to_string(), text.to_string())),
            RuntimeStreamEvent::Turn(TurnActivity { id, .. }) => {
                Some((id.0.to_string(), String::new()))
            }
            RuntimeStreamEvent::Session(_) => None,
        })
        .collect()
}

#[test]
fn the_first_delta_of_a_block_is_published_at_once_and_the_rest_when_the_frame_falls_due() {
    let clock = HandClock::new();
    let (observer, mut observations) = framed(&clock);
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    delta(&observer, &mut cursor, false, "A", "first");
    assert_eq!(
        publish_ready(&mut observations),
        vec![
            row("session_text", "A", "first"),
            row("turn_text", "A", "first")
        ],
        "time to first token never waits on a frame"
    );

    for text in ["a", "b", "c"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    clock.advance(INTERVAL - Duration::from_millis(1));
    assert_eq!(publish_ready(&mut observations), Vec::new());
    clock.advance(Duration::from_millis(1));
    assert_eq!(
        publish_ready(&mut observations),
        vec![
            row("session_text", "A", "abc"),
            row("turn_text", "A", "abc")
        ],
        "the frame is due its interval after it opened"
    );

    // A host that has not taken the due frame keeps getting it extended.
    delta(&observer, &mut cursor, false, "A", "d");
    clock.advance(INTERVAL * 3);
    delta(&observer, &mut cursor, false, "A", "e");
    assert_eq!(
        publish_ready(&mut observations),
        vec![row("session_text", "A", "de"), row("turn_text", "A", "de")]
    );
}

#[test]
fn a_frame_is_cut_before_any_other_event_and_never_spans_blocks_kinds_or_turns() {
    let clock = HandClock::new();
    let (observer, mut observations) = framed(&clock);
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for text in ["a1", "a2", "a3"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    marker(&observer, &mut cursor, "tool");
    for text in ["a4", "a5"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    // Alternating blocks: each delta is the first of its block again.
    delta(&observer, &mut cursor, false, "B", "b1");
    delta(&observer, &mut cursor, false, "A", "a6");
    delta(&observer, &mut cursor, true, "A", "r1");
    delta(&observer, &mut cursor, true, "A", "r2");
    // The same block on another physical turn is another frame.
    let other_turn = observer.for_turn(&crate::TurnId::fixture("turn-2".to_string()));
    delta(&other_turn, &mut cursor, true, "A", "r3");
    observer.publish(RuntimeStreamEvent::Session(SessionStreamEvent::Done));

    let rows = drain(&mut observations);
    let lane_rows = |lane: &str| {
        rows.iter()
            .filter(|(row_lane, _, _)| row_lane.starts_with(lane))
            .map(|(lane, block, text)| format!("{lane}:{block}:{text}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        lane_rows("turn"),
        vec![
            "turn_text:A:a1",
            "turn_text:A:a2a3",
            "turn::Error(ReportedFailure { message: \"tool\", envelope: None })",
            "turn_text:A:a4a5",
            "turn_text:B:b1",
            "turn_text:A:a6",
            "turn_reasoning:A:r1",
            "turn_reasoning:A:r2",
            "turn_reasoning:A:r3",
        ]
    );
    assert_eq!(
        lane_rows("session"),
        vec![
            "session_text:A:a1",
            "session_text:A:a2a3",
            "session_text:A:a4a5",
            "session_text:B:b1",
            "session_text:A:a6",
            "session_reasoning:A:r1",
            "session_reasoning:A:r2r3",
            "session::Done",
        ],
        "any lane's event cuts every frame; session deltas carry no turn"
    );
}

#[test]
fn a_frame_names_the_observation_range_it_covers() {
    let clock = HandClock::new();
    // An activity-only host, as engine runs have.
    let (observer, mut observations) = framed_with(&clock, DeltaCoalescing::recommended(), true);
    let mut cursor = ObservationCursor::new(ReplayKey::new("k"));
    for text in ["a", "b", "c", "d"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    marker(&observer, &mut cursor, "tool");
    delta(&observer, &mut cursor, false, "A", "e");
    delta(&observer, &mut cursor, false, "A", "f");
    assert_eq!(
        activity_ids(&mut observations),
        vec![
            ("k#0".into(), "a".into()),
            ("k#1..3".into(), "bcd".into()),
            ("k#4".into(), String::new()),
            ("k#5..6".into(), "ef".into()),
        ]
    );
}

#[test]
fn a_hosts_coalescing_terms_take_effect() {
    let clock = HandClock::new();
    let mut cursor = ObservationCursor::new(ReplayKey::new("k"));

    // A longer interval, a smaller cap and no immediate first delta.
    let interval = Duration::from_millis(200);
    let terms = DeltaCoalescing::new(interval, 4, false).expect("in range");
    let (observer, mut observations) = framed_with(&clock, terms, true);
    for text in ["ab", "cd"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    clock.advance(INTERVAL);
    assert_eq!(
        publish_ready(&mut observations),
        Vec::new(),
        "even the first delta waits for its frame, due after the longer interval"
    );
    delta(&observer, &mut cursor, false, "A", "e");
    assert_eq!(
        publish_ready(&mut observations),
        vec![row("turn_text", "A", "abcd")],
        "the frame is cut at four bytes"
    );
    clock.advance(interval - INTERVAL);
    assert_eq!(publish_ready(&mut observations), Vec::new());
    clock.advance(INTERVAL);
    assert_eq!(
        publish_ready(&mut observations),
        vec![row("turn_text", "A", "e")]
    );

    // Off: one event per delta, each under its own observation's id.
    let (observer, mut observations) = framed_with(&clock, DeltaCoalescing::off(), true);
    for text in ["f", "g", "h"] {
        delta(&observer, &mut cursor, false, "A", text);
    }
    assert_eq!(
        activity_ids(&mut observations),
        vec![
            ("k#3".into(), "f".into()),
            ("k#4".into(), "g".into()),
            ("k#5".into(), "h".into()),
        ]
    );
    assert!(
        DeltaCoalescing::new(Duration::ZERO, 1, true)
            .expect("zero is off")
            .is_off()
    );
}

#[test]
fn out_of_range_coalescing_terms_are_refused() {
    assert!(matches!(
        DeltaCoalescing::new(
            DeltaCoalescing::MAX_INTERVAL + Duration::from_millis(1),
            1,
            true
        ),
        Err(DeltaCoalescingError::Interval { .. })
    ));
    for max_frame_bytes in [0, DeltaCoalescing::MAX_FRAME_BYTES + 1] {
        assert!(matches!(
            DeltaCoalescing::new(INTERVAL, max_frame_bytes, true),
            Err(DeltaCoalescingError::MaxFrameBytes { .. })
        ));
    }
}

#[test]
fn a_frame_is_cut_at_its_size_cap() {
    let clock = HandClock::new();
    let (observer, mut observations) = framed(&clock);
    let mut cursor = ObservationCursor::new(ReplayKey::new("k"));
    let chunk = "x".repeat(MAX_FRAME_BYTES / 2);
    for _ in 0..6 {
        delta(&observer, &mut cursor, false, "A", &chunk);
    }
    let sizes = activity_ids(&mut observations)
        .into_iter()
        .map(|(id, text)| (id, text.len()))
        .collect::<Vec<_>>();
    assert_eq!(
        sizes,
        vec![
            ("k#0".into(), MAX_FRAME_BYTES / 2),
            ("k#1..2".into(), MAX_FRAME_BYTES),
            ("k#3..4".into(), MAX_FRAME_BYTES),
            ("k#5".into(), MAX_FRAME_BYTES / 2),
        ]
    );
}

fn stopped_cancelled(observer: &TurnObserver, cursor: &mut ObservationCursor) {
    cursor.observe(
        observer,
        ObservedEvent::Session(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: TurnCancellationEvidence::internal("observer-test"),
            }),
        }),
    );
}

#[test]
fn a_cancellation_delivers_the_open_frame_whole_before_the_terminal() {
    let clock = HandClock::new();
    let (observer, mut observations) = framed(&clock);
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    for index in 0..40 {
        delta(&observer, &mut cursor, false, "A", &format!("<{index}>"));
    }
    stopped_cancelled(&observer, &mut cursor);
    cursor.observe(&observer, ObservedEvent::Session(SessionStreamEvent::Done));
    let rows = publish_ready(&mut observations);

    let tail = (1..40)
        .map(|index| format!("<{index}>"))
        .collect::<String>();
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert_eq!(rows[0], row("session_text", "A", "<0>"));
    assert_eq!(rows[1], row("turn_text", "A", "<0>"));
    assert_eq!(rows[2], row("session_text", "A", &tail));
    assert_eq!(rows[3], row("turn_text", "A", &tail));
    assert!(rows[4].2.contains("Cancelled"), "{rows:?}");
    assert_eq!(rows[5].2, "Done");
}

#[test]
fn a_held_terminal_publishes_only_on_release_and_in_order() {
    let (observer, mut observations) = TurnObserver::unread();
    let mut cursor = ObservationCursor::new(ReplayKey::new("test"));
    delta(&observer, &mut cursor, false, "A", "before");
    observer.hold_terminal();
    delta(&observer, &mut cursor, false, "A", "held");
    stopped_cancelled(&observer, &mut cursor);
    cursor.observe(&observer, ObservedEvent::Session(SessionStreamEvent::Done));
    assert_eq!(
        drain(&mut observations),
        vec![
            row("session_text", "A", "before"),
            row("turn_text", "A", "before"),
        ],
        "nothing after the hold publishes before the commit"
    );

    observer.release_terminal();
    let released = drain(&mut observations);
    assert_eq!(released.len(), 4, "{released:?}");
    assert_eq!(released[0], row("session_text", "A", "held"));
    assert_eq!(released[1], row("turn_text", "A", "held"));
    assert!(released[2].2.contains("Cancelled"), "{released:?}");
    assert_eq!(released[3].2, "Done");
}

#[test]
fn keyed_observations_take_their_ids_from_key_and_ordinal() {
    let (observer, mut observations) = TurnObserver::unread();
    observer.observe(ShiftObservation {
        key: ReplayKey::new("root:t1:1:0:llm_call:1"),
        ordinal: 3,
        event: ObservedEvent::Activity {
            correlation_id: None,
            event: TurnEvent::Error(ReportedFailure {
                message: "independent".into(),
                envelope: None,
            }),
        },
    });
    observer.observe(ShiftObservation {
        key: ReplayKey::new("root:t1:1:0:llm_call:1"),
        ordinal: 4,
        event: ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new("block:7")),
            event: TurnEvent::Error(ReportedFailure {
                message: "correlated".into(),
                envelope: None,
            }),
        },
    });
    observer.observe(ShiftObservation {
        key: ReplayKey::new("root:t1:1:0:tool:2"),
        ordinal: 0,
        event: ObservedEvent::Session(SessionStreamEvent::Error(ReportedFailure {
            message: "projected".into(),
            envelope: None,
        })),
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
