use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::runtime::turn_driver::capture_writer::CaptureWriter;
use crate::store::CaptureFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Tool-argument wire fragments reach this module only as capture frames:
// hosts see a call once it is whole, as a semantic `Part(ToolCall)`.
pub(super) enum ProviderDeltaClass {
    AssistantProse,
    Reasoning,
}

impl ProviderDeltaClass {
    fn block_kind(self) -> StreamBlockKind {
        match self {
            Self::AssistantProse => StreamBlockKind::AssistantText,
            Self::Reasoning => StreamBlockKind::Reasoning,
        }
    }

    fn session_event(self, content: String, block: StreamBlockIdentity) -> SessionStreamEvent {
        match self {
            Self::AssistantProse => SessionStreamEvent::TextDelta { content, block },
            Self::Reasoning => SessionStreamEvent::ReasoningDelta { content, block },
        }
    }

    fn turn_event(self, text: Arc<str>, block: StreamBlockIdentity) -> TurnEvent {
        match self {
            Self::AssistantProse => TurnEvent::AssistantProseDelta { text, block },
            Self::Reasoning => TurnEvent::ReasoningDelta { text, block },
        }
    }

    fn start_frame(self, block: StreamBlockIdentity) -> CaptureFrame {
        match self {
            Self::AssistantProse => CaptureFrame::TextStart { block },
            Self::Reasoning => CaptureFrame::ReasoningStart { block },
        }
    }

    fn delta_frame(self, block: StreamBlockIdentity, text: String) -> CaptureFrame {
        match self {
            Self::AssistantProse => CaptureFrame::TextDelta { block, text },
            Self::Reasoning => CaptureFrame::ReasoningDelta { block, text },
        }
    }

    fn end_frame(self, block: StreamBlockIdentity, text: String) -> CaptureFrame {
        match self {
            Self::AssistantProse => CaptureFrame::TextEnd { block, text },
            Self::Reasoning => CaptureFrame::ReasoningEnd { block, text },
        }
    }
}

/// Ordinals of calls whose arguments the adapter never streamed: above every
/// adapter's dense attempt-local band.
const UNSTREAMED_CALL_ORDINAL_BASE: u64 = 1 << 32;

/// One streamed call's capture state within the attempt.
struct StreamedCall {
    identity: lash_sansio::ToolInputIdentity,
    ended: bool,
}

/// One provider call's stream, projected onto both host lanes.
///
/// Every delta and block boundary is published session projection first,
/// through the turn's observer, which never waits on the host. A slow host is
/// the observation queue's concern: it merges the deltas that queue up behind
/// a lagging host (see `turn_observer`), so the provider drain never stalls on
/// host throughput.
///
/// With a turn capture (ADR 0114 §4.1), every observation is held until
/// [`flush`](Self::flush) has persisted the frames staged before it, and is
/// then released in order: a host is never sent content a lost worker could
/// take with it. A failed flush stops publication for good and is kept as the
/// step's fault.
pub(super) struct ProviderHostForwarder<'a> {
    event_tx: &'a TurnObserver,
    /// The provider call's observation lane: every event the stream forwards
    /// sequences under the call's replay key (ADR 0105 §1).
    cursor: crate::engine::ObservationCursor,
    capture: Option<CaptureWriter>,
    held: Vec<crate::engine::ObservedEvent>,
    calls: BTreeMap<u64, StreamedCall>,
    unstreamed_calls: u64,
    fault: Option<crate::store::StoreError>,
}

impl<'a> ProviderHostForwarder<'a> {
    pub(super) fn new(
        event_tx: &'a TurnObserver,
        cursor: crate::engine::ObservationCursor,
        capture: Option<CaptureWriter>,
    ) -> Self {
        Self {
            event_tx,
            cursor,
            capture,
            held: Vec::new(),
            calls: BTreeMap::new(),
            unstreamed_calls: 0,
            fault: None,
        }
    }

    fn observe(&mut self, event: crate::engine::ObservedEvent) {
        if self.fault.is_some() {
            return;
        }
        if self.capture.is_some() {
            self.held.push(event);
        } else {
            self.cursor.observe(self.event_tx, event);
        }
    }

    fn capture_frame(&mut self, frame: CaptureFrame) {
        if let Some(capture) = self.capture.as_mut() {
            capture.push(frame);
        }
    }

    pub(super) fn forward_delta(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
        content: String,
    ) {
        if content.is_empty() {
            return;
        }
        self.capture_frame(class.delta_frame(block.clone(), content.clone()));
        if self.event_tx.is_closed() {
            return;
        }
        let correlation_id = TurnActivityId::new(block.id.clone());
        let text = Arc::from(content.as_str());
        self.observe(crate::engine::ObservedEvent::Session(
            class.session_event(content, block.clone()),
        ));
        self.observe(crate::engine::ObservedEvent::Activity {
            correlation_id: Some(correlation_id),
            event: class.turn_event(text, block),
        });
    }

    /// A provider-minted block opened: emit the boundary on both projections.
    pub(super) fn forward_block_start(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
    ) {
        self.capture_frame(class.start_frame(block.clone()));
        let kind = class.block_kind();
        self.observe(crate::engine::ObservedEvent::Session(
            SessionStreamEvent::StreamBlockStarted {
                kind,
                block: block.clone(),
            },
        ));
        self.observe(crate::engine::ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new(block.id.clone())),
            event: TurnEvent::StreamBlockStarted { kind, block },
        });
    }

    /// A provider-minted block closed; `text` is its authoritative text.
    pub(super) fn forward_block_end(
        &mut self,
        class: ProviderDeltaClass,
        block: StreamBlockIdentity,
        text: String,
    ) {
        self.capture_frame(class.end_frame(block.clone(), text.clone()));
        let kind = class.block_kind();
        self.observe(crate::engine::ObservedEvent::Session(
            SessionStreamEvent::StreamBlockCompleted {
                kind,
                block: block.clone(),
                content: text.clone(),
            },
        ));
        self.observe(crate::engine::ObservedEvent::Activity {
            correlation_id: Some(TurnActivityId::new(block.id.clone())),
            event: TurnEvent::StreamBlockCompleted {
                kind,
                block,
                text: text.into(),
            },
        });
    }

    pub(super) fn send_semantic_session_event(&mut self, event: SessionStreamEvent) {
        self.observe(crate::engine::ObservedEvent::Session(event));
    }

    pub(super) fn send_semantic_turn_activity(
        &mut self,
        correlation_id: Option<TurnActivityId>,
        event: TurnEvent,
    ) {
        self.observe(crate::engine::ObservedEvent::Activity {
            correlation_id,
            event,
        });
    }

    /// A call's arguments started streaming (ADR 0114 §2.1).
    pub(super) fn capture_tool_input_start(&mut self, call: lash_sansio::ToolInputIdentity) {
        self.calls.insert(
            call.ordinal,
            StreamedCall {
                identity: call.clone(),
                ended: false,
            },
        );
        self.capture_frame(CaptureFrame::ToolInputStart { call });
    }

    pub(super) fn capture_tool_input_delta(
        &mut self,
        call: lash_sansio::ToolInputIdentity,
        text: String,
    ) {
        self.learn_call_identity(&call);
        self.capture_frame(CaptureFrame::ToolInputDelta { call, text });
    }

    pub(super) fn capture_tool_input_end(
        &mut self,
        call: lash_sansio::ToolInputIdentity,
        raw_arguments: String,
    ) {
        self.learn_call_identity(&call);
        if let Some(streamed) = self.calls.get_mut(&call.ordinal) {
            streamed.ended = true;
        }
        self.capture_frame(CaptureFrame::ToolInputEnd {
            call,
            raw_arguments,
        });
    }

    fn learn_call_identity(&mut self, call: &lash_sansio::ToolInputIdentity) {
        if let Some(streamed) = self.calls.get_mut(&call.ordinal) {
            if streamed.identity.call_id.is_none() {
                streamed.identity.call_id.clone_from(&call.call_id);
            }
            if streamed.identity.tool_name.is_none() {
                streamed.identity.tool_name.clone_from(&call.tool_name);
            }
        }
    }

    /// The whole call arrived: record the protocol's parse of its arguments
    /// (ADR 0114 §1.2). A call whose arguments the adapter never streamed is
    /// captured whole, as a start and an end, before its verdict.
    pub(super) fn capture_tool_call(&mut self, call_id: &str, tool_name: &str, input_json: &str) {
        if self.capture.is_none() {
            return;
        }
        let streamed = self
            .calls
            .values()
            .find(|streamed| streamed.identity.call_id.as_deref() == Some(call_id))
            .map(|streamed| (streamed.identity.clone(), streamed.ended));
        let identity = match streamed {
            Some((identity, true)) => identity,
            Some((identity, false)) => {
                self.capture_tool_input_end(identity.clone(), input_json.to_string());
                identity
            }
            None => {
                let identity = lash_sansio::ToolInputIdentity {
                    ordinal: UNSTREAMED_CALL_ORDINAL_BASE + self.unstreamed_calls,
                    call_id: Some(call_id.to_string()),
                    tool_name: Some(tool_name.to_string()),
                    item_id: None,
                };
                self.unstreamed_calls += 1;
                self.capture_tool_input_start(identity.clone());
                self.capture_tool_input_end(identity.clone(), input_json.to_string());
                identity
            }
        };
        let frame = match serde_json::from_str::<serde_json::Value>(input_json) {
            Ok(arguments) => CaptureFrame::ToolCallParsed {
                call: identity,
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                arguments,
            },
            Err(error) => CaptureFrame::ToolCallUnparseable {
                call: identity,
                parse_error: error.to_string(),
            },
        };
        self.capture_frame(frame);
    }

    /// Persist every staged frame, then release the observations held behind
    /// them, in order. Returns `false` once the capture has failed.
    pub(super) async fn flush(&mut self) -> bool {
        if self.fault.is_some() {
            return false;
        }
        if let Some(capture) = self.capture.as_mut()
            && let Err(error) = capture.flush().await
        {
            self.held.clear();
            self.fault = Some(error);
            return false;
        }
        for event in std::mem::take(&mut self.held) {
            self.cursor.observe(self.event_tx, event);
        }
        true
    }

    /// A provider retry: persist what the attempt emitted, then retract the
    /// attempt, before anything announces the retraction (ADR 0114 §4.2).
    pub(super) async fn reset_attempt(&mut self) -> bool {
        if !self.flush().await {
            return false;
        }
        self.calls.clear();
        self.unstreamed_calls = 0;
        if let Some(capture) = self.capture.as_mut()
            && let Err(error) = capture.reset_attempt().await
        {
            self.fault = Some(error);
            return false;
        }
        true
    }

    /// Whether a capture write failed: publication has stopped.
    pub(super) fn faulted(&self) -> bool {
        self.fault.is_some()
    }

    /// The capture's end state: the fault that stopped it, or the watermark
    /// the step's recorded outcome carries.
    pub(super) fn finish(
        self,
    ) -> Result<Option<lash_core_execution::runtime::CaptureWatermark>, crate::store::StoreError>
    {
        match self.fault {
            Some(fault) => Err(fault),
            None => Ok(self.capture.as_ref().and_then(CaptureWriter::watermark)),
        }
    }
}
