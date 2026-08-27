//! Compiled sources for the Rust snippets on `docs/tracing.html`.

use std::sync::Arc;

use lash::provider::ProviderHandle;
use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};
use lash::{LashCore, ModelSpec};

async fn jsonl_trace_core(provider: ProviderHandle, model: String) -> anyhow::Result<()> {
    // docs:start:jsonl-trace-core
    use std::sync::Arc;

    use lash::{
        LashCore,
        tracing::{JsonlTraceSink, TraceLevel, TraceSink},
    };

    let trace_sink: Arc<dyn TraceSink> = Arc::new(JsonlTraceSink::new("./.lash-data/trace.jsonl"));

    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .trace_sink(trace_sink)
        .trace_level(TraceLevel::Extended)
        .build(crate::example_process_owner())?;
    // docs:end:jsonl-trace-core
    Ok(())
}

async fn lashlang_execution_jsonl(
    provider: ProviderHandle,
    model: ModelSpec,
) -> anyhow::Result<()> {
    // docs:start:lashlang-execution-jsonl
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    )
    .with_lashlang_execution_jsonl_path("./.lash-data/lashlang-execution.jsonl");
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .without_queued_work()
        .provider(provider)
        .model(model)
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::InMemoryAttachmentStore::new(),
        ))
        .process_env_store(std::sync::Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;
    // docs:end:lashlang-execution-jsonl
    Ok(())
}

async fn lashlang_graph_store(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:lashlang-graph-store
    use std::sync::Arc;

    use lash::tracing::{JsonlTraceSink, TeeTraceSink, TraceLashlangGraphStore, TraceSink};

    let lashlang_graphs = Arc::new(TraceLashlangGraphStore::default());
    let lashlang_execution_sink = Arc::new(TeeTraceSink::new([
        Arc::clone(&lashlang_graphs) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new("./.lash-data/lashlang-execution.jsonl"))
            as Arc<dyn TraceSink>,
    ]));

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    )
    .with_lashlang_execution_sink(lashlang_execution_sink);
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .without_queued_work()
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    let graph = lashlang_graphs.graph("process:process-id");
    // docs:end:lashlang-graph-store
    Ok(())
}

// docs:start:fanout-trace-sink
struct FanoutTraceSink {
    sinks: Vec<Arc<dyn TraceSink>>,
}

impl TraceSink for FanoutTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        for sink in &self.sinks {
            // Treat errors per-sink; one failing destination shouldn't take the others down.
            let _ = sink.append(record);
        }
        Ok(())
    }
}
// docs:end:fanout-trace-sink

async fn otel_trace_core(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:otel-trace-core
    use std::sync::Arc;

    use lash::{
        LashCore,
        tracing::{OtelTraceSink, TraceLevel, TraceSink},
    };

    // Exporter/provider setup stays with the host; this reads the
    // process-global OpenTelemetry tracer provider.
    let sink: Arc<dyn TraceSink> = Arc::new(OtelTraceSink::from_global_provider());
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .trace_sink(sink)
        .trace_level(TraceLevel::Extended)
        .build(crate::example_process_owner())?;
    // docs:end:otel-trace-core
    Ok(())
}

#[cfg(test)]
mod asserted_examples {
    use lash::tracing::{
        TraceContentBlock, TraceEvent, TraceLlmMessage, TraceLlmRequest, TraceLlmResponse,
        TracePromptComponent, TraceProviderRequestEvent, TraceProviderStreamEvent,
        TraceRuntimeStreamEvent, TraceTokenUsage, TraceToolSpec,
    };

    #[test]
    fn trace_events_project_tool_and_provider_activity_into_structured_audit_records() {
        let component = TracePromptComponent {
            id: "plugin:policy".to_string(),
            kind: "guidance".to_string(),
            hash: "sha256:prompt-policy".to_string(),
            chars: Some(42),
        };
        assert_eq!(component.id, "plugin:policy");
        assert_eq!(component.kind, "guidance");
        assert_eq!(component.hash, "sha256:prompt-policy");
        assert_eq!(component.chars, Some(42));

        let tool = TraceToolSpec {
            name: "search_docs".to_string(),
            description: "Search the host knowledge base.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"]
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "required": ["matches"]
            }),
        };
        assert_eq!(tool.name, "search_docs");
        assert_eq!(tool.description, "Search the host knowledge base.");
        assert_eq!(tool.input_schema["required"][0], "query");
        assert_eq!(tool.output_schema["required"][0], "matches");

        let request = TraceLlmRequest {
            model: "reasoning-model".to_string(),
            model_variant: Some("2026-08".to_string()),
            messages: vec![TraceLlmMessage {
                role: "assistant".to_string(),
                blocks: vec![
                    TraceContentBlock::ToolCall {
                        call_id: Some("call-7".to_string()),
                        tool_name: "search_docs".to_string(),
                        input_json: serde_json::json!({ "query": "release notes" }),
                        item_id: Some("item-3".to_string()),
                        has_signature: true,
                    },
                    TraceContentBlock::ToolResult {
                        call_id: Some("call-7".to_string()),
                        tool_name: Some("search_docs".to_string()),
                        content: r#"{"matches":["v1.2"]}"#.to_string(),
                    },
                ],
            }],
            attachments: Vec::new(),
            tools: vec![tool],
            tool_choice: "required".to_string(),
            output_spec: None,
            stream: true,
        };
        assert_eq!(request.tool_choice, "required");
        assert_eq!(request.tools[0].name, "search_docs");
        let request_wire = serde_json::to_value(&request).expect("trace request must serialize");
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["kind"],
            "tool_call"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["call_id"],
            "call-7"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["tool_name"],
            "search_docs"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["input_json"]["query"],
            "release notes"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["item_id"],
            "item-3"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][0]["has_signature"],
            true
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][1]["kind"],
            "tool_result"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][1]["call_id"],
            "call-7"
        );
        assert_eq!(
            request_wire["messages"][0]["blocks"][1]["tool_name"],
            "search_docs"
        );
        assert!(
            request_wire["messages"][0]["blocks"][1]["content"]
                .as_str()
                .unwrap()
                .contains("v1.2")
        );

        let provider_request = TraceProviderRequestEvent {
            provider: "provider:test".to_string(),
            sequence: 4,
            elapsed_ms: 35,
            endpoint: "/v1/responses".to_string(),
            body_len: 128,
            body_sha256: "sha256:request".to_string(),
            body_json: Some(serde_json::json!({ "model": "reasoning-model" })),
            body_json_omitted_reason: None,
        };
        assert_eq!(provider_request.provider, "provider:test");
        assert_eq!(provider_request.sequence, 4);
        assert_eq!(provider_request.elapsed_ms, 35);
        assert_eq!(provider_request.endpoint, "/v1/responses");
        assert_eq!(provider_request.body_len, 128);
        assert_eq!(provider_request.body_sha256, "sha256:request");
        assert_eq!(
            provider_request.body_json.as_ref().unwrap()["model"],
            "reasoning-model"
        );
        assert!(provider_request.body_json_omitted_reason.is_none());

        let provider_stream = TraceProviderStreamEvent {
            provider: "provider:test".to_string(),
            sequence: 5,
            elapsed_ms: 52,
            event_name: "response.output_item.added".to_string(),
            item_id: Some("item-3".to_string()),
            output_index: Some(0),
            raw_len: 96,
            raw_sha256: "sha256:event".to_string(),
            raw_json: Some(serde_json::json!({ "type": "tool_call" })),
        };
        assert_eq!(provider_stream.provider, "provider:test");
        assert_eq!(provider_stream.sequence, 5);
        assert_eq!(provider_stream.elapsed_ms, 52);
        assert_eq!(provider_stream.event_name, "response.output_item.added");
        assert_eq!(provider_stream.item_id.as_deref(), Some("item-3"));
        assert_eq!(provider_stream.output_index, Some(0));
        assert_eq!(provider_stream.raw_len, 96);
        assert_eq!(provider_stream.raw_sha256, "sha256:event");
        assert_eq!(
            provider_stream.raw_json.as_ref().unwrap()["type"],
            "tool_call"
        );

        let runtime_stream = TraceRuntimeStreamEvent {
            sequence: 6,
            elapsed_ms: 55,
            event_name: "tool_call".to_string(),
            raw_text: None,
            visible_text: None,
            item_id: Some("item-3".to_string()),
            output_index: Some(0),
            call_id: Some("call-7".to_string()),
            tool_name: Some("search_docs".to_string()),
            input_json: Some(serde_json::json!({ "query": "release notes" })),
            usage: None,
        };
        assert_eq!(runtime_stream.tool_name.as_deref(), Some("search_docs"));

        let completed_event: TraceEvent = serde_json::from_value(serde_json::json!({
            "type": "tool_call_completed",
            "call_id": "call-7",
            "name": "search_docs",
            "args": { "query": "release notes" },
            "output": {
                "outcome": { "status": "success", "payload": { "matches": ["v1.2"] } }
            },
            "duration_ms": 18
        }))
        .expect("completed tool events must decode");
        let TraceEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            duration_ms,
            ..
        } = &completed_event
        else {
            panic!("the wire event must retain its tool-completion shape");
        };
        assert_eq!(call_id.as_deref(), Some("call-7"));
        assert_eq!(name, "search_docs");
        assert_eq!(args["query"], "release notes");
        assert_eq!(output.value_for_projection()["matches"][0], "v1.2");
        assert_eq!(*duration_ms, 18);

        let events = vec![
            TraceEvent::PromptBuilt {
                prompt_hash: "sha256:prompt".to_string(),
                prompt_chars: 2_048,
                components: vec![component],
            },
            TraceEvent::ProviderRequest {
                event: provider_request,
            },
            TraceEvent::ProviderStreamEvent {
                event: provider_stream,
            },
            TraceEvent::RuntimeStreamEvent {
                event: runtime_stream,
            },
            TraceEvent::ToolCallStarted {
                call_id: Some("call-7".to_string()),
                name: "search_docs".to_string(),
                args: serde_json::json!({ "query": "release notes" }),
            },
            completed_event,
            TraceEvent::ProtocolStep {
                plugin_id: "standard".to_string(),
                payload: serde_json::json!({ "state": "tool_result" }),
            },
            TraceEvent::LlmCallCompleted {
                response: TraceLlmResponse {
                    text: "Found one match.".to_string(),
                    duration_ms: 72,
                    terminal_reason: Some("stop".to_string()),
                    parts: None,
                    generation_disposition: None,
                },
                usage: Some(TraceTokenUsage::default()),
                provider_usage: Some(serde_json::json!({ "cached_tokens": 64 })),
                stream_summary: None,
                attempts: None,
            },
            TraceEvent::RollingHistoryCompactionNeeded {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                threshold_tokens: 20_000,
            },
            TraceEvent::RollingHistoryPromptPruned {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                dropped_prefix_messages: 2,
                retained_messages: 1,
            },
            TraceEvent::RollingHistoryCompactionStarted {
                source_messages: 3,
                instructions_present: true,
            },
            TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 1 },
            TraceEvent::CompositionChanged {
                fingerprint: "sha256:composition".to_string(),
                rendered_system_prompt: "Use only approved tools.".to_string(),
                tool_schemas: request.tools.clone(),
            },
        ];
        let event_wire = serde_json::to_value(&events).expect("trace events must serialize");
        assert_eq!(event_wire[0]["type"], "prompt_built");
        assert_eq!(event_wire[0]["prompt_hash"], "sha256:prompt");
        assert_eq!(event_wire[0]["prompt_chars"], 2_048);
        assert_eq!(event_wire[0]["components"][0]["id"], "plugin:policy");
        assert_eq!(event_wire[1]["type"], "provider_request");
        assert_eq!(
            event_wire[1]["event"]["body_json"]["model"],
            "reasoning-model"
        );
        assert_eq!(event_wire[2]["type"], "provider_stream_event");
        assert_eq!(event_wire[2]["event"]["raw_json"]["type"], "tool_call");
        assert_eq!(event_wire[4]["type"], "tool_call_started");
        assert_eq!(event_wire[4]["call_id"], "call-7");
        assert_eq!(event_wire[4]["name"], "search_docs");
        assert_eq!(event_wire[4]["args"]["query"], "release notes");
        assert_eq!(event_wire[5]["type"], "tool_call_completed");
        assert_eq!(event_wire[5]["call_id"], "call-7");
        assert_eq!(event_wire[5]["name"], "search_docs");
        assert_eq!(event_wire[5]["args"]["query"], "release notes");
        assert_eq!(
            event_wire[5]["output"]["outcome"]["payload"]["matches"][0],
            "v1.2"
        );
        assert_eq!(event_wire[5]["duration_ms"], 18);
        assert_eq!(event_wire[6]["plugin_id"], "standard");
        assert_eq!(event_wire[7]["provider_usage"]["cached_tokens"], 64);
        assert_eq!(event_wire[8]["type"], "rolling_history_compaction_needed");
        assert_eq!(event_wire[8]["context_budget_tokens"], 30_000);
        assert_eq!(event_wire[8]["max_context_tokens"], 40_000);
        assert_eq!(event_wire[8]["threshold_tokens"], 20_000);
        assert_eq!(event_wire[9]["type"], "rolling_history_prompt_pruned");
        assert_eq!(event_wire[9]["context_budget_tokens"], 30_000);
        assert_eq!(event_wire[9]["max_context_tokens"], 40_000);
        assert_eq!(event_wire[9]["dropped_prefix_messages"], 2);
        assert_eq!(event_wire[9]["retained_messages"], 1);
        assert_eq!(event_wire[10]["type"], "rolling_history_compaction_started");
        assert_eq!(event_wire[10]["source_messages"], 3);
        assert_eq!(event_wire[10]["instructions_present"], true);
        assert!(event_wire[11]["type"] == "rolling_history_compaction_completed");
        assert_eq!(event_wire[11]["summary_nodes"], 1);
        assert_eq!(event_wire[12]["type"], "composition_changed");
        assert_eq!(event_wire[12]["fingerprint"], "sha256:composition");
        assert_eq!(
            event_wire[12]["rendered_system_prompt"],
            "Use only approved tools."
        );
        assert_eq!(event_wire[12]["tool_schemas"][0]["name"], "search_docs");
        assert_eq!(
            event_wire[12]["tool_schemas"][0]["input_schema"]["required"][0],
            "query"
        );
        assert_eq!(
            event_wire[12]["tool_schemas"][0]["output_schema"]["required"][0],
            "matches"
        );
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[tokio::test]
    async fn documented_tracing_builders_resolve() {
        jsonl_trace_core(
            crate::test_support::provider(),
            "docs-snippet-test".to_string(),
        )
        .await
        .expect("JSONL tracing snippet must build");
        lashlang_execution_jsonl(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("Lashlang JSONL snippet must build");
        lashlang_graph_store(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("Lashlang graph-store snippet must build");
        otel_trace_core(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("OpenTelemetry tracing snippet must build");
    }
}

#[cfg(test)]
mod replay_provenance_examples {
    use lash::tracing::{
        TraceEvent, TraceProviderReplayDropEvent, TraceProviderReplayDropReason,
        TraceProviderReplayKind, TraceProviderRouteIdentity,
    };

    #[test]
    fn provider_replay_drop_trace_preserves_both_routes() {
        let replay_drop = TraceProviderReplayDropEvent {
            replay_kind: TraceProviderReplayKind::Reasoning,
            reason: TraceProviderReplayDropReason::ForeignRoute,
            minting_route: Some(TraceProviderRouteIdentity {
                provider: "anthropic".to_string(),
                endpoint: "https://api.anthropic.com".to_string(),
                model: "claude-sonnet-4-6".to_string(),
            }),
            serving_route: TraceProviderRouteIdentity {
                provider: "google_oauth".to_string(),
                endpoint: "https://cloudcode-pa.googleapis.com/v1internal".to_string(),
                model: "gemini-2.5-pro".to_string(),
            },
        };

        let response_text_code = TraceProviderReplayKind::ResponseText.code();
        let reasoning_code = replay_drop.replay_kind.code();
        let tool_call_code = TraceProviderReplayKind::ToolCall.code();
        assert_eq!(response_text_code, "response_text");
        assert_eq!(reasoning_code, "reasoning");
        assert_eq!(tool_call_code, "tool_call");
        let unstamped_code = TraceProviderReplayDropReason::Unstamped.code();
        let foreign_route_code = replay_drop.reason.code();
        assert_eq!(unstamped_code, "unstamped");
        assert_eq!(foreign_route_code, "foreign_route");
        let minting_route = replay_drop.minting_route.as_ref().expect("minting route");
        assert_eq!(minting_route.provider, "anthropic");
        assert_eq!(minting_route.model, "claude-sonnet-4-6");
        assert_eq!(replay_drop.serving_route.provider, "google_oauth");
        assert_eq!(replay_drop.serving_route.model, "gemini-2.5-pro");

        let event = TraceEvent::ProviderReplayDropped { event: replay_drop };
        let wire = serde_json::to_value(&event).expect("replay drop trace serializes");
        assert_eq!(wire["type"], "provider_replay_dropped");
        assert_eq!(wire["event"]["reason"], "foreign_route");
        assert_eq!(
            wire["event"]["serving_route"]["endpoint"],
            "https://cloudcode-pa.googleapis.com/v1internal"
        );
    }
}
