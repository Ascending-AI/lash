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
        let mut forwarder = ProviderHostForwarder::new(
            &host_tx,
            crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new("test:stream")),
        );
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
    assert_eq!(
        host_rx.len(),
        4,
        "an unread host's queue stays bounded: each lane holds its first delta \
         and one frame the rest pile into"
    );
}

#[test]
fn every_delta_reaches_both_lanes_in_order_framed() {
    let (host_tx, mut host_rx) = TurnObserver::unread();
    let mut forwarder = ProviderHostForwarder::new(
        &host_tx,
        crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new("test:stream")),
    );

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
    let session = events.iter().filter_map(session_delta).collect::<Vec<_>>();
    let turn = events.iter().filter_map(turn_delta).collect::<Vec<_>>();
    // Each block's first delta on its own, the rest of the block framed.
    assert_eq!(
        session,
        vec![
            ("text", "alpha"),
            ("text", "betagamma"),
            ("reasoning", "why"),
            ("reasoning", "therefore"),
        ]
    );
    assert_eq!(
        turn,
        vec![
            ("text", "test:stream/assistant", "alpha"),
            ("text", "test:stream/assistant", "betagamma"),
            ("reasoning", "test:stream/reasoning", "why"),
            ("reasoning", "test:stream/reasoning", "therefore"),
        ]
    );
}

#[test]
fn interleaved_correlations_and_classes_remain_distinct() {
    let (host_tx, mut host_rx) = TurnObserver::unread();
    let mut forwarder = ProviderHostForwarder::new(
        &host_tx,
        crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new("test:stream")),
    );
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
            ("text", "test:stream/A", "a1"),
            ("text", "test:stream/B", "b"),
            ("text", "test:stream/A", "a2"),
            ("reasoning", "test:stream/A", "r"),
        ]
    );
}
