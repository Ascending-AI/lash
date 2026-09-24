use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use super::*;

#[test]
fn terminal_attempt_position_tracks_observed_stream_state() {
    let empty = LlmStreamAccumulator::default();
    let no_evidence = crate::LlmStreamEvidence::default();
    assert_eq!(
        observed_stream_protocol_position(false, &empty, &no_evidence),
        crate::ProtocolPosition::NoResponse
    );

    let response_observed = crate::LlmStreamEvidence {
        response_started: true,
        ..Default::default()
    };
    assert_eq!(
        observed_stream_protocol_position(false, &empty, &response_observed),
        crate::ProtocolPosition::ResponseObserved
    );

    let request_diagnostic_only = crate::LlmStreamEvidence {
        request_body: Some("{\"model\":\"test\"}".to_string()),
        http_summary: Some("HTTP POST https://provider.test".to_string()),
        ..Default::default()
    };
    assert_eq!(
        observed_stream_protocol_position(false, &empty, &request_diagnostic_only),
        crate::ProtocolPosition::NoResponse,
        "request diagnostics alone cannot establish a response"
    );

    let mut output_started = LlmStreamAccumulator::default();
    output_started.push_text("partial output");
    assert_eq!(
        observed_stream_protocol_position(false, &output_started, &no_evidence),
        crate::ProtocolPosition::OutputStarted
    );
}

fn projected_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str)> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { content, .. }) => {
            Some(("session_text", content))
        }
        RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { content, .. }) => {
            Some(("session_reasoning", content))
        }
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::AssistantProseDelta { text, .. },
            ..
        }) => Some(("turn_text", text.as_ref())),
        RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::ReasoningDelta { text, .. },
            ..
        }) => Some(("turn_reasoning", text.as_ref())),
        _ => None,
    }
}

fn session_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str)> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta { content, .. }) => {
            Some(("text", content))
        }
        RuntimeStreamEvent::Session(SessionStreamEvent::ReasoningDelta { content, .. }) => {
            Some(("reasoning", content))
        }
        _ => None,
    }
}

fn turn_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str, &str)> {
    match event {
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event: TurnEvent::AssistantProseDelta { text, .. },
            ..
        }) => Some(("text", correlation_id.0.as_ref(), text.as_ref())),
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event: TurnEvent::ReasoningDelta { text, .. },
            ..
        }) => Some(("reasoning", correlation_id.0.as_ref(), text.as_ref())),
        _ => None,
    }
}

#[test]
fn provider_drain_never_waits_on_the_host() {
    const DELTA_COUNT: usize = 64;
    let (provider_tx, mut provider_rx) = tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
    let (host_tx, host_rx) = TurnObserver::unread();
    let drained = AtomicUsize::new(0);
    let mut drain = Box::pin(async {
        let mut forwarder = ProviderHostForwarder::new(&host_tx);
        while let Some(LlmStreamEvent::Delta { text, .. }) = provider_rx.recv().await {
            drained.fetch_add(1, Ordering::Relaxed);
            forwarder.forward_delta(
                ProviderDeltaClass::AssistantProse,
                StreamBlockIdentity::new("assistant", 0),
                text,
            );
        }
    });

    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    for sent in 0..DELTA_COUNT {
        provider_tx
            .send(LlmStreamEvent::Delta {
                block: StreamBlockIdentity::new("text:0", 0),
                text: "x".to_string(),
            })
            .expect("provider queue remains open");
        assert_eq!(drain.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            drained.load(Ordering::Relaxed),
            sent + 1,
            "the provider drain keeps up while nothing reads the host lane"
        );
    }
    drop(drain);
    assert!(
        host_rx.len() <= crate::runtime::turn_observer::LAG_BUDGET + 2,
        "an unread host's queue stays bounded: its lagging deltas merge"
    );
}

#[test]
fn every_delta_is_published_on_both_lanes_in_order() {
    let (host_tx, mut host_rx) = TurnObserver::unread();
    let mut forwarder = ProviderHostForwarder::new(&host_tx);

    for chunk in ["alpha", "beta", "gamma"] {
        forwarder.forward_delta(
            ProviderDeltaClass::AssistantProse,
            StreamBlockIdentity::new("assistant", 0),
            chunk.to_string(),
        );
    }
    for chunk in ["why", "therefore"] {
        forwarder.forward_delta(
            ProviderDeltaClass::Reasoning,
            StreamBlockIdentity::new("reasoning", 0),
            chunk.to_string(),
        );
    }

    let events = std::iter::from_fn(|| host_rx.try_take()).collect::<Vec<_>>();
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

#[test]
fn interleaved_correlations_and_classes_remain_distinct() {
    let (host_tx, mut host_rx) = TurnObserver::unread();
    let mut forwarder = ProviderHostForwarder::new(&host_tx);
    for (class, correlation, content) in [
        (ProviderDeltaClass::AssistantProse, "A", "a1"),
        (ProviderDeltaClass::AssistantProse, "B", "b"),
        (ProviderDeltaClass::AssistantProse, "A", "a2"),
        (ProviderDeltaClass::Reasoning, "A", "r"),
    ] {
        forwarder.forward_delta(
            class,
            StreamBlockIdentity::new(correlation, 0),
            content.to_string(),
        );
    }

    let events = std::iter::from_fn(|| host_rx.try_take()).collect::<Vec<_>>();
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
}
