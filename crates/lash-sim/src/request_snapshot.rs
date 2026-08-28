use std::sync::Arc;

use lash::PromptLayerSink as _;
use lash_core::{
    PromptTemplate, PromptTemplateEntry, PromptTemplateSection, facade_support::TraceLevel,
};
use serde_json::{Value, json};

use crate::provider::ScriptedLlmHttpTransport;
use crate::runtime_providers::{
    OPENAI_COMPATIBLE, runtime_provider_components, runtime_scripts_for_texts,
};

#[tokio::test]
async fn second_history_bearing_turn_snapshots_the_full_assembled_provider_request() {
    let scripts = runtime_scripts_for_texts(
        OPENAI_COMPATIBLE,
        &["first reply".to_string(), "second reply".to_string()],
    )
    .expect("runtime scripts");
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts(scripts).expect("valid runtime provider scripts"),
    );
    let (provider, model, _) =
        runtime_provider_components(OPENAI_COMPATIBLE, &transport).expect("runtime provider");
    let trace_dir = tempfile::tempdir().expect("trace directory");
    let trace_path = trace_dir.path().join("provider-requests.jsonl");
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .with_native_queued_work()
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
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

    let first = session
        .turn(lash::TurnInput::text("first question"))
        .run()
        .await
        .expect("first turn");
    assert_eq!(first.assistant_message(), Some("first reply"));
    let second = session
        .turn(lash::TurnInput::text("follow-up question"))
        .run()
        .await
        .expect("second turn");
    assert_eq!(second.assistant_message(), Some("second reply"));
    core.flush_trace_sink()
        .expect("flush provider request trace");

    let entries = std::fs::read_to_string(&trace_path)
        .expect("trace file")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace entry"))
        .collect::<Vec<_>>();
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
                    "description": "Execute up to 25 independent tool calls concurrently. Calls start in parallel; ordering is not guaranteed. Calls past index 25 are rejected.",
                    "name": "batch",
                    "parameters": {
                        "additionalProperties": false,
                        "properties": {
                            "tool_calls": {
                                "description": "Array of 1-25 objects like { tool: \"<tool_name>\", parameters: { <arg>: \"<value>\" } }. Use only for independent calls. Do not include another batch call. More than 25 calls is rejected as a tool error.",
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
    assert_eq!(requests[1]["event"]["body_len"], 1266);
    assert_eq!(
        requests[1]["event"]["body_sha256"],
        "154d52c5cba6a3a7f2981503e22adf0682604326bc9918f46e21bacd12711ea3"
    );
}
