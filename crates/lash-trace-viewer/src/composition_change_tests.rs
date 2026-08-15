use lash_trace::{TraceEvent, TraceToolSpec};

use crate::render::interpret_typed;

pub(super) fn sample() -> TraceEvent {
    TraceEvent::CompositionChanged {
        fingerprint: "composition-sha".to_string(),
        rendered_system_prompt: "system policy".to_string(),
        tool_schemas: vec![TraceToolSpec {
            name: "search".to_string(),
            description: "Search documents".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
            output_schema: serde_json::json!({ "type": "array" }),
        }],
    }
}

#[test]
fn composition_change_render_includes_fingerprint_prompt_and_ordered_schemas() {
    let event = sample();
    let raw = serde_json::to_value(&event).expect("composition event JSON");
    let (title, summary, failed) = interpret_typed(&event, &raw);

    assert!(title.contains("13 prompt chars"));
    assert!(title.contains("1 tools"));
    assert!(summary.contains("composition-sha"));
    assert!(summary.contains("system policy"));
    assert!(summary.contains("search"));
    assert!(!failed);
}
