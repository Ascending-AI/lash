//! A tool's direct model completion runs inside its call on the session
//! actor's turn, answers with its usage and call record, and is traced like
//! any model call.

// The trace JSONL this law reads is a file the test host owns.
#![allow(clippy::disallowed_methods)]

use super::*;

const PROMPT: &str = "raw prompt";

fn direct_probe_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:direct_probe",
        "direct_probe",
        "Ask the model directly and answer what it said.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

/// A tool whose call asks the model [`PROMPT`] directly.
struct DirectProbe;

#[async_trait]
impl ToolProvider for DirectProbe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![direct_probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "direct_probe").then(|| Arc::new(direct_probe_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(PROMPT),
                "direct-llm-test",
            )
            .await
        {
            Ok(completion) => lash_core::ToolOutcome::ok(serde_json::json!({
                "text": completion.text,
                "input_tokens": completion.usage.input_tokens,
                "output_tokens": completion.usage.output_tokens,
                "attempts": completion.llm_call.attempts.len(),
            }))
            .into(),
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tools_direct_completion_runs_in_its_call_and_records_usage_and_trace() -> Result<()> {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("direct-llm")
        .complete({
            let turn_calls = Arc::clone(&turn_calls);
            move |request| {
                let direct = last_user_text(&request).contains(PROMPT);
                let turn_call = if direct {
                    None
                } else {
                    Some(turn_calls.fetch_add(1, Ordering::SeqCst))
                };
                async move {
                    Ok(match turn_call {
                        None => LlmResponse {
                            usage: lash_core::llm::types::LlmUsage {
                                input_tokens: 4,
                                output_tokens: 6,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_output_tokens: 1,
                            },
                            ..text_response("raw direct answer")
                        },
                        Some(0) => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "direct-call".to_string(),
                                tool_name: "direct_probe".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        },
                        Some(_) => text_response("done"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let trace = tempfile::tempdir().expect("trace directory");
    let trace_path = trace.path().join("trace.jsonl");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(DirectProbe))
    .trace_jsonl_path(trace_path.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("direct-llm").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("ask the model directly"))
        .output()
        .await?;

    assert_eq!(output.assistant_message(), Some("done"));
    let answered = output
        .result
        .tool_calls
        .iter()
        .find(|call| call.tool == "direct_probe")
        .expect("the probe's call is reported")
        .output
        .value_for_projection();
    assert_eq!(answered["text"], "raw direct answer");
    assert_eq!(answered["input_tokens"], 4);
    assert_eq!(answered["output_tokens"], 6);
    assert_eq!(answered["attempts"], 1);

    core.flush_trace_sink()?;
    let records = lash_trace::parse_jsonl_records::<serde_json::Value>(
        &std::fs::read_to_string(&trace_path).expect("read the trace"),
    )
    .expect("trace records");
    assert!(
        records.iter().any(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("llm_call_completed")
                && record.pointer("/usage/output_tokens") == Some(&serde_json::json!(6))
        }),
        "the direct call is traced with its usage: {records:?}"
    );
    Ok(())
}
