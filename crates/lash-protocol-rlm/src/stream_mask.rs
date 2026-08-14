//! RLM stream mask: suppresses paired `<lashlang>` blocks from the visible
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
    complete_end_tag_span, complete_start_tag_span, possible_start_tag_suffix_len,
};
#[cfg(test)]
use crate::dialect::LashlangDialect;
use crate::dialect::RlmDialect;

/// Install the stream-mask hooks on the given registrar. Called by
/// [`crate::plugin::RlmProtocolPlugin::register`] when the session is active.
pub fn register_stream_mask(
    reg: &mut PluginRegistrar,
    dialect: Arc<dyn RlmDialect>,
) -> Result<(), PluginError> {
    let state = Arc::new(Mutex::new(CellDetector::with_dialect(dialect)));

    let stream_state = Arc::clone(&state);
    reg.output()
        .stream(Arc::new(move |ctx: AssistantStreamHookContext| {
            let state = Arc::clone(&stream_state);
            Box::pin(async move {
                let mut detector = state.lock_recover();
                Ok(detector.process_chunk(&ctx.chunk))
            })
        }));

    let response_state = Arc::clone(&state);
    reg.output().response(Arc::new(
        move |ctx: lash_core::plugin::AssistantResponseHookContext| {
            let state = Arc::clone(&response_state);
            Box::pin(async move {
                let response = {
                    let mut detector = state.lock_recover();
                    let events = detector.finish_response();
                    let response = transform_final_response(&detector, ctx.response);
                    detector.reset();
                    (response, events)
                };
                Ok(lash_core::plugin::AssistantResponseTransform {
                    response: response.0,
                    events: response.1,
                })
            })
        },
    ));

    let cleanup_state = Arc::clone(&state);
    reg.output()
        .stream_finished(Arc::new(move |ctx: AssistantStreamFinishedContext| {
            let state = Arc::clone(&cleanup_state);
            Box::pin(async move {
                let _reason = ctx.reason;
                let mut detector = state.lock_recover();
                detector.reset();
                Ok(())
            })
        }));

    Ok(())
}

fn transform_final_response(
    detector: &CellDetector,
    mut response: lash_core::LlmResponse,
) -> lash_core::LlmResponse {
    if !detector.cell_closed {
        return response;
    }

    let spliced = detector.spliced_response_text();
    response.full_text = spliced.clone();
    response
        .parts
        .retain(|part| !matches!(part, lash_core::LlmOutputPart::Text { .. }));
    response.parts.push(lash_core::LlmOutputPart::Text {
        text: spliced,
        response_meta: None,
    });
    response
}

struct CellDetector {
    dialect: Arc<dyn RlmDialect>,
    pending: String,
    inside_cell: bool,
    cell_closed: bool,
    emitted_start: bool,
    emitted_end: bool,
    visible_prose: String,
    cell_body: String,
}

impl CellDetector {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_dialect(Arc::new(LashlangDialect::prompt_only(
            lash_lashlang_runtime::LashlangSurface::default(),
        )))
    }

    fn with_dialect(dialect: Arc<dyn RlmDialect>) -> Self {
        Self {
            dialect,
            pending: String::new(),
            inside_cell: false,
            cell_closed: false,
            emitted_start: false,
            emitted_end: false,
            visible_prose: String::new(),
            cell_body: String::new(),
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.inside_cell = false;
        self.cell_closed = false;
        self.emitted_start = false;
        self.emitted_end = false;
        self.visible_prose.clear();
        self.cell_body.clear();
    }

    fn splice_into_visible(&self, visible: &str) -> String {
        debug_assert!(self.cell_closed);
        self.dialect.render_history_cell(visible, &self.cell_body)
    }

    fn spliced_response_text(&self) -> String {
        self.splice_into_visible(&self.visible_prose)
    }

    fn process_chunk(&mut self, chunk: &str) -> AssistantStreamTransform {
        if self.cell_closed {
            return AssistantStreamTransform {
                chunk: String::new(),
                reasoning_deltas: Vec::new(),
                events: Vec::new(),
                abort_stream: false,
            };
        }

        if self.inside_cell {
            return self.capture_cell_body_chunk(chunk, String::new(), Vec::new());
        }

        self.pending.push_str(chunk);

        if let Some(span) = complete_start_tag_span(&self.pending, self.dialect.cell_tags()) {
            self.inside_cell = true;
            let prose_before = self.pending[..span.start_tag_start].to_string();
            self.visible_prose.push_str(&prose_before);
            let body_suffix = self.pending[span.body_start..span.body_end].to_string();
            self.pending.clear();

            let events = vec![self.start_event()];

            return self.capture_cell_body_chunk(&body_suffix, prose_before, events);
        }

        let safe_len = self.pending.len()
            - possible_start_tag_suffix_len(&self.pending, self.dialect.cell_tags());
        if safe_len == 0 {
            return AssistantStreamTransform {
                chunk: String::new(),
                reasoning_deltas: Vec::new(),
                events: Vec::new(),
                abort_stream: false,
            };
        }

        let flushed = self.pending[..safe_len].to_string();
        self.pending = self.pending[safe_len..].to_string();
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
        self.cell_body.push_str(chunk);
        // `allow_eof` stays false: a chunk boundary is not response EOF, which
        // is what makes the mask — not the provider's stop — own the boundary.
        let abort_stream = if let Some(span) =
            complete_end_tag_span(&self.cell_body, false, self.dialect.cell_tags())
        {
            self.cell_body = self.cell_body[..span.body_end].to_string();
            self.cell_closed = true;
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

    fn finish_response(&mut self) -> Vec<PluginRuntimeEvent> {
        if self.cell_closed || !self.inside_cell {
            return Vec::new();
        }
        // Genuine response EOF, so a closing tag at the buffer end counts.
        let Some(span) = complete_end_tag_span(&self.cell_body, true, self.dialect.cell_tags())
        else {
            return Vec::new();
        };
        self.cell_body.truncate(span.body_end);
        self.cell_closed = true;
        vec![self.end_event()]
    }

    fn start_event(&mut self) -> PluginRuntimeEvent {
        debug_assert!(!self.emitted_start);
        self.emitted_start = true;
        PluginRuntimeEvent::Custom {
            name: self.dialect.stream_cell_start_event_name().to_string(),
            payload: serde_json::json!({}),
        }
    }

    fn end_event(&mut self) -> PluginRuntimeEvent {
        debug_assert!(!self.emitted_end);
        self.emitted_end = true;
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

    fn first_lashlang_cell_span(text: &str) -> Option<crate::cell_scan::CellSpan> {
        first_cell_span(
            text,
            crate::dialect::CellTags {
                open: "<lashlang>",
                close: "</lashlang>",
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
        assert!(d.pending.is_empty());
    }

    #[test]
    fn possible_start_tag_suffix_is_held() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Plan.\n<lash");
        assert_eq!(t.chunk, "Plan.\n");
        assert_eq!(d.pending, "<lash");

        let t = d.process_chunk("lang>\n");
        assert_eq!(t.chunk, "");
        assert!(d.inside_cell);
        assert!(!d.cell_closed);
        assert_eq!(t.events.len(), 1);
        assert!(!t.abort_stream);
    }

    #[test]
    fn indented_start_tag_split_after_whitespace_is_held() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Plan.\n  ");
        assert_eq!(t.chunk, "Plan.\n");
        assert_eq!(d.pending, "  ");

        let t = d.process_chunk("<lashlang>\nfinish 1");
        assert_eq!(t.chunk, "");
        assert!(d.inside_cell);
        assert!(!d.cell_closed);
        assert_eq!(d.cell_body, "finish 1");
    }

    #[test]
    fn start_tag_and_body_in_same_chunk_preserves_body_and_does_not_abort_before_close() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Thinking...\n\n<lashlang>\ncode\n```markdown\ninside\n```\n");
        assert_eq!(t.chunk, "Thinking...\n\n");
        assert!(d.inside_cell);
        assert_eq!(d.cell_body, "code\n```markdown\ninside\n```\n");
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
            detector
                .cell_closed
                .then(|| detector.spliced_response_text())
        }

        for raw in [
            "<lashlang>\nprint 1\n</lashlang>suffix",
            "<lashlang>\nprint 1\n</lashlang>",
            "<lashlang>\nprint 1\n</lashlang>\nsuffix",
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

        let malformed = "<lashlang>\nprint 1\n</lashlang>suffix";
        let after_tag = malformed.find("suffix").expect("suffix boundary");
        assert_eq!(
            accepted(&[&malformed[..after_tag], &malformed[after_tag..]]),
            None
        );
    }

    #[test]
    fn body_after_start_tag_is_suppressed_until_close() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<lashlang>\n").chunk, "");
        let t = d.process_chunk("finish \"hi\"\n");
        assert_eq!(t.chunk, "");
        assert!(!t.abort_stream);
        assert_eq!(d.cell_body, "finish \"hi\"\n");
    }

    #[test]
    fn inline_start_tag_text_does_not_trigger() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Use <lashlang> here.\n");
        assert_eq!(t.chunk, "Use <lashlang> here.\n");
        assert!(!d.inside_cell);
        assert!(t.events.is_empty());
    }

    #[test]
    fn incomplete_start_tag_can_become_visible_prose() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<lashlang>").chunk, "");
        let t = d.process_chunk(" here\n");
        assert_eq!(t.chunk, "<lashlang> here\n");
        assert!(!d.inside_cell);
    }

    #[test]
    fn reset_prevents_cross_response_leak() {
        let mut d = CellDetector::new();
        d.process_chunk("Hi! How can I help you?");
        d.reset();

        let t = d.process_chunk("New response.\n\n<lashlang>\ncode\n");
        assert_eq!(t.chunk, "New response.\n\n");
        assert!(!t.chunk.contains("How can I help"));
    }

    #[test]
    fn reset_after_partial_cell_isolates_next_response() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<lashlang>\nfinish 1");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(d.inside_cell);
        assert!(!d.cell_closed);
        assert_eq!(d.cell_body, "finish 1");

        d.reset();

        let t = d.process_chunk("Next response.");
        assert_eq!(t.chunk, "Next response.");
        assert!(!d.inside_cell);
        assert!(!d.cell_closed);
        assert!(d.cell_body.is_empty());
    }

    #[test]
    fn reset_after_closed_cell_isolates_next_response() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<lashlang>\nfinish 1\n</lashlang>\n");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(t.abort_stream);
        assert!(d.cell_closed);

        d.reset();

        let t = d.process_chunk("Next response.");
        assert_eq!(t.chunk, "Next response.");
        assert!(!t.abort_stream);
        assert!(!d.inside_cell);
        assert!(!d.cell_closed);
    }

    #[test]
    fn close_tag_split_across_chunks_aborts_stream() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("<lashlang>\nfinish 1\n</lash").chunk, "");

        let t = d.process_chunk("lang>\n");
        assert_eq!(t.chunk, "");
        assert!(t.abort_stream);
        assert!(d.cell_closed);
        assert_eq!(d.cell_body, "finish 1");
        assert_eq!(event_names(&t.events), vec!["rlm_lashlang_cell_end"]);
    }

    #[test]
    fn close_tag_plus_trailing_prose_in_same_chunk_aborts_and_drops_suffix() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<lashlang>\nfinish 1\n</lashlang>\nTrailing prose.");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(t.abort_stream);
        assert!(d.cell_closed);
        assert_eq!(d.cell_body, "finish 1");
        assert_eq!(
            event_names(&t.events),
            vec!["rlm_lashlang_cell_start", "rlm_lashlang_cell_end"]
        );
        assert_eq!(
            d.spliced_response_text(),
            "Visible.\n<lashlang>\nfinish 1\n</lashlang>"
        );
    }

    #[test]
    fn client_abort_preserves_preceding_signed_reasoning_part() {
        let mut detector = CellDetector::new();
        let transformed =
            detector.process_chunk("Visible.\n<lashlang>\nprint \"hi\"\n</lashlang>\nignored");
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
                full_text: "provider partial".to_string(),
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
            response.full_text,
            "Visible.\n<lashlang>\nprint \"hi\"\n</lashlang>"
        );
    }

    #[test]
    fn incomplete_block_does_not_abort_and_does_not_close() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("Visible.\n<lashlang>\nfinish 1");
        assert_eq!(t.chunk, "Visible.\n");
        assert!(!t.abort_stream);
        assert!(d.inside_cell);
        assert!(!d.cell_closed);
        assert_eq!(d.cell_body, "finish 1");
    }

    fn stream_chunks(chunks: &[&str]) -> (CellDetector, String) {
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
        d.finish_response();
        (d, visible)
    }

    fn response_with_text(text: &str) -> lash_core::LlmResponse {
        lash_core::LlmResponse {
            full_text: text.to_string(),
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
            "Quick check.\n\n<lashlang>\n",
            "print \"hi\"\n",
            "finish 1\n</lashlang>",
        ]);
        assert_eq!(visible, "Quick check.\n\n");
        let spliced = d.spliced_response_text();
        let span = first_lashlang_cell_span(&spliced).expect("spliced cell parses");
        let code = &spliced[span.body_start..span.body_end];
        assert_eq!(code, "print \"hi\"\nfinish 1");
    }

    #[test]
    fn final_response_splice_ignores_raw_provider_full_text_with_suffix() {
        let raw_final = "Visible before code.\n<lashlang>\nfinish \"ok\"\n</lashlang>\nignored";
        let (d, visible) = stream_chunks(&[
            "Visible before",
            " code.\n<lash",
            "lang>\nfinish ",
            "\"ok\"\n</lashlang>\nignored",
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
            "Visible before code.\n<lashlang>\nfinish \"ok\"\n</lashlang>"
        );
        let span = first_lashlang_cell_span(&spliced).expect("spliced cell parses");
        assert_eq!(&spliced[span.body_start..span.body_end], "finish \"ok\"");
        assert!(!spliced.contains("ignored"));
    }

    #[test]
    fn final_response_transform_never_splices_using_raw_provider_text() {
        let raw_final = "Visible before code.\n<lashlang>\nfinish \"ok\"\n</lashlang>\nignored";
        let (d, visible) = stream_chunks(&[
            "Visible before",
            " code.\n%%",
            " ordinary prose\n<lashlang>\nfinish ",
            "\"ok\"\n</lashlang>\nignored",
        ]);
        assert_eq!(visible, "Visible before code.\n%% ordinary prose\n");

        let response = transform_final_response(&d, response_with_text(raw_final));
        assert_eq!(
            response.full_text,
            "Visible before code.\n%% ordinary prose\n<lashlang>\nfinish \"ok\"\n</lashlang>"
        );
        assert_eq!(response.full_text.matches("<lashlang>").count(), 1);
        assert_eq!(response.full_text.matches("</lashlang>").count(), 1);
        let span = first_lashlang_cell_span(&response.full_text).expect("cell parses");
        assert_eq!(
            &response.full_text[span.body_start..span.body_end],
            "finish \"ok\""
        );
        assert!(
            !response.full_text[span.end_tag_end..].contains("ignored"),
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
        assert_eq!(text_parts, vec![response.full_text.as_str()]);
    }

    #[test]
    fn final_response_transform_replaces_raw_text_parts_but_preserves_reasoning_parts() {
        let raw_final = "Plan.\n<lashlang>\nfinish \"ok\"\n</lashlang>\nignored";
        let (d, visible) = stream_chunks(&["Plan.\n<lash", "lang>\nfinish \"ok\"\n</lashlang>"]);
        assert_eq!(visible, "Plan.\n");
        let response = lash_core::LlmResponse {
            full_text: raw_final.to_string(),
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
            response.full_text,
            "Plan.\n<lashlang>\nfinish \"ok\"\n</lashlang>"
        );
        assert_eq!(response.full_text.matches("<lashlang>").count(), 1);
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
        assert_eq!(text_parts, vec![response.full_text.as_str()]);
    }

    #[test]
    fn final_response_transform_is_noop_without_detected_cell() {
        let mut d = CellDetector::new();
        assert_eq!(d.process_chunk("Visible only").chunk, "Visible only");

        let response = response_with_text("Visible only");
        let transformed = transform_final_response(&d, response.clone());
        assert_eq!(transformed.full_text, response.full_text);
        assert_eq!(transformed.parts, response.parts);
    }

    #[test]
    fn final_response_splice_also_handles_already_transformed_visible_text() {
        let (d, visible) = stream_chunks(&["Visible.\n", "<lashlang>\nfinish \"ok\"\n</lashlang>"]);
        assert_eq!(visible, "Visible.\n");

        let spliced = d.spliced_response_text();
        assert_eq!(spliced, "Visible.\n<lashlang>\nfinish \"ok\"\n</lashlang>");
        let span = first_lashlang_cell_span(&spliced).expect("spliced cell parses");
        assert_eq!(&spliced[span.body_start..span.body_end], "finish \"ok\"");
    }

    #[test]
    fn final_response_splice_preserves_start_tag_line_split_across_chunks() {
        let (d, visible) = stream_chunks(&[
            "Line one.",
            "\n  ",
            "<las",
            "hlang>  \n",
            "payload = r\"\"\"```markdown\nbody\n```\"\"\"\n",
            "finish payload\n  </lash",
            "lang>  ",
        ]);
        assert_eq!(visible, "Line one.\n");

        let spliced = d.spliced_response_text();
        assert_eq!(
            spliced,
            "Line one.\n<lashlang>\npayload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload\n</lashlang>"
        );
        let span = first_lashlang_cell_span(&spliced).expect("spliced cell parses");
        assert_eq!(
            &spliced[span.body_start..span.body_end],
            "payload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload"
        );
    }

    #[test]
    fn start_tag_only_without_newline_is_left_to_final_parser() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("<lashlang>");
        assert_eq!(t.chunk, "");
        assert!(!d.inside_cell);
        assert_eq!(d.splice_or_visible_for_test(""), "<lashlang>");
    }

    #[test]
    fn final_response_transform_is_noop_for_incomplete_streamed_block() {
        let mut d = CellDetector::new();
        assert_eq!(
            d.process_chunk("Visible.\n<lashlang>\nfinish 1").chunk,
            "Visible.\n"
        );
        assert!(d.inside_cell);
        assert!(!d.cell_closed);

        let response = response_with_text("Visible.\n<lashlang>\nfinish 1");
        let transformed = transform_final_response(&d, response.clone());
        assert_eq!(transformed.full_text, response.full_text);
        assert_eq!(transformed.parts, response.parts);
    }

    #[test]
    fn old_percent_marker_streams_as_plain_prose() {
        let mut d = CellDetector::new();
        let t = d.process_chunk("%%lashlang\nfinish 1\n");
        assert_eq!(t.chunk, "%%lashlang\nfinish 1\n");
        assert!(!d.inside_cell);
        assert!(!t.abort_stream);
    }

    impl CellDetector {
        fn splice_or_visible_for_test(&self, visible: &str) -> String {
            if self.inside_cell {
                self.splice_into_visible(visible)
            } else {
                let mut out = visible.to_string();
                out.push_str(&self.pending);
                out
            }
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
