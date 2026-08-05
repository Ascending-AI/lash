use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use super::*;

fn projected_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str)> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { content }) => {
            Some(("session_text", content))
        }
        RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { content }) => {
            Some(("session_reasoning", content))
        }
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::AssistantProseDelta { text },
            ..
        }) => Some(("turn_text", text.as_ref())),
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::ReasoningDelta { text },
            ..
        }) => Some(("turn_reasoning", text.as_ref())),
        _ => None,
    }
}

fn session_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str)> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { content }) => {
            Some(("text", content))
        }
        RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { content }) => {
            Some(("reasoning", content))
        }
        _ => None,
    }
}

fn turn_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str, &str)> {
    match event {
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event: TurnEvent::AssistantProseDelta { text },
            ..
        }) => Some(("text", correlation_id.0.as_ref(), text.as_ref())),
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event: TurnEvent::ReasoningDelta { text },
            ..
        }) => Some(("reasoning", correlation_id.0.as_ref(), text.as_ref())),
        _ => None,
    }
}

#[test]
fn slow_host_does_not_stall_provider_drain() {
    const DELTA_COUNT: usize = 64;
    let (provider_tx, mut provider_rx) = tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
    let (host_tx, host_rx) = tokio::sync::mpsc::channel(1);
    let drained = AtomicUsize::new(0);
    let mut drain = Box::pin(async {
        let mut forwarder = ProviderHostForwarder::new(&host_tx);
        while let Some(LlmStreamEvent::Delta(text)) = provider_rx.recv().await {
            drained.fetch_add(1, Ordering::Relaxed);
            forwarder.forward_delta(
                ProviderDeltaClass::AssistantProse,
                TurnActivityId::new("assistant"),
                text,
            );
        }
    });

    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut max_provider_queue = 0_usize;
    for sent in 0..DELTA_COUNT {
        provider_tx
            .send(LlmStreamEvent::Delta("x".to_string()))
            .expect("provider queue remains open");
        assert_eq!(drain.as_mut().poll(&mut context), Poll::Pending);
        let occupancy = sent + 1 - drained.load(Ordering::Relaxed);
        max_provider_queue = max_provider_queue.max(occupancy);
    }
    drop(drain);

    assert_eq!(
        drained.load(Ordering::Relaxed),
        DELTA_COUNT,
        "provider events must be assembled even while the host lane is full"
    );
    assert!(
        max_provider_queue <= 1,
        "provider queue occupancy {max_provider_queue} must stay at a small constant"
    );
    assert_eq!(host_rx.len(), 1, "host occupancy stays at its hard bound");
}

#[test]
fn full_host_lane_does_not_construct_pending_payloads() {
    const CHUNK_COUNT: usize = 32_000;
    let (host_tx, _host_rx) = tokio::sync::mpsc::channel(1);
    host_tx
        .try_send(RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta {
            content: "occupied".to_string(),
        }))
        .expect("host lane starts full");
    let mut forwarder = ProviderHostForwarder::new(&host_tx);

    for _ in 0..CHUNK_COUNT {
        forwarder.forward_delta(
            ProviderDeltaClass::AssistantProse,
            TurnActivityId::new("assistant"),
            "0123456789abcdef".to_string(),
        );
    }

    assert_eq!(
        forwarder.payload_constructions(),
        0,
        "a stalled lane must reserve capacity before cloning the growing payload"
    );
}

#[test]
fn cancelled_finish_drops_pending_deltas_without_waiting_for_host() {
    let (host_tx, host_rx) = tokio::sync::mpsc::channel(1);
    let mut forwarder = ProviderHostForwarder::new(&host_tx);
    for _ in 0..1_024 {
        forwarder.forward_delta(
            ProviderDeltaClass::AssistantProse,
            TurnActivityId::new("assistant"),
            "pending".to_string(),
        );
    }
    assert_eq!(host_rx.len(), 1, "the host lane is saturated");

    let mut finish = Box::pin(forwarder.finish(true));
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    assert_eq!(
        finish.as_mut().poll(&mut context),
        Poll::Ready(()),
        "cancellation must not wait for host capacity to flush deltas"
    );
}

#[tokio::test]
async fn cancelled_finish_keeps_forwarded_projection_lanes_even() {
    let (host_tx, mut host_rx) = tokio::sync::mpsc::channel(1);
    let mut forwarder = ProviderHostForwarder::new(&host_tx);
    forwarder.forward_delta(
        ProviderDeltaClass::AssistantProse,
        TurnActivityId::new("assistant"),
        "pending".to_string(),
    );

    assert_eq!(
        session_delta(&host_rx.try_recv().expect("session projection is forwarded")),
        Some(("text", "pending"))
    );
    forwarder.finish(true).await;
    assert_eq!(
        turn_delta(&host_rx.try_recv().expect("turn projection catches up")),
        Some(("text", "assistant", "pending"))
    );
}

#[tokio::test]
async fn fast_host_receives_every_delta_event_unchanged() {
    let (host_tx, mut host_rx) = tokio::sync::mpsc::channel(16);
    let mut forwarder = ProviderHostForwarder::new(&host_tx);

    for chunk in ["alpha", "beta", "gamma"] {
        forwarder.forward_delta(
            ProviderDeltaClass::AssistantProse,
            TurnActivityId::new("assistant"),
            chunk.to_string(),
        );
    }
    for chunk in ["why", "therefore"] {
        forwarder.forward_delta(
            ProviderDeltaClass::Reasoning,
            TurnActivityId::new("reasoning"),
            chunk.to_string(),
        );
    }
    forwarder.finish(false).await;

    let events = std::iter::from_fn(|| host_rx.try_recv().ok()).collect::<Vec<_>>();
    let projected = events
        .iter()
        .filter_map(projected_delta)
        .collect::<Vec<_>>();
    assert_eq!(
        projected,
        vec![
            ("session_text", "alpha"),
            ("turn_text", "alpha"),
            ("session_text", "beta"),
            ("turn_text", "beta"),
            ("session_text", "gamma"),
            ("turn_text", "gamma"),
            ("session_reasoning", "why"),
            ("turn_reasoning", "why"),
            ("session_reasoning", "therefore"),
            ("turn_reasoning", "therefore"),
        ]
    );
}

#[tokio::test]
async fn interleaved_correlations_and_classes_remain_distinct_at_capacity_one() {
    let (host_tx, mut host_rx) = tokio::sync::mpsc::channel(1);
    let mut forwarder = ProviderHostForwarder::new(&host_tx);
    for (class, correlation, content) in [
        (ProviderDeltaClass::AssistantProse, "A", "a1"),
        (ProviderDeltaClass::AssistantProse, "B", "b"),
        (ProviderDeltaClass::AssistantProse, "A", "a2"),
        (ProviderDeltaClass::Reasoning, "A", "r"),
    ] {
        forwarder.forward_delta(class, TurnActivityId::new(correlation), content.to_string());
    }

    let forwarding = forwarder.finish(false);
    let collecting = async {
        let mut events = Vec::new();
        while events.len() < 8 {
            events.push(host_rx.recv().await.expect("all projections are forwarded"));
        }
        events
    };
    let ((), events) = tokio::join!(forwarding, collecting);

    let session = events.iter().filter_map(session_delta).collect::<Vec<_>>();
    let turn = events.iter().filter_map(turn_delta).collect::<Vec<_>>();
    assert_eq!(
        session,
        vec![
            ("text", "a1"),
            ("text", "b"),
            ("text", "a2"),
            ("reasoning", "r"),
        ]
    );
    assert_eq!(
        turn,
        vec![
            ("text", "A", "a1"),
            ("text", "B", "b"),
            ("text", "A", "a2"),
            ("reasoning", "A", "r"),
        ]
    );
    assert_eq!(
        session,
        turn.iter()
            .map(|(class, _, content)| (*class, *content))
            .collect::<Vec<_>>(),
        "session and turn lanes must preserve identical event boundaries"
    );
}

#[tokio::test]
async fn slow_host_coalesces_losslessly_before_semantic_events() {
    let chunks = (0..64)
        .map(|index| format!("<{index}>"))
        .collect::<Vec<_>>();
    let expected = chunks.concat();
    let reasoning_chunks = (0..32)
        .map(|index| format!("[{index}]"))
        .collect::<Vec<_>>();
    let expected_reasoning = reasoning_chunks.concat();
    let (host_tx, mut host_rx) = tokio::sync::mpsc::channel(1);
    let mut forwarder = ProviderHostForwarder::new(&host_tx);
    for chunk in &chunks {
        forwarder.forward_delta(
            ProviderDeltaClass::AssistantProse,
            TurnActivityId::new("assistant"),
            chunk.clone(),
        );
    }
    for chunk in &reasoning_chunks {
        forwarder.forward_delta(
            ProviderDeltaClass::Reasoning,
            TurnActivityId::new("reasoning"),
            chunk.clone(),
        );
    }

    let forwarding = async {
        forwarder
            .send_semantic_turn_activity(
                TurnActivityId::new("attempt"),
                TurnEvent::ModelAttemptReset {
                    assistant_prose_correlation_ids: vec![TurnActivityId::new("assistant")],
                    reasoning_correlation_ids: Vec::new(),
                },
            )
            .await;
        forwarder
            .send_semantic_turn_activity(
                TurnActivityId::new("terminal"),
                TurnEvent::Error {
                    message: "terminal".to_string(),
                },
            )
            .await;
    };
    let collecting = async {
        let mut events = Vec::new();
        while let Some(event) = host_rx.recv().await {
            let terminal = matches!(
                &event,
                RuntimeStreamEvent::Turn(TurnActivity {
                    event: TurnEvent::Error { message },
                    ..
                }) if message == "terminal"
            );
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    };
    let ((), events) = tokio::join!(forwarding, collecting);

    let session_text_chunks = events
        .iter()
        .filter_map(|event| match projected_delta(event) {
            Some(("session_text", content)) => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    let turn_text_chunks = events
        .iter()
        .filter_map(|event| match projected_delta(event) {
            Some(("turn_text", content)) => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    let session_reasoning_chunks = events
        .iter()
        .filter_map(|event| match projected_delta(event) {
            Some(("session_reasoning", content)) => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    let turn_reasoning_chunks = events
        .iter()
        .filter_map(|event| match projected_delta(event) {
            Some(("turn_reasoning", content)) => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    let delta_count = events
        .iter()
        .filter(|event| projected_delta(event).is_some())
        .count();
    assert_eq!(session_text_chunks, turn_text_chunks);
    assert_eq!(session_reasoning_chunks, turn_reasoning_chunks);
    assert_eq!(session_text_chunks.concat(), expected);
    assert_eq!(session_reasoning_chunks.concat(), expected_reasoning);
    assert!(
        delta_count < (chunks.len() + reasoning_chunks.len()) * 2,
        "lag must shrink delta granularity"
    );

    let reset_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeStreamEvent::Turn(TurnActivity {
                    event: TurnEvent::ModelAttemptReset { .. },
                    ..
                })
            )
        })
        .expect("attempt reset is forwarded");
    let terminal_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeStreamEvent::Turn(TurnActivity {
                    event: TurnEvent::Error { message },
                    ..
                }) if message == "terminal"
            )
        })
        .expect("terminal error is forwarded");
    assert_eq!(
        events[..reset_index]
            .iter()
            .filter(|event| projected_delta(event).is_some())
            .count(),
        delta_count,
        "all pending deltas flush before the first semantic event"
    );
    assert_eq!(terminal_index, reset_index + 1);
}

#[test]
fn run_llm_call_routes_all_semantic_forwarding_through_the_forwarder() {
    let source = include_str!("../streaming.rs");
    assert!(source.contains("send_semantic_session_event("));
    assert!(source.contains("send_semantic_turn_activity("));
    assert!(
        !source.contains("send_session_event("),
        "streaming.rs must not bypass the forwarder's delta flush for session events"
    );
    assert!(
        !source.contains("send_turn_activity("),
        "streaming.rs must not bypass the forwarder's delta flush for turn events"
    );
}
