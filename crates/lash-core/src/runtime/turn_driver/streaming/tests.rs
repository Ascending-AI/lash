use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
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

fn kind_label(kind: StreamBlockKind) -> &'static str {
    match kind {
        StreamBlockKind::AssistantText => "text",
        StreamBlockKind::Reasoning => "reasoning",
    }
}

/// The stream-block payload an event of either host lane carries.
fn stream_block(event: &RuntimeStreamEvent) -> Option<&StreamBlockEvent> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(block))
        | RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::StreamBlock(block),
            ..
        }) => Some(block),
        _ => None,
    }
}

fn session_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str)> {
    match event {
        RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(StreamBlockEvent::Delta {
            kind,
            text,
            ..
        })) => Some((kind_label(*kind), text)),
        _ => None,
    }
}

fn turn_delta(event: &RuntimeStreamEvent) -> Option<(&'static str, &str, &str)> {
    match event {
        RuntimeStreamEvent::Turn(TurnActivity {
            correlation_id,
            event: TurnEvent::StreamBlock(StreamBlockEvent::Delta { kind, text, .. }),
            ..
        }) => Some((kind_label(*kind), correlation_id.0.as_ref(), text.as_str())),
        _ => None,
    }
}

/// A streamed reply as a host renders it: each block's kind, identity and
/// text, in the provider's block order.
#[derive(Debug, Default, PartialEq, Eq)]
struct FoldedReply {
    blocks: std::collections::BTreeMap<u64, FoldedBlock>,
}

#[derive(Debug, PartialEq, Eq)]
struct FoldedBlock {
    kind: StreamBlockKind,
    block: StreamBlockIdentity,
    text: String,
    completed: bool,
}

impl FoldedReply {
    /// The one reducer: it reads the shared payload and nothing of the lane
    /// that carried it.
    fn fold(&mut self, event: &StreamBlockEvent) {
        let folded = self
            .blocks
            .entry(event.block().ordinal)
            .or_insert_with(|| FoldedBlock {
                kind: event.kind(),
                block: event.block().clone(),
                text: String::new(),
                completed: false,
            });
        match event {
            StreamBlockEvent::Started { .. } => {}
            StreamBlockEvent::Delta { text, .. } => folded.text.push_str(text),
            StreamBlockEvent::Completed { text, .. } => {
                folded.text.clone_from(text);
                folded.completed = true;
            }
        }
    }
}

/// FIG-5526: both host lanes carry the one stream-block payload, so one
/// reducer folds a streamed reply identically from the session stream and
/// from the turn-activity stream — two reasoning blocks of one provider item
/// stay apart by ordinal on both, and a completion corrects delta drift on
/// both.
#[test]
fn one_reducer_folds_a_streamed_reply_identically_from_both_host_lanes() {
    let (host_tx, mut host_rx) = TurnObserver::unread();
    let mut forwarder = ProviderHostForwarder::new(
        &host_tx,
        crate::engine::ObservationCursor::new(crate::engine::ReplayKey::new("test:stream")),
    );
    let summary = |index: u64| {
        StreamBlockIdentity::new(format!("rs_1:summary:{index}"), index)
            .with_item_id(Some("rs_1".to_string()))
    };
    let answer = StreamBlockIdentity::new("msg_1:0", 2).with_item_id(Some("msg_1".to_string()));
    let reasoning = StreamBlockKind::Reasoning;
    let prose = StreamBlockKind::AssistantText;
    for event in [
        StreamBlockEvent::started(reasoning, summary(0)),
        StreamBlockEvent::delta(reasoning, summary(0), "Weigh "),
        StreamBlockEvent::delta(reasoning, summary(0), "the options."),
        StreamBlockEvent::completed(reasoning, summary(0), "Weigh the options."),
        StreamBlockEvent::started(reasoning, summary(1)),
        StreamBlockEvent::delta(reasoning, summary(1), "Pick one."),
        StreamBlockEvent::completed(reasoning, summary(1), "Pick one."),
        StreamBlockEvent::started(prose, answer.clone()),
        StreamBlockEvent::delta(prose, answer.clone(), "The answer is "),
        StreamBlockEvent::delta(prose, answer.clone(), "fourty-two"),
        StreamBlockEvent::completed(prose, answer.clone(), "The answer is forty-two."),
    ] {
        forwarder.forward_block(event);
    }

    let events = std::iter::from_fn(|| host_rx.try_take()).collect::<Vec<_>>();
    let fold_lane = |session_lane: bool| {
        let mut reply = FoldedReply::default();
        for event in &events {
            if matches!(event, RuntimeStreamEvent::Session(_)) == session_lane
                && let Some(block) = stream_block(event)
            {
                reply.fold(block);
            }
        }
        reply
    };
    let from_session = fold_lane(true);
    let from_activity = fold_lane(false);
    assert_eq!(from_session, from_activity);
    assert_eq!(
        from_activity
            .blocks
            .values()
            .map(|block| (
                block.kind,
                block.block.clone(),
                block.text.as_str(),
                block.completed
            ))
            .collect::<Vec<_>>(),
        vec![
            (reasoning, summary(0), "Weigh the options.", true),
            (reasoning, summary(1), "Pick one.", true),
            (prose, answer, "The answer is forty-two.", true),
        ]
    );
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
        while let Some(LlmStreamEvent::Block(StreamBlockEvent::Delta { text, .. })) =
            provider_rx.recv().await
        {
            drained.fetch_add(1, Ordering::Relaxed);
            forwarder.forward_block(StreamBlockEvent::delta(
                StreamBlockKind::AssistantText,
                StreamBlockIdentity::new("assistant", 0),
                text,
            ));
        }
    });

    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    for sent in 0..DELTA_COUNT {
        provider_tx
            .send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                block: StreamBlockIdentity::new("text:0", 0),
                text: "x".to_string(),
            }))
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
        forwarder.forward_block(StreamBlockEvent::delta(
            StreamBlockKind::AssistantText,
            StreamBlockIdentity::new("assistant", 0),
            chunk.to_string(),
        ));
    }
    for chunk in ["why", "therefore"] {
        forwarder.forward_block(StreamBlockEvent::delta(
            StreamBlockKind::Reasoning,
            StreamBlockIdentity::new("reasoning", 0),
            chunk.to_string(),
        ));
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
        (StreamBlockKind::AssistantText, "A", "a1"),
        (StreamBlockKind::AssistantText, "B", "b"),
        (StreamBlockKind::AssistantText, "A", "a2"),
        (StreamBlockKind::Reasoning, "A", "r"),
    ] {
        forwarder.forward_block(StreamBlockEvent::delta(
            class,
            StreamBlockIdentity::new(correlation, 0),
            content.to_string(),
        ));
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
