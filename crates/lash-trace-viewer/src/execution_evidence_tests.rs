use lash_trace::{TraceContext, TraceEvent, TraceRecord};

use crate::build_model;
use crate::tests::{every_variant, loaded_trace};

#[test]
fn llm_and_tool_retry_ladders_render_attempt_count_reason_delay_and_evidence() {
    let trace = loaded_trace(
        every_variant()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    TraceEvent::LlmCallCompleted { .. } | TraceEvent::ToolCallCompleted { .. }
                )
            })
            .map(|event| TraceRecord::new(TraceContext::default(), event))
            .collect(),
    );
    let model = build_model(&trace);
    assert_eq!(model.events.len(), 2);
    for event in model.events {
        assert!(event.summary.contains("attempts: 2"));
        assert!(event.summary.contains("#1 failed"));
        assert!(event.summary.contains("retry after"));
        assert!(event.summary.contains("#2 completed"));
        if event.kind == "llm_call_completed" {
            assert!(event.summary.contains("model=served-model"));
            assert!(event.summary.contains("reasoning_tokens=0"));
            assert_eq!(event.summary.matches("collection_interruption=").count(), 1);
            assert!(
                event
                    .summary
                    .contains("collection_interruption=protocol_abort")
            );
        }
    }
}
