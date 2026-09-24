//! RLM stream mask: suppresses paired `<typescript>` blocks from the visible
//! assistant stream and aborts the provider stream as soon as the closing tag
//! is complete.
//!
//! Registered from `RlmProtocolPlugin::register` via
//! [`register_stream_mask`].

use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use lash_core::PluginRuntimeEvent;
use lash_core::plugin::{
    AssistantStreamFinishedContext, AssistantStreamHookContext, AssistantStreamTransform,
    PluginError, PluginRegistrar,
};

use crate::cell_scan::{
    StreamedCellStart, complete_cell_start, complete_end_tag_span, possible_start_tag_suffix_len,
};
use crate::dialect::TypescriptDialect;

/// Called by [`crate::plugin::RlmProtocolPlugin::register`] when the session is active.
pub fn register_stream_mask(
    reg: &mut PluginRegistrar,
    dialect: Arc<TypescriptDialect>,
) -> Result<(), PluginError> {
    // One provider stream's scan. Only phase 1 touches it: the stream hook
    // fills it and the stream-finished hook hands its end state to the
    // journal and clears it. Phase 2 reads the journaled state alone, so it
    // derives the same response on any worker, after any restart.
    let state = Arc::new(Mutex::new(CellDetector::with_dialect(Arc::clone(&dialect))));

    let stream_state = Arc::clone(&state);
    reg.output()
        .stream(Arc::new(move |ctx: AssistantStreamHookContext| {
            let state = Arc::clone(&stream_state);
            Box::pin(async move {
                let mut detector = state.lock_recover();
                Ok(detector.process_chunk(&ctx.chunk))
            })
        }));

    let finished_state = Arc::clone(&state);
    reg.output()
        .stream_finished(Arc::new(move |ctx: AssistantStreamFinishedContext| {
            let state = Arc::clone(&finished_state);
            Box::pin(async move { state.lock_recover().finish_stream(ctx.reason) })
        }));

    reg.output().response(Arc::new(
        move |ctx: lash_core::plugin::AssistantResponseHookContext| {
            let dialect = Arc::clone(&dialect);
            Box::pin(async move {
                let Some(recorded) = ctx.stream_state else {
                    // Nothing streamed that phase 2 could splice.
                    return Ok(lash_core::plugin::AssistantResponseTransform {
                        response: ctx.response,
                        events: Vec::new(),
                    });
                };
                let mut detector = CellDetector::from_recorded(dialect, recorded)?;
                let events = detector.finish_response();
                let response = transform_final_response(&detector, ctx.response);
                Ok(lash_core::plugin::AssistantResponseTransform { response, events })
            })
        },
    ));

    Ok(())
}

fn transform_final_response(
    detector: &CellDetector,
    mut response: lash_core::LlmResponse,
) -> lash_core::LlmResponse {
    if !matches!(detector.scan, CellScan::Closed { .. }) {
        return response;
    }

    let spliced = detector.spliced_response_text();
    response
        .parts
        .retain(|part| !matches!(part, lash_core::LlmOutputPart::Text { .. }));
    response.parts.push(lash_core::LlmOutputPart::Text {
        text: spliced,
        response_meta: None,
    });
    response
}

/// The scan's three real phases. `pending` exists only while the mask is
/// still deciding whether held bytes become a start tag, and `body` exists
/// only once a cell has opened — so stale pre-cell text cannot append into a
/// body, and a closed cell cannot reopen. The cell-start and cell-end events
/// are each emitted exactly on the transition that creates the state they
/// announce.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum CellScan {
    /// Reading prose; `pending` holds bytes withheld while they might still
    /// become a start tag.
    Scanning { pending: String },
    /// Inside a cell; `body` accumulates cell source until the close tag.
    Body { body: String },
    /// The close tag completed (or an inline cell arrived whole); `body` is
    /// the final cell source the splice renders.
    Closed { body: String },
}

struct CellDetector {
    dialect: Arc<TypescriptDialect>,
    scan: CellScan,
    visible_prose: String,
}

/// The detector's end state as phase 1 journals it: what the response hook
/// needs to splice the cell the stream saw.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedCellScan {
    scan: CellScan,
    visible_prose: String,
}

impl CellDetector {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_dialect(Arc::new(TypescriptDialect::prompt_only(
            lash_lashlang_runtime::LashlangSurface::default(),
        )))
    }

    fn with_dialect(dialect: Arc<TypescriptDialect>) -> Self {
        Self {
            dialect,
            scan: CellScan::Scanning {
                pending: String::new(),
            },
            visible_prose: String::new(),
        }
    }

    /// Rebuild the detector phase 1 journaled, for the response hook.
    fn from_recorded(
        dialect: Arc<TypescriptDialect>,
        recorded: serde_json::Value,
    ) -> Result<Self, PluginError> {
        let RecordedCellScan {
            scan,
            visible_prose,
        } = serde_json::from_value(recorded).map_err(|error| {
            PluginError::Session(format!(
                "the journaled stream-mask state does not decode: {error}"
            ))
        })?;
        Ok(Self {
            dialect,
            scan,
            visible_prose,
        })
    }

    /// End the stream: hand the end state to the journal when the stream
    /// produced a response phase 2 will derive, and clear the detector either
    /// way, so the next stream starts from nothing whether or not phase 2
    /// ever runs.
    ///
    /// A scan still reading prose with nothing held needs no state: the
    /// response hook has nothing to splice and no end-of-response leg to run.
    fn finish_stream(
        &mut self,
        reason: lash_core::plugin::AssistantStreamFinishReason,
    ) -> Result<Option<serde_json::Value>, PluginError> {
        let produced_response = matches!(
            reason,
            lash_core::plugin::AssistantStreamFinishReason::Complete
                | lash_core::plugin::AssistantStreamFinishReason::Aborted
        );
        let scan = std::mem::replace(
            &mut self.scan,
            CellScan::Scanning {
                pending: String::new(),
            },
        );
        let visible_prose = std::mem::take(&mut self.visible_prose);
        if !produced_response
            || matches!(&scan, CellScan::Scanning { pending } if pending.is_empty())
        {
            return Ok(None);
        }
        serde_json::to_value(RecordedCellScan {
            scan,
            visible_prose,
        })
        .map(Some)
        .map_err(|error| {
            PluginError::Session(format!("the stream-mask state does not encode: {error}"))
        })
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.scan = CellScan::Scanning {
            pending: String::new(),
        };
        self.visible_prose.clear();
    }

    fn splice_into_visible(&self, visible: &str) -> String {
        let CellScan::Closed { body } = &self.scan else {
            unreachable!("a splice exists only once the scan has closed");
        };
        self.dialect.render_history_cell(visible, body)
    }

    fn spliced_response_text(&self) -> String {
        self.splice_into_visible(&self.visible_prose)
    }

    fn process_chunk(&mut self, chunk: &str) -> AssistantStreamTransform {
        match self.scan {
            CellScan::Closed { .. } => {
                return AssistantStreamTransform {
                    chunk: String::new(),
                    reasoning_deltas: Vec::new(),
                    events: Vec::new(),
                    abort_stream: false,
                };
            }
            CellScan::Body { .. } => {
                return self.capture_cell_body_chunk(chunk, String::new(), Vec::new());
            }
            CellScan::Scanning { .. } => {}
        }

        let CellScan::Scanning { pending } = &mut self.scan else {
            unreachable!("closed and body scans returned above");
        };
        pending.push_str(chunk);

        // `allow_eof` stays false for the same reason it does inside a body: a
        // chunk boundary is not the end of the response, and an inline cell read
        // from a half-arrived line would make the provider's framing decide what
        // executed. [`Self::finish_response`] runs the EOF leg.
        match complete_cell_start(pending, false, self.dialect.cell_tags()) {
            Some(StreamedCellStart::Block(span)) => {
                let prose_before = pending[..span.start_tag_start].to_string();
                self.visible_prose.push_str(&prose_before);
                let body_suffix = pending[span.body_start..span.body_end].to_string();
                self.scan = CellScan::Body {
                    body: String::new(),
                };

                let events = vec![self.start_event()];

                return self.capture_cell_body_chunk(&body_suffix, prose_before, events);
            }
            // An inline cell arrives already closed: there is no later body to
            // wait for, so the mask opens and closes it in one step and aborts
            // the provider stream on the same boundary a block cell does.
            Some(StreamedCellStart::Inline(span)) => {
                let prose_before = self.take_inline_cell(span);
                return AssistantStreamTransform {
                    chunk: prose_before,
                    reasoning_deltas: Vec::new(),
                    events: vec![self.start_event(), self.end_event()],
                    abort_stream: true,
                };
            }
            None => {}
        }

        let CellScan::Scanning { pending } = &mut self.scan else {
            unreachable!("cell starts transitioned above");
        };
        let safe_len =
            pending.len() - possible_start_tag_suffix_len(pending, self.dialect.cell_tags());
        if safe_len == 0 {
            return AssistantStreamTransform {
                chunk: String::new(),
                reasoning_deltas: Vec::new(),
                events: Vec::new(),
                abort_stream: false,
            };
        }

        let flushed = pending[..safe_len].to_string();
        *pending = pending[safe_len..].to_string();
        self.visible_prose.push_str(&flushed);
        AssistantStreamTransform {
            chunk: flushed,
            reasoning_deltas: Vec::new(),
            events: Vec::new(),
            abort_stream: false,
        }
    }

    fn capture_cell_body_chunk(
        &mut self,
        chunk: &str,
        visible_chunk: String,
        mut events: Vec<PluginRuntimeEvent>,
    ) -> AssistantStreamTransform {
        let CellScan::Body { body } = &mut self.scan else {
            unreachable!("body chunks are captured only inside a cell");
        };
        body.push_str(chunk);
        // `allow_eof` stays false: a chunk boundary is not response EOF, which
        // is what makes the mask — not the provider's stop — own the boundary.
        let abort_stream =
            if let Some(span) = complete_end_tag_span(body, false, self.dialect.cell_tags()) {
                body.truncate(span.body_end);
                let body = std::mem::take(body);
                self.scan = CellScan::Closed { body };
                events.push(self.end_event());
                true
            } else {
                false
            };

        AssistantStreamTransform {
            chunk: visible_chunk,
            reasoning_deltas: Vec::new(),
            events,
            abort_stream,
        }
    }

    /// Consume the inline cell `span` addresses out of the scan's pending
    /// text, returning the prose that preceded it on the way.
    fn take_inline_cell(&mut self, span: crate::cell_scan::CellSpan) -> String {
        let CellScan::Scanning { pending } = &mut self.scan else {
            unreachable!("an inline cell is taken only while scanning");
        };
        let prose_before = pending[..span.start_tag_start].to_string();
        self.visible_prose.push_str(&prose_before);
        let body = pending[span.body_start..span.body_end].to_string();
        self.scan = CellScan::Closed { body };
        prose_before
    }

    fn finish_response(&mut self) -> Vec<PluginRuntimeEvent> {
        match &mut self.scan {
            CellScan::Closed { .. } => Vec::new(),
            CellScan::Scanning { pending } => {
                // No cell has opened and no more text is coming, so a held line
                // that could still have closed as an inline cell now either is
                // one or is prose. This is the inline shape's EOF leg, the
                // counterpart of the one `complete_end_tag_span` has always had
                // for the block shape.
                let tags = self.dialect.cell_tags();
                if let Some(StreamedCellStart::Inline(span)) =
                    complete_cell_start(pending, true, tags)
                {
                    self.take_inline_cell(span);
                    return vec![self.start_event(), self.end_event()];
                }
                // Prose after all. It was withheld from the live deltas while
                // the line might still have become a cell, and this hook has no
                // delta to emit it on; recording it as visible prose is what
                // keeps the detector's own account of the response whole. The
                // transcript is unaffected either way — with no cell, the
                // response passes through untransformed, tail included.
                let held = std::mem::take(pending);
                self.visible_prose.push_str(&held);
                Vec::new()
            }
            CellScan::Body { body } => {
                // Genuine response EOF, so a closing tag at the buffer end
                // counts.
                let Some(span) = complete_end_tag_span(body, true, self.dialect.cell_tags()) else {
                    return Vec::new();
                };
                body.truncate(span.body_end);
                let body = std::mem::take(body);
                self.scan = CellScan::Closed { body };
                vec![self.end_event()]
            }
        }
    }

    fn start_event(&mut self) -> PluginRuntimeEvent {
        PluginRuntimeEvent::Custom {
            name: self.dialect.stream_cell_start_event_name().to_string(),
            payload: serde_json::json!({}),
        }
    }

    fn end_event(&mut self) -> PluginRuntimeEvent {
        PluginRuntimeEvent::Custom {
            name: self.dialect.stream_cell_end_event_name().to_string(),
            payload: serde_json::json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell_scan::first_cell_span;

    fn first_cell_span_for_tests(text: &str) -> Option<crate::cell_scan::CellSpan> {
        first_cell_span(
            text,
            crate::dialect::CellTags {
                open: "<typescript>",
                close: "</typescript>",
            },
        )
    }

    #[test]
    fn prose_streams_as_assistant_text_before_cell() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Hello, here's my plan.\n\n");
        assert_eq!(t.chunk, "Hello, here's my plan.\n\n");
        assert!(t.reasoning_deltas.is_empty());
        assert!(t.events.is_empty());
        assert!(!t.abort_stream);
    }

    #[test]
    fn short_prose_without_newline_streams_immediately() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Hi - what can I help with?");
        assert_eq!(t.chunk, "Hi - what can I help with?");
        assert!(pending(&d).is_empty());
    }

    #[test]
    fn possible_start_tag_suffix_is_held() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Plan.\n<type");
        assert_eq!(t.chunk, "Plan.\n");
        assert_eq!(pending(&d), "<type");

        let t = d.process_chunk("script>\n");
        assert_eq!(t.chunk, "");
        assert!(inside(&d));
        assert!(!closed(&d));
        assert_eq!(t.events.len(), 1);
        assert!(!t.abort_stream);
    }

    #[test]
    fn indented_start_tag_split_after_whitespace_is_held() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Plan.\n  ");
        assert_eq!(t.chunk, "Plan.\n");
        assert_eq!(pending(&d), "  ");

        let t = d.process_chunk("<typescript>\nfinish 1");
        assert_eq!(t.chunk, "");
        assert!(inside(&d));
        assert!(!closed(&d));
        assert_eq!(body(&d), "finish 1");
    }

    #[test]
    fn start_tag_and_body_in_same_chunk_preserves_body_and_does_not_abort_before_close() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Thinking...\n\n<typescript>\ncode\n```markdown\ninside\n```\n");
        assert_eq!(t.chunk, "Thinking...\n\n");
        assert!(inside(&d));
        assert_eq!(body(&d), "code\n```markdown\ninside\n```\n");
        assert!(!t.abort_stream);
    }

    #[test]
    fn accepted_cell_is_independent_of_split_immediately_after_end_tag() {
        fn accepted(chunks: &[&str]) -> Option<String> {
            let mut detector = CellDetector::new();
            for chunk in chunks {
                if detector.process_chunk(chunk).abort_stream {
                    break;
                }
            }
            detector.finish_response();
            closed(&detector).then(|| detector.spliced_response_text())
        }

        for raw in [
            "<typescript>\nprint 1\n</typescript>suffix",
            "<typescript>\nprint 1\n</typescript>",
            "<typescript>\nprint 1\n</typescript>\nsuffix",
        ] {
            let expected = accepted(&[raw]);
            for split in raw
                .char_indices()
                .map(|(index, _)| index)
                .chain([raw.len()])
            {
                assert_eq!(
                    accepted(&[&raw[..split], &raw[split..]]),
                    expected,
                    "accepted cell changed at byte split {split} for {raw:?}"
                );
            }
        }

        let malformed = "<typescript>\nprint 1\n</typescript>suffix";
        let after_tag = malformed.find("suffix").expect("suffix boundary");
        assert_eq!(
            accepted(&[&malformed[..after_tag], &malformed[after_tag..]]),
            None
        );
    }

    #[test]
    fn body_after_start_tag_is_suppressed_until_close() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<typescript>\n").chunk, "");
        let t = d.process_chunk("finish \"hi\"\n");
        assert_eq!(t.chunk, "");
        assert!(!t.abort_stream);
        assert_eq!(body(&d), "finish \"hi\"\n");
    }

    /// A one-line cell is masked, not shown: its source never reaches the
    /// visible stream, and the splice hands history the canonical block form.
    ///
    /// Terminated here; the same reply without its newline is the EOF leg below.
    #[test]
    fn one_line_cell_is_masked_and_normalized() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Checking.\n<typescript>finish 1</typescript>\n");
        assert_eq!(t.chunk, "Checking.\n");
        assert!(t.abort_stream);
        assert!(closed(&d));
        assert_eq!(body(&d), "finish 1");
        assert_eq!(
            event_names(&t.events),
            vec!["rlm_typescript_cell_start", "rlm_typescript_cell_end"]
        );
        assert_eq!(
            d.spliced_response_text(),
            "Checking.\n<typescript>\nfinish 1\n</typescript>"
        );
    }

    /// A one-line cell that is the response's last line closes at EOF, where the
    /// line is known to be whole.
    #[test]
    fn one_line_cell_at_response_end_closes_on_the_eof_leg() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Checking.\n<typescript>finish 1</typescript>");
        assert_eq!(t.chunk, "Checking.\n");
        assert!(!t.abort_stream, "an unfinished line decides nothing yet");
        assert!(!closed(&d));

        let events = d.finish_response();
        assert!(closed(&d));
        assert_eq!(body(&d), "finish 1");
        assert_eq!(
            event_names(&events),
            vec!["rlm_typescript_cell_start", "rlm_typescript_cell_end"]
        );
        assert_eq!(
            d.spliced_response_text(),
            "Checking.\n<typescript>\nfinish 1\n</typescript>"
        );
    }

    /// The source of a one-line cell must not be flushed as prose while the
    /// line is still arriving.
    #[test]
    fn one_line_cell_split_mid_source_holds_the_line() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<typescript>fin").chunk, "");
        let t = d.process_chunk("ish 1</typescript>\n");
        assert_eq!(t.chunk, "");
        assert!(t.abort_stream);
        assert_eq!(body(&d), "finish 1");
    }

    /// Held prose reaches the detector's account of the response at EOF instead
    /// of vanishing with the buffer, in both the tag-prefix and opened-line
    /// shapes.
    #[test]
    fn an_unfinished_line_that_is_prose_is_released_at_response_end() {
        for raw in [
            "<typescript> is the opening tag.",
            "The tag is <lash",
            "<typescript>print 1</typescript> ok",
        ] {
            let mut d = CellDetector::new();
            d.process_chunk(raw);
            let events = d.finish_response();
            assert!(events.is_empty(), "{raw:?} is not a cell");
            assert!(!closed(&d), "{raw:?} is not a cell");
            assert!(pending(&d).is_empty(), "{raw:?} left text held");
            assert_eq!(d.visible_prose, raw, "{raw:?} lost its tail");
        }
    }

    /// What executed may not depend on where the provider split its chunks —
    /// including for a line whose closing tag is *not* the end of it, the shape
    /// that reads as a finished cell in every prefix.
    #[test]
    fn one_line_cell_is_independent_of_where_the_stream_splits() {
        fn accepted(chunks: &[&str]) -> Option<String> {
            let mut detector = CellDetector::new();
            for chunk in chunks {
                if detector.process_chunk(chunk).abort_stream {
                    break;
                }
            }
            detector.finish_response();
            closed(&detector).then(|| detector.spliced_response_text())
        }

        for (raw, expected) in [
            (
                "Plan.\n<typescript>finish 1</typescript>",
                Some("Plan.\n<typescript>\nfinish 1\n</typescript>".to_string()),
            ),
            (
                "Plan.\n<typescript>finish 1</typescript>\ntail",
                Some("Plan.\n<typescript>\nfinish 1\n</typescript>".to_string()),
            ),
            // Prose, at every split: a trailer after the closing tag means the
            // line was never a cell.
            ("Plan.\n<typescript>print 1</typescript> ok\n", None),
            ("Plan.\n<typescript>print 1</typescript> ok", None),
        ] {
            assert_eq!(accepted(&[raw]), expected, "whole: {raw:?}");
            for split in raw
                .char_indices()
                .map(|(index, _)| index)
                .chain([raw.len()])
            {
                assert_eq!(
                    accepted(&[&raw[..split], &raw[split..]]),
                    expected,
                    "{raw:?} changed at byte split {split}"
                );
            }
        }
    }

    #[test]
    fn inline_start_tag_text_does_not_trigger() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Use <typescript> here.\n");
        assert_eq!(t.chunk, "Use <typescript> here.\n");
        assert!(!inside(&d));
        assert!(t.events.is_empty());
    }

    #[test]
    fn incomplete_start_tag_can_become_visible_prose() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<typescript>").chunk, "");
        let t = d.process_chunk(" here\n");
        assert_eq!(t.chunk, "<typescript> here\n");
        assert!(!inside(&d));
    }

    #[test]
    fn reset_prevents_cross_response_leak() {
        let mut d = CellDetector::new();
        d.process_chunk("Hi! How can I help you?");
        d.reset();

        let t = d.process_chunk("New response.\n\n<typescript>\ncode\n");
        assert_eq!(t.chunk, "New response.\n\n");
        assert!(!t.chunk.contains("How can I help"));
    }

    #[test]
    fn reset_after_partial_cell_isolates_next_response() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<typescript>\nfinish 1");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(inside(&d));
        assert!(!closed(&d));
        assert_eq!(body(&d), "finish 1");

        d.reset();

        let t = d.process_chunk("Next response.");
        assert_eq!(t.chunk, "Next response.");
        assert!(!inside(&d));
        assert!(!closed(&d));
        assert!(body(&d).is_empty());
    }

    #[test]
    fn reset_after_closed_cell_isolates_next_response() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<typescript>\nfinish 1\n</typescript>\n");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(t.abort_stream);
        assert!(closed(&d));

        d.reset();

        let t = d.process_chunk("Next response.");
        assert_eq!(t.chunk, "Next response.");
        assert!(!t.abort_stream);
        assert!(!inside(&d));
        assert!(!closed(&d));
    }

    /// The detector is session-scoped, so a turn whose phase 2 never ran must
    /// not be able to suppress the turn after it. The stream's end hands the
    /// state to the journal and clears the detector, whatever happens next.
    #[test]
    fn a_finished_stream_leaves_nothing_for_the_next_turn() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<typescript>\nfinish 1\n</typescript>\n");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(t.abort_stream);
        assert!(closed(&d));

        let recorded = d
            .finish_stream(lash_core::plugin::AssistantStreamFinishReason::Aborted)
            .expect("encode the end state");
        assert!(recorded.is_some(), "a closed cell is phase 2's to splice");

        let t = d.process_chunk("Next turn prose.");
        assert_eq!(
            t.chunk, "Next turn prose.",
            "the next turn's prose must reach the user"
        );
        assert!(!t.abort_stream);
        assert!(!closed(&d));
        assert!(body(&d).is_empty());
        assert_eq!(d.visible_prose, "Next turn prose.");
    }

    /// A stream that produced no response hands phase 2 nothing, and neither
    /// does one that never held a byte back.
    #[test]
    fn only_a_stream_with_something_to_splice_records_state() {
        for reason in [
            lash_core::plugin::AssistantStreamFinishReason::AttemptReset,
            lash_core::plugin::AssistantStreamFinishReason::Cancelled,
            lash_core::plugin::AssistantStreamFinishReason::ProviderError,
        ] {
            let mut d = CellDetector::new();
            d.process_chunk("Visible.\n<typescript>\nfinish 1\n</typescript>\n");
            assert_eq!(d.finish_stream(reason).expect("finish"), None, "{reason:?}");
            assert!(!inside(&d));
        }
        let mut d = CellDetector::new();
        d.process_chunk("Prose only.");
        assert_eq!(
            d.finish_stream(lash_core::plugin::AssistantStreamFinishReason::Complete)
                .expect("finish"),
            None
        );
    }

    /// Phase 2 redriven alone, on a worker whose detector never saw the
    /// stream, derives exactly the response the streaming worker would have:
    /// it reads the journaled end state, not plugin memory.
    #[test]
    fn phase_two_on_another_worker_splices_from_the_journaled_state() {
        let chunks = [
            "Visible before",
            " code.\n<type",
            "script>\nfinish ",
            "\"ok\"\n</typescript>\nignored",
        ];
        let raw_final = "Visible before code.\n<typescript>\nfinish \"ok\"\n</typescript>\nignored";

        // The worker that streamed: the old in-memory derivation.
        let (mut streamed, _) = stream_chunks(&chunks);
        let in_memory_events = streamed.finish_response();
        let in_memory = transform_final_response(&streamed, response_with_text(raw_final));

        // The journal: phase 1's recorded end state, through its JSON bytes.
        let (mut phase_one, _) = stream_only(&chunks);
        let recorded = phase_one
            .finish_stream(lash_core::plugin::AssistantStreamFinishReason::Aborted)
            .expect("encode")
            .expect("a closed cell records state");
        let journaled: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&recorded).expect("serialize"))
                .expect("deserialize");

        // Another worker: a fresh detector rebuilt from the journal alone.
        let mut elsewhere = CellDetector::from_recorded(
            Arc::new(TypescriptDialect::prompt_only(
                lash_lashlang_runtime::LashlangSurface::default(),
            )),
            journaled,
        )
        .expect("decode");
        let events = elsewhere.finish_response();
        let derived = transform_final_response(&elsewhere, response_with_text(raw_final));

        assert_eq!(derived.parts, in_memory.parts);
        assert_eq!(event_names(&events), event_names(&in_memory_events));
        assert_eq!(
            derived.full_text(),
            "Visible before code.\n<typescript>\nfinish \"ok\"\n</typescript>"
        );
    }

    /// The end-of-response leg runs from the journal too: an inline cell the
    /// stream was still holding closes in phase 2.
    #[test]
    fn a_held_inline_cell_closes_on_the_journaled_eof_leg() {
        let (mut d, visible) = stream_only(&["Checking.\n<typescript>finish 1</typescript>"]);
        assert_eq!(visible, "Checking.\n");
        let recorded = d
            .finish_stream(lash_core::plugin::AssistantStreamFinishReason::Complete)
            .expect("encode")
            .expect("held bytes record state");
        let mut phase_two =
            CellDetector::from_recorded(Arc::clone(&d.dialect), recorded).expect("decode");
        let events = phase_two.finish_response();
        assert_eq!(
            event_names(&events),
            vec!["rlm_typescript_cell_start", "rlm_typescript_cell_end"]
        );
        assert!(closed(&phase_two));
    }

    #[test]
    fn close_tag_split_across_chunks_aborts_stream() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<typescript>\nfinish 1\n</type").chunk, "");

        let t = d.process_chunk("script>\n");
        assert_eq!(t.chunk, "");
        assert!(t.abort_stream);
        assert!(closed(&d));
        assert_eq!(body(&d), "finish 1");
        assert_eq!(event_names(&t.events), vec!["rlm_typescript_cell_end"]);
    }

    #[test]
    fn close_tag_plus_trailing_prose_in_same_chunk_aborts_and_drops_suffix() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<typescript>\nfinish 1\n</typescript>\nTrailing prose.");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(t.abort_stream);
        assert!(closed(&d));
        assert_eq!(body(&d), "finish 1");
        assert_eq!(
            event_names(&t.events),
            vec!["rlm_typescript_cell_start", "rlm_typescript_cell_end"]
        );
        assert_eq!(
            d.spliced_response_text(),
            "Visible.\n<typescript>\nfinish 1\n</typescript>"
        );
    }

    #[test]
    fn client_abort_preserves_preceding_signed_reasoning_part() {
        let mut detector = CellDetector::new();
        let transformed =
            detector.process_chunk("Visible.\n<typescript>\nprint \"hi\"\n</typescript>\nignored");
        assert!(transformed.abort_stream);

        let replay = lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-1".to_string()),
            encrypted_content: None,
            signature: Some("signed".to_string()),
            redacted: false,
            summary: vec!["thought".to_string()],
            ..Default::default()
        };
        let response = transform_final_response(
            &detector,
            lash_core::LlmResponse {
                parts: vec![
                    lash_core::LlmOutputPart::Reasoning {
                        text: "thought".to_string(),
                        replay: Some(replay.clone()),
                    },
                    lash_core::LlmOutputPart::Text {
                        text: "provider partial".to_string(),
                        response_meta: None,
                    },
                ],
                ..Default::default()
            },
        );

        assert!(matches!(
            response.parts.first(),
            Some(lash_core::LlmOutputPart::Reasoning {
                replay: Some(actual),
                ..
            }) if actual == &replay
        ));
        assert_eq!(
            response.full_text(),
            "Visible.\n<typescript>\nprint \"hi\"\n</typescript>"
        );
    }

    #[test]
    fn incomplete_block_does_not_abort_and_does_not_close() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<typescript>\nfinish 1");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(!t.abort_stream);
        assert!(inside(&d));
        assert!(!closed(&d));
        assert_eq!(body(&d), "finish 1");
    }

    fn stream_chunks(chunks: &[&str]) -> (CellDetector, String) {
        let (mut d, visible) = stream_only(chunks);
        d.finish_response();
        (d, visible)
    }

    /// Phase 1 alone: the chunks, without the response hook's EOF leg.
    fn stream_only(chunks: &[&str]) -> (CellDetector, String) {
        let mut d = CellDetector::new();
        let mut visible = String::new();
        for chunk in chunks {
            let t = d.process_chunk(chunk);
            visible.push_str(&t.chunk);
            assert!(t.reasoning_deltas.is_empty());
            if t.abort_stream {
                break;
            }
        }
        (d, visible)
    }

    fn response_with_text(text: &str) -> lash_core::LlmResponse {
        lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..lash_core::LlmResponse::default()
        }
    }

    #[test]
    fn final_response_splice_reconstructs_cell_with_exact_body() {
        let (d, visible) = stream_chunks(&[
            "Quick check.\n\n<typescript>\n",
            "print \"hi\"\n",
            "finish 1\n</typescript>",
        ]);
        assert_eq!(visible, "Quick check.\n\n");
        let spliced = d.spliced_response_text();
        let span = first_cell_span_for_tests(&spliced).expect("spliced cell parses");
        let code = &spliced[span.body_start..span.body_end];
        assert_eq!(code, "print \"hi\"\nfinish 1");
    }

    #[test]
    fn final_response_splice_ignores_raw_provider_full_text_with_suffix() {
        let raw_final = "Visible before code.\n<typescript>\nfinish \"ok\"\n</typescript>\nignored";
        let (d, visible) = stream_chunks(&[
            "Visible before",
            " code.\n<type",
            "script>\nfinish ",
            "\"ok\"\n</typescript>\nignored",
        ]);
        assert_eq!(visible, "Visible before code.\n");

        // This is the production shape for streaming providers that return
        // their original raw final text after the stream hook has already
        // suppressed the cell body. Using `raw_final` as the splice base would
        // keep suffix text that the stream abort intentionally dropped.
        assert!(raw_final.contains("ignored"));
        let spliced = d.spliced_response_text();
        assert_eq!(
            spliced,
            "Visible before code.\n<typescript>\nfinish \"ok\"\n</typescript>"
        );
        let span = first_cell_span_for_tests(&spliced).expect("spliced cell parses");
        assert_eq!(&spliced[span.body_start..span.body_end], "finish \"ok\"");
        assert!(!spliced.contains("ignored"));
    }

    #[test]
    fn final_response_transform_never_splices_using_raw_provider_text() {
        let raw_final = "Visible before code.\n<typescript>\nfinish \"ok\"\n</typescript>\nignored";
        let (d, visible) = stream_chunks(&[
            "Visible before",
            " code.\n%%",
            " ordinary prose\n<typescript>\nfinish ",
            "\"ok\"\n</typescript>\nignored",
        ]);
        assert_eq!(visible, "Visible before code.\n%% ordinary prose\n");

        let response = transform_final_response(&d, response_with_text(raw_final));
        assert_eq!(
            response.full_text(),
            "Visible before code.\n%% ordinary prose\n<typescript>\nfinish \"ok\"\n</typescript>"
        );
        assert_eq!(response.full_text().matches("<typescript>").count(), 1);
        assert_eq!(response.full_text().matches("</typescript>").count(), 1);
        let span = first_cell_span_for_tests(&response.full_text()).expect("cell parses");
        assert_eq!(
            &response.full_text()[span.body_start..span.body_end],
            "finish \"ok\""
        );
        assert!(
            !response.full_text()[span.end_tag_end..].contains("ignored"),
            "suffix after the close tag must not survive streaming abort normalization"
        );
        let text_parts = response
            .parts
            .iter()
            .filter_map(|part| match part {
                lash_core::LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(text_parts, vec![response.full_text().as_str()]);
    }

    #[test]
    fn final_response_transform_replaces_raw_text_parts_but_preserves_reasoning_parts() {
        let raw_final = "Plan.\n<typescript>\nfinish \"ok\"\n</typescript>\nignored";
        let (d, visible) =
            stream_chunks(&["Plan.\n<type", "script>\nfinish \"ok\"\n</typescript>"]);
        assert_eq!(visible, "Plan.\n");
        let response = lash_core::LlmResponse {
            execution_evidence: Some(lash_core::ExecutionEvidence {
                served_model: Some("provider/model".to_string()),
                provider_response_id: Some("response-1".to_string()),
                provider_request_id: None,
                reasoning_output_tokens: Some(0),
                provider_finish_reason: Some("stop".to_string()),
                collection_interruption: None,
            }),
            parts: vec![
                lash_core::LlmOutputPart::Text {
                    text: raw_final.to_string(),
                    response_meta: None,
                },
                lash_core::LlmOutputPart::Reasoning {
                    text: "brief reasoning summary".to_string(),
                    replay: None,
                },
                lash_core::LlmOutputPart::Text {
                    text: "stale provider text".to_string(),
                    response_meta: None,
                },
            ],
            response_metadata: Default::default(),
            ..lash_core::LlmResponse::default()
        };

        let response = transform_final_response(&d, response);
        assert_eq!(
            response.full_text(),
            "Plan.\n<typescript>\nfinish \"ok\"\n</typescript>"
        );
        assert_eq!(response.full_text().matches("<typescript>").count(), 1);
        assert_eq!(
            response
                .execution_evidence
                .as_ref()
                .and_then(|evidence| evidence.provider_response_id.as_deref()),
            Some("response-1")
        );
        assert!(matches!(
            response.parts.first(),
            Some(lash_core::LlmOutputPart::Reasoning { text, .. })
                if text == "brief reasoning summary"
        ));
        let text_parts = response
            .parts
            .iter()
            .filter_map(|part| match part {
                lash_core::LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(text_parts, vec![response.full_text().as_str()]);
    }

    #[test]
    fn final_response_transform_is_noop_without_detected_cell() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("Visible only").chunk, "Visible only");

        let response = response_with_text("Visible only");
        let transformed = transform_final_response(&d, response.clone());
        assert_eq!(transformed.full_text(), response.full_text());
        assert_eq!(transformed.parts, response.parts);
    }

    #[test]
    fn final_response_splice_also_handles_already_transformed_visible_text() {
        let (d, visible) =
            stream_chunks(&["Visible.\n", "<typescript>\nfinish \"ok\"\n</typescript>"]);
        assert_eq!(visible, "Visible.\n");

        let spliced = d.spliced_response_text();
        assert_eq!(
            spliced,
            "Visible.\n<typescript>\nfinish \"ok\"\n</typescript>"
        );
        let span = first_cell_span_for_tests(&spliced).expect("spliced cell parses");
        assert_eq!(&spliced[span.body_start..span.body_end], "finish \"ok\"");
    }

    #[test]
    fn final_response_splice_preserves_start_tag_line_split_across_chunks() {
        let (d, visible) = stream_chunks(&[
            "Line one.",
            "\n  ",
            "<types",
            "cript>  \n",
            "payload = r\"\"\"```markdown\nbody\n```\"\"\"\n",
            "finish payload\n  </types",
            "cript>  ",
        ]);
        assert_eq!(visible, "Line one.\n");

        let spliced = d.spliced_response_text();
        assert_eq!(
            spliced,
            "Line one.\n<typescript>\npayload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload\n</typescript>"
        );
        let span = first_cell_span_for_tests(&spliced).expect("spliced cell parses");
        assert_eq!(
            &spliced[span.body_start..span.body_end],
            "payload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload"
        );
    }

    #[test]
    fn start_tag_only_without_newline_is_left_to_final_parser() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("<typescript>");
        assert_eq!(t.chunk, "");
        assert!(!inside(&d));
        assert_eq!(d.splice_or_visible_for_test(""), "<typescript>");
    }

    #[test]
    fn final_response_transform_is_noop_for_incomplete_streamed_block() {
        let mut d = CellDetector::new();
        assert_eq!(
            d.process_chunk("Visible.\n<typescript>\nfinish 1").chunk,
            "Visible.\n"
        );
        assert!(inside(&d));
        assert!(!closed(&d));

        let response = response_with_text("Visible.\n<typescript>\nfinish 1");
        let transformed = transform_final_response(&d, response.clone());
        assert_eq!(transformed.full_text(), response.full_text());
        assert_eq!(transformed.parts, response.parts);
    }

    #[test]
    fn old_percent_marker_streams_as_plain_prose() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("%%typescript\nfinish 1\n");
        assert_eq!(t.chunk, "%%typescript\nfinish 1\n");
        assert!(!inside(&d));
        assert!(!t.abort_stream);
    }

    /// The end event is emitted exactly on the transition to `Closed` — once —
    /// on both the block path and the inline path, and never again when the
    /// response hook or late chunks revisit the closed scan.
    #[test]
    fn cell_end_event_is_emitted_exactly_once_on_inline_and_block_paths() {
        fn end_count(chunks: &[&str]) -> usize {
            let mut d = CellDetector::new();
            let mut names: Vec<String> = Vec::new();
            for chunk in chunks {
                names.extend(
                    event_names(&d.process_chunk(chunk).events)
                        .into_iter()
                        .map(str::to_string),
                );
            }
            names.extend(
                event_names(&d.finish_response())
                    .into_iter()
                    .map(str::to_string),
            );
            // A second phase-2 pass must not re-emit either.
            names.extend(
                event_names(&d.finish_response())
                    .into_iter()
                    .map(str::to_string),
            );
            names
                .iter()
                .filter(|name| name.as_str() == "rlm_typescript_cell_end")
                .count()
        }

        // Block path: close tag mid-stream, then a late chunk and phase 2.
        assert_eq!(
            end_count(&[
                "Visible.\n<typescript>\nfinish 1\n</typescript>\n",
                "trailing chunk after close",
            ]),
            1,
            "block path"
        );
        // Inline path: the cell opens and closes in one transition, then EOF.
        assert_eq!(
            end_count(&["Checking.\n<typescript>finish 1</typescript>\n"]),
            1,
            "inline path"
        );
        // Inline at the EOF leg: the same once-only guarantee when the close
        // is recognized at response end rather than on a chunk.
        assert_eq!(
            end_count(&["Checking.\n<typescript>finish 1</typescript>"]),
            1,
            "eof inline path"
        );
    }

    impl CellDetector {
        fn splice_or_visible_for_test(&self, visible: &str) -> String {
            match &self.scan {
                CellScan::Scanning { pending } => {
                    let mut out = visible.to_string();
                    out.push_str(pending);
                    out
                }
                CellScan::Body { .. } | CellScan::Closed { .. } => {
                    self.splice_into_visible(visible)
                }
            }
        }
    }

    /// Discriminant assertions for the scan phases the old flag pair encoded.
    fn inside(d: &CellDetector) -> bool {
        !matches!(d.scan, CellScan::Scanning { .. })
    }

    fn closed(d: &CellDetector) -> bool {
        matches!(d.scan, CellScan::Closed { .. })
    }

    fn pending(d: &CellDetector) -> &str {
        match &d.scan {
            CellScan::Scanning { pending } => pending,
            _ => "",
        }
    }

    fn body(d: &CellDetector) -> &str {
        match &d.scan {
            CellScan::Scanning { .. } => "",
            CellScan::Body { body } | CellScan::Closed { body } => body,
        }
    }

    fn event_names(events: &[PluginRuntimeEvent]) -> Vec<&str> {
        events
            .iter()
            .map(|event| match event {
                PluginRuntimeEvent::Custom { name, .. } => name.as_str(),
                _ => panic!("unexpected event: {event:?}"),
            })
            .collect()
    }
}
