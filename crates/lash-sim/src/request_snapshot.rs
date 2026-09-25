use std::sync::Arc;

use lash::PromptLayerSink as _;
use lash_core::{
    PromptTemplate, PromptTemplateEntry, PromptTemplateSection, facade_support::TraceLevel,
};
use serde_json::{Value, json};

use crate::provider::ScriptedLlmHttpTransport;
use crate::runtime_providers::{
    OPENAI_COMPATIBLE, runtime_provider_components, runtime_script_for_text,
};

#[tokio::test]
async fn second_history_bearing_turn_snapshots_the_full_assembled_provider_request() {
    let scripts = ["first reply", "second reply"]
        .into_iter()
        .map(|text| runtime_script_for_text(OPENAI_COMPATIBLE, text))
        .collect::<Result<Vec<_>, _>>()
        .expect("runtime scripts");
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts(scripts).expect("valid runtime provider scripts"),
    );
    let (provider, model, _) =
        runtime_provider_components(OPENAI_COMPATIBLE, &transport).expect("runtime provider");
    let trace_dir = tempfile::tempdir().expect("trace directory");
    let trace_path = trace_dir.path().join("provider-requests.jsonl");
    let engine = crate::backend::SimEngine::new(0x5eed_7005)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model)
        .prompt_template(PromptTemplate::new(vec![PromptTemplateSection::untitled(
            vec![PromptTemplateEntry::text("System snapshot instruction.")],
        )]))
        .trace_jsonl_path(&trace_path)
        .trace_level(TraceLevel::Extended)
        .build(crate::sim_process_owner())
        .expect("runtime core");
    let session = core
        .session("history-request-snapshot")
        .open()
        .await
        .expect("session");

    let first = engine
        .run_text_turn(&session, "history-snapshot-turn-1", "first question")
        .await
        .expect("first turn handler")
        .expect("first turn");
    assert_eq!(first.assistant_message(), Some("first reply"));
    let second = engine
        .run_text_turn(&session, "history-snapshot-turn-2", "follow-up question")
        .await
        .expect("second turn handler")
        .expect("second turn");
    assert_eq!(second.assistant_message(), Some("second reply"));
    core.flush_trace_sink()
        .expect("flush provider request trace");

    let entries = lash_core::facade_support::parse_jsonl_records::<Value>(
        &std::fs::read_to_string(&trace_path).expect("trace file"),
    )
    .expect("trace entries");
    let requests = entries
        .iter()
        .filter(|entry| entry["type"] == "provider_request")
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        2,
        "provider request trace entries: {entries:?}"
    );
    let assembled = requests[1]["event"]["body_json"].clone();
    assert_eq!(
        assembled,
        json!({
            "max_tokens": 32768,
            "messages": [
                {
                    "content": [{
                        "text": "System snapshot instruction.",
                        "type": "text"
                    }],
                    "role": "system"
                },
                {
                    "content": [{ "text": "first question", "type": "text" }],
                    "role": "user"
                },
                {
                    "content": [{ "text": "first reply", "type": "text" }],
                    "role": "assistant"
                },
                {
                    "content": [{ "text": "follow-up question", "type": "text" }],
                    "role": "user"
                }
            ],
            "model": "openai/gpt-5.4",
            "parallel_tool_calls": true,
            "stream": true,
            "stream_options": { "include_usage": true },
            "tool_choice": "auto",
            "tools": [{
                "function": {
                    "description": "Run 1-25 independent tool calls concurrently. Execution order is not guaranteed; results return in input order, each with a success flag and result or error. Do not nest batch calls.",
                    "name": "batch",
                    "parameters": {
                        "additionalProperties": false,
                        "properties": {
                            "tool_calls": {
                                "description": "1-25 objects { tool, parameters }; each tool must be exposed and parameters must match its schema.",
                                "items": {
                                    "additionalProperties": false,
                                    "properties": {
                                        "parameters": {
                                            "additionalProperties": true,
                                            "properties": {},
                                            "type": "object"
                                        },
                                        "tool": { "type": "string" }
                                    },
                                    "required": ["tool", "parameters"],
                                    "type": "object"
                                },
                                "maxItems": 25,
                                "minItems": 1,
                                "type": "array"
                            }
                        },
                        "required": ["tool_calls"],
                        "type": "object"
                    },
                    "strict": false
                },
                "type": "function"
            }]
        })
    );
    assert_eq!(requests[1]["event"]["body_len"], 1202);
    assert_eq!(
        requests[1]["event"]["body_sha256"],
        "38b714af125879f7f019f6b8348bb7b9c3bd0750ca6e6bf1e875e738c32895c4"
    );
}
