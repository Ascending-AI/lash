//! The host's one telemetry content policy governs what a turn's records
//! carry on every built-in telemetry path (FIG-5530): a host's `send()` on
//! the core's node over SQLite memory stores.

use super::*;

use crate::support::TurnInput;
use lash_core::llm::types::{LlmOutputPart, LlmTerminalReason};
use lash_core::testing::runtime_helpers::{EchoTool, MockCall, mock_provider};
use lash_trace::{TelemetryContent, TraceRecord, TraceSink, TraceSinkError};
use std::sync::Mutex as StdMutex;

const PROMPT: &str = "PROMPT-MARKER";
const ARGUMENT: &str = "ARGUMENT-MARKER";
const RESPONSE: &str = "RESPONSE-MARKER";
/// `EchoTool` returns its argument under this prefix.
const OUTPUT: &str = "raw:ARGUMENT-MARKER";
const PROVIDER_CALL: &str = "call-content-law";

/// The line a serializing sink writes for each record it is handed: what
/// `StderrTraceSink` puts on stderr.
#[derive(Default)]
struct Lines(StdMutex<Vec<String>>);

impl TraceSink for Lines {
    fn append(&self, record: &TraceRecord) -> std::result::Result<(), TraceSinkError> {
        self.0
            .lock()
            .expect("lines")
            .push(serde_json::to_string(record)?);
        Ok(())
    }
}

/// The records the telemetry adapter is handed to project.
#[derive(Default)]
struct Projected(StdMutex<Vec<TraceRecord>>);

impl lash_trace::TraceDomainProjector for Projected {
    fn project(
        &self,
        _scope: &lash_trace::DurableTraceScope,
        _attempt: Option<&lash_trace::AttemptObservation>,
        _source: &lash_trace::EmissionSource,
        record: &TraceRecord,
    ) {
        self.0.lock().expect("projected").push(record.clone());
    }
}

/// What one tool-calling turn wrote to each telemetry path.
struct Telemetry {
    jsonl: String,
    lines: Vec<String>,
    projected: Vec<TraceRecord>,
    lash_call_id: String,
}

impl Telemetry {
    /// Every path's records as text, named for a failure message.
    fn paths(&self) -> [(&'static str, String); 3] {
        [
            ("JSONL", self.jsonl.clone()),
            ("stderr", self.lines.join("\n")),
            (
                "the adapter's projection",
                self.projected
                    .iter()
                    .map(|record| serde_json::to_string(record).expect("encode record"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ]
    }

    fn records(&self) -> Vec<serde_json::Value> {
        lash_trace::parse_jsonl_records::<serde_json::Value>(&self.jsonl).expect("trace records")
    }
}

/// One turn that sends [`PROMPT`], calls `echo_tool` with [`ARGUMENT`] and
/// answers [`RESPONSE`], under `configure`'s telemetry settings.
#[allow(
    clippy::disallowed_methods,
    reason = "the law reads back the trace file its own core wrote"
)]
async fn tool_calling_turn(
    session: &str,
    configure: impl FnOnce(crate::core::LashCoreBuilder) -> crate::core::LashCoreBuilder,
) -> Result<Telemetry> {
    let dir = tempfile::tempdir().expect("trace directory");
    let path = dir.path().join("trace.jsonl");
    let lines = Arc::new(Lines::default());
    let projected = Arc::new(Projected::default());
    // Report the terminals the provider observed: an Unknown terminal seals
    // an interrupted attempt even when the response carries output.
    let provider = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: PROVIDER_CALL.to_owned(),
                    tool_name: "echo_tool".to_owned(),
                    input_json: format!(r#"{{"value":"{ARGUMENT}"}}"#),
                    replay: None,
                }],
                terminal_reason: LlmTerminalReason::ToolUse,
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                terminal_reason: LlmTerminalReason::Stop,
                ..text_response(RESPONSE)
            }),
        },
    ])
    .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let runtime = lash_core::runtime::TraceRuntime::new(backend.clock())
        .with_projector(Arc::clone(&projected) as Arc<dyn lash_trace::TraceDomainProjector>);
    let core = configure(
        explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(Arc::new(EchoTool))
            .trace_runtime(runtime)
            .trace_sink(Arc::new(lash_trace::TeeTraceSink::new([
                Arc::new(lash_trace::JsonlTraceSink::new(path.clone())) as Arc<dyn TraceSink>,
                Arc::clone(&lines) as Arc<dyn TraceSink>,
            ])))
            .trace_level(lash_trace::TraceLevel::Extended),
    )
    .build(crate::testing::runtime_lease_owner())?;
    // output() waits for the durable settled outcome before trace shutdown.
    let turn = core
        .session(crate::SessionId::parse(session).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text(PROMPT))
        .output()
        .await?;
    // The app's own response and the turn's durable tool record keep their
    // contracts whatever the telemetry policy.
    assert_eq!(turn.result.tool_calls.len(), 1);
    assert_eq!(
        turn.result.tool_calls[0].args["value"], ARGUMENT,
        "the turn result carries the call's arguments"
    );
    assert!(
        format!("{:?}", turn.result).contains(RESPONSE),
        "the turn result carries the model's response"
    );
    let lash_call_id = turn.result.tool_calls[0].call_id.to_string();

    core.flush_trace_sink().expect("flush the trace sink");
    core.shutdown().await?;
    Ok(Telemetry {
        jsonl: std::fs::read_to_string(&path).unwrap_or_default(),
        lines: lines.0.lock().expect("lines").clone(),
        projected: projected.0.lock().expect("projected").clone(),
        lash_call_id,
    })
}

fn of_type<'a>(entries: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    entries
        .iter()
        .filter(|entry| entry["type"] == kind)
        .collect()
}

/// With no telemetry content setting, a turn with a tool call writes its
/// records to JSONL, to a serializing sink and to the adapter with their
/// identities and outcomes and with no prompt, response, tool argument or
/// tool output. A host that states `TelemetryContent::Captured` gets the same
/// records with all four present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telemetry_content_policy_governs_every_built_in_telemetry_path() -> Result<()> {
    let omitted = tool_calling_turn("content-omitted", |builder| builder).await?;
    for (path, text) in omitted.paths() {
        assert!(!text.is_empty(), "{path} received the turn's records");
        for content in [PROMPT, ARGUMENT, RESPONSE, OUTPUT] {
            assert!(
                !text.contains(content),
                "{path} carries {content} with telemetry content omitted:\n{text}"
            );
        }
    }
    let entries = omitted.records();
    assert!(
        entries.iter().all(|entry| entry["content"] == "omitted"),
        "every record says its content was omitted: {entries:?}"
    );
    assert!(
        omitted
            .projected
            .iter()
            .all(|record| record.content == TelemetryContent::Omitted),
        "the adapter is handed governed records"
    );
    assert_eq!(omitted.lines.len(), entries.len(), "one line per record");

    let started = of_type(&entries, "tool_call_started");
    let completed = of_type(&entries, "tool_call_completed");
    assert_eq!((started.len(), completed.len()), (1, 1), "{entries:?}");
    for record in [started[0], completed[0]] {
        assert_eq!(record["call_id"], omitted.lash_call_id.as_str());
        assert_eq!(record["provider_call_id"], PROVIDER_CALL);
        assert_eq!(record["name"], "echo_tool");
        assert_eq!(record["args"], serde_json::Value::Null);
        assert_eq!(record["context"]["session_id"], "content-omitted");
    }
    assert_eq!(completed[0]["output"]["outcome"]["status"], "success");
    assert_eq!(
        completed[0]["output"]["outcome"]["payload"],
        serde_json::Value::Null
    );
    let llm_started = of_type(&entries, "llm_call_started");
    let llm_completed = of_type(&entries, "llm_call_completed");
    assert_eq!(
        (llm_started.len(), llm_completed.len()),
        (2, 2),
        "{entries:?}"
    );
    for record in &llm_started {
        assert!(record["request"]["model"].is_string());
        assert_eq!(record["request"]["messages"], serde_json::json!([]));
        assert_eq!(record["request"]["tools"][0]["name"], "echo_tool");
        assert_eq!(record["request"]["tools"][0]["description"], "");
    }
    for record in &llm_completed {
        assert_eq!(record["response"]["text"], "");
        assert!(record["usage"].is_object(), "usage counts stay: {record}");
        assert!(record["context"]["llm_call_id"].is_string());
    }
    for record in of_type(&entries, "composition_changed") {
        assert!(record["fingerprint"].is_string());
        assert_eq!(record["rendered_system_prompt"], "");
    }
    let requests = of_type(&entries, "provider_event");
    for record in &requests {
        assert!(record["event"]["raw_len"].is_u64(), "{record}");
        assert!(record["event"]["raw_sha256"].is_string(), "{record}");
        assert!(record["event"].get("raw_json").is_none(), "{record}");
    }
    let captured = tool_calling_turn("content-captured", |builder| {
        builder.telemetry_content(TelemetryContent::Captured)
    })
    .await?;
    for (path, text) in captured.paths() {
        for content in [PROMPT, ARGUMENT, RESPONSE, OUTPUT] {
            assert!(
                text.contains(content),
                "{path} lacks {content} with telemetry content captured:\n{text}"
            );
        }
    }
    let entries = captured.records();
    assert!(
        entries.iter().all(|entry| entry["content"] == "captured"),
        "{entries:?}"
    );
    assert_eq!(
        of_type(&entries, "tool_call_completed")[0]["args"]["value"],
        ARGUMENT
    );
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        omitted
            .records()
            .iter()
            .map(|entry| entry["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        "the policy changes what records carry, not which records exist"
    );
    for telemetry in [&omitted, &captured] {
        let entries = telemetry.records();
        let attempts = of_type(&entries, "llm_attempt_completed");
        assert_eq!(attempts.len(), 2, "{entries:?}");
        for record in attempts {
            assert_eq!(record["attempt"]["ordinal"], 1);
            assert_eq!(record["attempt"]["outcome"], "completed", "{record}");
            assert_eq!(record["observation"]["provider"], "mock");
        }
    }
    Ok(())
}
