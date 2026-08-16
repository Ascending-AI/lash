//! Versioned provider-variation fixture matrix at the transport seam.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash_core::provider::{
    ProviderCompletion, ProviderCompletionError, ProviderHandle, ProviderOptions,
    ProviderReliability, StreamTermination,
};
use lash_core::{AttemptOutcome, ProtocolPosition, ProviderFailureKind};
use lash_llm_transport::LlmHttpTransport;
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::codex::ws_testing::{ScriptedWsAction, spawn_scripted_websocket};
use lash_provider_openai::{CodexProvider, OpenAiCompatibleProvider, OpenAiProvider};
use lash_sansio::llm::types::{
    LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRequestScope, LlmRole,
    LlmStreamEvent, LlmStreamEvidence, LlmTerminalReason, LlmToolChoice,
};
use lash_sansio::session_model::{Message, MessageRole, render_prompt, shared_parts};
use lash_sansio::sync::MutexExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::provider::{
    ProviderWireEndpoint, ProviderWireEvent, ProviderWireHeader, ProviderWireRequestMatch,
    ProviderWireScript, ScriptedLlmHttpTransport,
};
use crate::provider_variations::LASHLANG_CLOSE_DELIMITER;

const MATRIX_SCHEMA: &str = "lash.provider-variation-matrix.v1";
const MATRIX_JSON: &str = include_str!("../provider-scripts/variation-matrix/v1/matrix.json");
const EXPECTED_DIALECTS: [&str; 6] = [
    "anthropic.messages",
    "google.generate-content",
    "openai.chat-completions",
    "openai.responses",
    "codex.responses-sse",
    "codex.responses-websocket",
];
const EXPECTED_VARIATIONS: [&str; 8] = [
    "stop_consumed",
    "literal_present",
    "equivalent_outcome_mapping",
    "missing_terminal_evidence",
    "event_shape_variation",
    "reasoning_rendered_path",
    "response_identity_execution_evidence",
    "multi_reset_retry_sequence",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    schema: String,
    dialects: Vec<String>,
    rows: Vec<MatrixRow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixRow {
    variation: String,
    expectation: MatrixExpectation,
    cells: BTreeMap<String, MatrixCell>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MatrixExpectation {
    StopBoundary { literal_present: bool },
    EquivalentOutcomeMapping,
    MissingTerminalEvidence,
    EventShapeVariation,
    ReasoningRenderedPath,
    ResponseIdentityExecutionEvidence,
    MultiResetRetrySequence,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MatrixCell {
    ProviderWireScripts {
        paths: Vec<String>,
    },
    RecordedStreams {
        recordings: Vec<Recording>,
    },
    NotApplicable {
        reason: String,
        recordings: Vec<Recording>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    case: String,
    #[serde(default = "default_http_status")]
    status: u16,
    #[serde(default)]
    request_match: ProviderWireRequestMatch,
    #[serde(default)]
    expected_finish_reason: Option<String>,
    #[serde(default)]
    headers: Vec<ProviderWireHeader>,
    #[serde(default)]
    events: Vec<RecordedPayload>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    close_after_events: bool,
    #[serde(default)]
    transport_error: Option<String>,
    #[serde(default)]
    retryable: bool,
}

const fn default_http_status() -> u16 {
    200
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedPayload {
    #[serde(default)]
    json: Option<Value>,
    #[serde(default)]
    raw: Option<String>,
}

impl RecordedPayload {
    fn wire(&self) -> String {
        match (&self.json, &self.raw) {
            (Some(value), None) => value.to_string(),
            (None, Some(raw)) => raw.clone(),
            _ => panic!("recorded payload must contain exactly one of json or raw"),
        }
    }
}

impl Matrix {
    fn load() -> Self {
        serde_json::from_str(MATRIX_JSON).expect("provider variation matrix parses")
    }
}

#[test]
fn matrix_is_a_complete_versioned_row_by_column_product() {
    let matrix = Matrix::load();
    assert_eq!(matrix.schema, MATRIX_SCHEMA);
    assert_eq!(matrix.dialects, EXPECTED_DIALECTS);
    assert_eq!(
        matrix
            .rows
            .iter()
            .map(|row| row.variation.as_str())
            .collect::<Vec<_>>(),
        EXPECTED_VARIATIONS
    );

    let expected = EXPECTED_DIALECTS.into_iter().collect::<BTreeSet<_>>();
    for row in &matrix.rows {
        assert_eq!(
            row.cells
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            expected,
            "{} must fill every dialect column",
            row.variation
        );
        for (dialect, cell) in &row.cells {
            match cell {
                MatrixCell::ProviderWireScripts { paths } => {
                    assert!(
                        !paths.is_empty(),
                        "{} {dialect} has no fixture",
                        row.variation
                    );
                    for path in paths {
                        assert!(
                            matrix_dir().join(path).is_file(),
                            "{} {dialect} references missing fixture {path}",
                            row.variation
                        );
                    }
                }
                MatrixCell::RecordedStreams { recordings } => assert!(
                    !recordings.is_empty(),
                    "{} {dialect} has no recordings",
                    row.variation
                ),
                MatrixCell::NotApplicable { reason, recordings } => {
                    assert!(
                        row.variation == "stop_consumed" && !reason.trim().is_empty(),
                        "only the provider-owned wire-stop row may be inapplicable"
                    );
                    assert_eq!(recordings.len(), 1);
                }
            }
        }
    }
}

#[test]
fn checked_in_matrix_matches_its_deterministic_generator() {
    let temp = tempfile::tempdir().expect("matrix regeneration tempdir");
    let output = std::process::Command::new("python3")
        .arg(matrix_generator())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env(
            "LASH_PROVIDER_MATRIX_OUTPUT",
            temp.path().join("matrix.json"),
        )
        .output()
        .expect("run matrix generator");
    assert!(
        output.status.success(),
        "matrix generator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated =
        std::fs::read(temp.path().join("matrix.json")).expect("read regenerated provider matrix");
    assert_eq!(generated, MATRIX_JSON.as_bytes());
}

#[tokio::test]
async fn http_adapter_columns_match_every_shared_row_expectation() {
    let matrix = Matrix::load();
    for dialect in EXPECTED_DIALECTS
        .into_iter()
        .filter(|dialect| *dialect != "codex.responses-websocket")
    {
        for row in &matrix.rows {
            let cell = row.cells.get(dialect).expect("complete matrix column");
            if let MatrixCell::NotApplicable { recordings, .. } = cell {
                run_unsupported_stop_http(dialect, row, recordings).await;
                continue;
            }
            let recordings = match materialize_recordings(dialect, cell) {
                Some(recordings) => recordings,
                None => continue,
            };
            run_http_row(dialect, row, recordings).await;
        }
    }
}

#[tokio::test]
async fn codex_websocket_column_matches_every_shared_row_expectation() {
    let matrix = Matrix::load();
    let dialect = "codex.responses-websocket";
    for row in &matrix.rows {
        let cell = row.cells.get(dialect).expect("complete WebSocket column");
        if let MatrixCell::NotApplicable { recordings, .. } = cell {
            run_unsupported_stop_websocket(row, recordings).await;
            continue;
        }
        let Some(recordings) = materialize_recordings(dialect, cell) else {
            continue;
        };
        run_websocket_row(row, recordings).await;
    }
}

async fn run_websocket_row(row: &MatrixRow, recordings: Vec<Recording>) {
    let dialect = "codex.responses-websocket";
    match row.expectation {
        MatrixExpectation::StopBoundary { literal_present } => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let (route, completion) =
                complete_websocket(row, std::slice::from_ref(recording)).await;
            let completion =
                completion.unwrap_or_else(|error| panic!("{} failed: {error:?}", row.variation));
            assert_eq!(completion.terminal_reason, LlmTerminalReason::Stop);
            assert_eq!(
                completion.full_text.contains(LASHLANG_CLOSE_DELIMITER),
                literal_present
            );
            assert_current_success_contract(dialect, &completion, recording);
            assert_replay_origins(&completion.parts, &route);
        }
        MatrixExpectation::EquivalentOutcomeMapping => {
            for (case, expected) in [
                ("stop", LlmTerminalReason::Stop),
                ("tool", LlmTerminalReason::ToolUse),
                ("length", LlmTerminalReason::OutputLimit),
                ("content_filter", LlmTerminalReason::ContentFilter),
            ] {
                let recording = named_recording(&recordings, case);
                let (_, completion) =
                    complete_websocket(row, std::slice::from_ref(recording)).await;
                let completion = completion
                    .unwrap_or_else(|error| panic!("{} {case} failed: {error:?}", row.variation));
                assert_eq!(completion.terminal_reason, expected, "{case}");
                if case == "tool" {
                    assert_lookup_tool(&completion.parts, dialect, &row.variation);
                }
                assert_current_success_contract(dialect, &completion, recording);
            }
        }
        MatrixExpectation::MissingTerminalEvidence => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let (route, completion) =
                complete_websocket(row, std::slice::from_ref(recording)).await;
            let error = completion.expect_err("WebSocket EOF before response.completed must fail");
            assert_eq!(error.error.kind, ProviderFailureKind::Stream);
            assert!(error.error.code.is_some());
            let partial = error
                .error
                .partial_response
                .as_deref()
                .expect("WebSocket missing-terminal failure carries partial state");
            assert_eq!(partial.terminal_reason, LlmTerminalReason::Unknown);
            assert_eq!(partial.full_text, "matrix partial");
            assert_eq!(error.call_record.attempts.len(), 1);
            assert_eq!(
                error.call_record.attempts[0].protocol_position,
                ProtocolPosition::OutputStarted
            );
            assert!(partial.http_summary.is_some());
            assert!(partial.execution_evidence.is_some());
            assert_replay_origins(&partial.parts, &route);
        }
        MatrixExpectation::EventShapeVariation => {
            for case in ["empty_deltas", "empty_terminal_chunks"] {
                let recording = named_recording(&recordings, case);
                let events = Arc::new(Mutex::new(Vec::new()));
                let (_, completion) =
                    complete_websocket_with_events(row, std::slice::from_ref(recording), &events)
                        .await;
                let completion = completion
                    .unwrap_or_else(|error| panic!("{} {case} failed: {error:?}", row.variation));
                assert_eq!(completion.full_text, "matrix shape");
                assert_eq!(completion.terminal_reason, LlmTerminalReason::Stop);
                assert_current_success_contract(dialect, &completion, recording);
                assert_no_empty_stream_output(dialect, case, &events.lock_recover());
            }
            let recording = named_recording(&recordings, "split_reasoning_tool");
            let (_, completion) = complete_websocket(row, std::slice::from_ref(recording)).await;
            let completion = completion.unwrap_or_else(|error| {
                panic!("{} split reasoning/tool failed: {error:?}", row.variation)
            });
            assert_eq!(completion.terminal_reason, LlmTerminalReason::ToolUse);
            assert_reasoning(
                &completion.parts,
                "matrix reasoning",
                dialect,
                &row.variation,
            );
            assert_lookup_tool(&completion.parts, dialect, &row.variation);
            assert_current_success_contract(dialect, &completion, recording);
        }
        MatrixExpectation::ReasoningRenderedPath => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let (route, completion) =
                complete_websocket(row, std::slice::from_ref(recording)).await;
            let completion =
                completion.unwrap_or_else(|error| panic!("{} failed: {error:?}", row.variation));
            assert_reasoning(
                &completion.parts,
                "matrix reasoning",
                dialect,
                &row.variation,
            );
            assert_reasoning_rendered(&completion.parts, dialect);
            assert_replay_origins(&completion.parts, &route);
            assert_current_success_contract(dialect, &completion, recording);
        }
        MatrixExpectation::ResponseIdentityExecutionEvidence => {
            let recording = named_recording(&recordings, "response_identity");
            let (route, completion) =
                complete_websocket(row, std::slice::from_ref(recording)).await;
            let completion =
                completion.unwrap_or_else(|error| panic!("{} failed: {error:?}", row.variation));
            assert_identity_evidence(&completion, dialect, recording);
            assert_replay_origins(&completion.parts, &route);
            let failure = named_recording(&recordings, "failed_response");
            let (_, error) = complete_websocket(row, std::slice::from_ref(failure)).await;
            assert_failed_identity(
                dialect,
                &error.expect_err("recorded WebSocket failure must fail"),
            );
        }
        MatrixExpectation::MultiResetRetrySequence => {
            let events = Arc::new(Mutex::new(Vec::new()));
            let (_, completion) = complete_websocket_with_events(row, &recordings, &events).await;
            let completion =
                completion.unwrap_or_else(|error| panic!("{} failed: {error:?}", row.variation));
            assert_eq!(completion.full_text, "matrix retry settled");
            assert_eq!(completion.call_record.attempts.len(), 3);
            assert_eq!(
                completion
                    .call_record
                    .attempts
                    .iter()
                    .map(|attempt| attempt.outcome)
                    .collect::<Vec<_>>(),
                [
                    AttemptOutcome::Interrupted,
                    AttemptOutcome::Interrupted,
                    AttemptOutcome::Completed
                ]
            );
            assert_retry_stream_reduction(dialect, &events.lock_recover(), &completion);
        }
    }
}

async fn complete_websocket(
    row: &MatrixRow,
    recordings: &[Recording],
) -> (
    lash_core::ProviderRouteIdentity,
    Result<ProviderCompletion, ProviderCompletionError>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    complete_websocket_with_events(row, recordings, &events).await
}

async fn complete_websocket_with_events(
    row: &MatrixRow,
    recordings: &[Recording],
    events: &Arc<Mutex<Vec<LlmStreamEvent>>>,
) -> (
    lash_core::ProviderRouteIdentity,
    Result<ProviderCompletion, ProviderCompletionError>,
) {
    let (route, result, _) =
        complete_websocket_with_events_and_capture(row, recordings, events).await;
    (route, result)
}

async fn complete_websocket_with_events_and_capture(
    row: &MatrixRow,
    recordings: &[Recording],
    events: &Arc<Mutex<Vec<LlmStreamEvent>>>,
) -> (
    lash_core::ProviderRouteIdentity,
    Result<ProviderCompletion, ProviderCompletionError>,
    Vec<Value>,
) {
    let actions = recordings
        .iter()
        .map(|recording| {
            if recording.transport_error.is_some() {
                ScriptedWsAction::CloseBeforeStart
            } else {
                ScriptedWsAction::RecordedFrames {
                    frames: recording.events.iter().map(RecordedPayload::wire).collect(),
                    close_after_frames: recording.close_after_events,
                }
            }
        })
        .collect();
    let server = spawn_scripted_websocket(actions).await;
    let mut provider = ProviderHandle::new(
        CodexProvider::new("access", "refresh", 0)
            .force_websocket_transport()
            .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
            .with_options(ProviderOptions {
                expose_thinking: true,
                reliability: ProviderReliability::codex()
                    .max_attempts(3)
                    .base_delay_ms(0)
                    .max_delay_ms(0)
                    .stream_chunk_timeout_ms(Some(2_000)),
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    let route = provider.route_identity(dialect_model("codex.responses-websocket"));
    let result = provider
        .complete(matrix_request(
            "codex.responses-websocket",
            row,
            Arc::clone(events),
        ))
        .await;
    (route, result, server.captured())
}

async fn run_unsupported_stop_websocket(row: &MatrixRow, recordings: &[Recording]) {
    let recording = only_recording(recordings, "codex.responses-websocket", &row.variation);
    let events = Arc::new(Mutex::new(Vec::new()));
    let (_, completion, captured) =
        complete_websocket_with_events_and_capture(row, std::slice::from_ref(recording), &events)
            .await;
    let completion = completion.expect("unsupported WebSocket stop fixture completes literally");
    assert_eq!(captured.len(), 1, "Codex must send one response.create");
    assert_eq!(captured[0]["type"], "response.create");
    assert!(
        !json_contains_key(&captured[0], "stop")
            && !json_contains_key(&captured[0], "stop_sequences"),
        "Codex WebSocket serialized an unsupported wire stop field: {}",
        captured[0]
    );
    assert_unsupported_stop_disposition("codex.responses-websocket", &completion, recording);
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key) || object.values().any(|value| json_contains_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| json_contains_key(value, key)),
        _ => false,
    }
}

async fn run_unsupported_stop_http(dialect: &str, row: &MatrixRow, recordings: &[Recording]) {
    let recording = only_recording(recordings, dialect, &row.variation);
    let completion = complete_http(dialect, row, std::slice::from_ref(recording))
        .await
        .unwrap_or_else(|error| panic!("{dialect} unsupported stop fixture failed: {error:?}"));
    assert_unsupported_stop_disposition(dialect, &completion, recording);
}

fn assert_unsupported_stop_disposition(
    dialect: &str,
    completion: &ProviderCompletion,
    recording: &Recording,
) {
    assert!(completion.full_text.contains(LASHLANG_CLOSE_DELIMITER));
    let disposition = completion
        .generation_disposition
        .as_ref()
        .unwrap_or_else(|| panic!("{dialect} omitted generation disposition"));
    assert_eq!(
        disposition.stop_sequences,
        lash_core::GenerationOptionDisposition::OmittedUnsupported,
        "{dialect} must report its unsupported wire-stop field"
    );
    assert_eq!(
        completion.call_record.attempts[0]
            .generation_disposition
            .as_ref(),
        Some(disposition)
    );
    assert_current_success_contract(dialect, completion, recording);
}

async fn run_http_row(dialect: &str, row: &MatrixRow, recordings: Vec<Recording>) {
    match row.expectation {
        MatrixExpectation::StopBoundary { literal_present } => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let completion = complete_http(dialect, row, std::slice::from_ref(recording))
                .await
                .unwrap_or_else(|error| panic!("{} {} failed: {error:?}", row.variation, dialect));
            assert_eq!(completion.terminal_reason, LlmTerminalReason::Stop);
            assert_eq!(
                completion.full_text.contains(LASHLANG_CLOSE_DELIMITER),
                literal_present,
                "{} {dialect} violated the shared delimiter expectation",
                row.variation
            );
            assert_current_success_contract(dialect, &completion, recording);
        }
        MatrixExpectation::EquivalentOutcomeMapping => {
            let expectations = [
                ("stop", LlmTerminalReason::Stop),
                ("tool", LlmTerminalReason::ToolUse),
                ("length", LlmTerminalReason::OutputLimit),
                ("content_filter", LlmTerminalReason::ContentFilter),
            ];
            for (case, expected) in expectations {
                let recording = named_recording(&recordings, case);
                let completion = complete_http(dialect, row, std::slice::from_ref(recording))
                    .await
                    .unwrap_or_else(|error| {
                        panic!("{} {dialect} {case} failed: {error:?}", row.variation)
                    });
                assert_eq!(
                    completion.terminal_reason, expected,
                    "{} {dialect} {case} must normalize identically",
                    row.variation
                );
                if case == "tool" {
                    assert_lookup_tool(&completion.parts, dialect, &row.variation);
                }
                assert_current_success_contract(dialect, &completion, recording);
            }
        }
        MatrixExpectation::MissingTerminalEvidence => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let events = Arc::new(Mutex::new(Vec::new()));
            let error =
                complete_http_with_events(dialect, row, std::slice::from_ref(recording), &events)
                    .await
                    .expect_err("missing terminal evidence must fail end-to-end");
            assert_eq!(error.error.kind, ProviderFailureKind::Stream);
            assert!(
                error.error.code.is_some(),
                "{dialect} must classify the missing terminal"
            );
            let partial = error
                .error
                .partial_response
                .as_deref()
                .expect("missing-terminal failure carries parsed partial state");
            assert_eq!(partial.terminal_reason, LlmTerminalReason::Unknown);
            assert_eq!(partial.full_text, "matrix partial");
            assert_eq!(error.call_record.attempts.len(), 1);
            assert_eq!(
                error.call_record.attempts[0].protocol_position,
                ProtocolPosition::OutputStarted
            );
            assert_current_partial_contract(dialect, partial);
        }
        MatrixExpectation::EventShapeVariation => {
            for case in ["empty_deltas", "empty_terminal_chunks"] {
                let recording = named_recording(&recordings, case);
                let events = Arc::new(Mutex::new(Vec::new()));
                let completion = complete_http_with_events(
                    dialect,
                    row,
                    std::slice::from_ref(recording),
                    &events,
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("{} {dialect} {case} failed: {error:?}", row.variation)
                });
                assert_eq!(completion.full_text, "matrix shape");
                assert_eq!(completion.terminal_reason, LlmTerminalReason::Stop);
                assert_current_success_contract(dialect, &completion, recording);
                assert_no_empty_stream_output(dialect, case, &events.lock_recover());
            }
            let recording = named_recording(&recordings, "split_reasoning_tool");
            let completion = complete_http(dialect, row, std::slice::from_ref(recording))
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{} {dialect} split reasoning/tool failed: {error:?}",
                        row.variation
                    )
                });
            assert_eq!(completion.terminal_reason, LlmTerminalReason::ToolUse);
            assert_reasoning(
                &completion.parts,
                "matrix reasoning",
                dialect,
                &row.variation,
            );
            assert_lookup_tool(&completion.parts, dialect, &row.variation);
            assert_current_success_contract(dialect, &completion, recording);
        }
        MatrixExpectation::ReasoningRenderedPath => {
            let recording = only_recording(&recordings, dialect, &row.variation);
            let completion = complete_http(dialect, row, std::slice::from_ref(recording))
                .await
                .unwrap_or_else(|error| panic!("{} {dialect} failed: {error:?}", row.variation));
            assert_reasoning(
                &completion.parts,
                "matrix reasoning",
                dialect,
                &row.variation,
            );
            assert_reasoning_rendered(&completion.parts, dialect);
            assert_replay_origins(&completion.parts, &completion_route(dialect));
            assert_current_success_contract(dialect, &completion, recording);
        }
        MatrixExpectation::ResponseIdentityExecutionEvidence => {
            let recording = named_recording(&recordings, "response_identity");
            let completion = complete_http(dialect, row, std::slice::from_ref(recording))
                .await
                .unwrap_or_else(|error| panic!("{} {dialect} failed: {error:?}", row.variation));
            assert_identity_evidence(&completion, dialect, recording);
            assert_replay_origins(&completion.parts, &completion_route(dialect));
            let failure = named_recording(&recordings, "failed_response");
            let error = complete_http(dialect, row, std::slice::from_ref(failure))
                .await
                .expect_err("recorded HTTP failure must fail");
            assert_failed_identity(dialect, &error);
            if matches!(dialect, "openai.responses" | "codex.responses-sse") {
                let buffered = named_recording(&recordings, "buffered_response_identity");
                let completion = complete_http_buffered(dialect, row, buffered)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("{dialect} buffered identity fixture failed: {error:?}")
                    });
                assert_identity_evidence(&completion, dialect, buffered);
                assert_replay_origins(&completion.parts, &completion_route(dialect));
            }
        }
        MatrixExpectation::MultiResetRetrySequence => {
            let events = Arc::new(Mutex::new(Vec::new()));
            let completion = complete_http_with_events(dialect, row, &recordings, &events)
                .await
                .unwrap_or_else(|error| panic!("{} {dialect} failed: {error:?}", row.variation));
            assert_eq!(completion.full_text, "matrix retry settled");
            assert_eq!(completion.call_record.attempts.len(), 3);
            let interrupted = if dialect == "openai.chat-completions" {
                AttemptOutcome::Failed
            } else {
                AttemptOutcome::Interrupted
            };
            assert_eq!(
                completion
                    .call_record
                    .attempts
                    .iter()
                    .map(|attempt| attempt.outcome)
                    .collect::<Vec<_>>(),
                [interrupted, interrupted, AttemptOutcome::Completed,],
                "{dialect} retry outcomes changed"
            );
            let events = events.lock_recover();
            assert_retry_stream_reduction(dialect, &events, &completion);
        }
    }
}

async fn complete_http(
    dialect: &str,
    row: &MatrixRow,
    recordings: &[Recording],
) -> Result<ProviderCompletion, ProviderCompletionError> {
    let events = Arc::new(Mutex::new(Vec::new()));
    complete_http_with_events(dialect, row, recordings, &events).await
}

async fn complete_http_buffered(
    dialect: &str,
    row: &MatrixRow,
    recording: &Recording,
) -> Result<ProviderCompletion, ProviderCompletionError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts([recorded_script(
        dialect,
        &row.variation,
        recording,
    )]));
    let mut provider = http_provider(dialect, transport);
    let mut request = matrix_request(dialect, row, Arc::new(Mutex::new(Vec::new())));
    request.stream_events = None;
    provider.complete(request).await
}

async fn complete_http_with_events(
    dialect: &str,
    row: &MatrixRow,
    recordings: &[Recording],
    events: &Arc<Mutex<Vec<LlmStreamEvent>>>,
) -> Result<ProviderCompletion, ProviderCompletionError> {
    let scripts = recordings
        .iter()
        .map(|recording| recorded_script(dialect, &row.variation, recording))
        .collect::<Vec<_>>();
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts(scripts));
    let mut provider = http_provider(dialect, transport);
    let mut options = provider.options();
    options.expose_thinking = true;
    options.reliability = ProviderReliability::default()
        .max_attempts(3)
        .base_delay_ms(0)
        .max_delay_ms(0);
    provider.set_options(options);
    provider
        .complete(matrix_request(dialect, row, Arc::clone(events)))
        .await
}

fn http_provider(dialect: &str, transport: Arc<ScriptedLlmHttpTransport>) -> ProviderHandle {
    let transport: Arc<dyn LlmHttpTransport> = transport;
    match dialect {
        "anthropic.messages" => ProviderHandle::new(
            AnthropicProvider::new("matrix-key")
                .with_base_url(Some("https://anthropic.matrix".to_string()))
                .with_transport(transport)
                .into_components(),
        ),
        "google.generate-content" => ProviderHandle::new(
            GoogleOAuthProvider::new("access", "refresh", 0)
                .with_project_id(Some("matrix-project".to_string()))
                .with_transport(transport)
                .into_components(),
        ),
        "openai.chat-completions" => ProviderHandle::new(
            OpenAiCompatibleProvider::new("matrix-key", "https://openai-chat.matrix")
                .with_transport(transport)
                .into_components(),
        ),
        "openai.responses" => ProviderHandle::new(
            OpenAiProvider::new("matrix-key")
                .with_transport(transport)
                .into_components(),
        ),
        "codex.responses-sse" => ProviderHandle::new(
            CodexProvider::new("access", "refresh", 0)
                .force_sse_transport()
                .with_http_transport(transport)
                .into_components(),
        ),
        other => panic!("unsupported HTTP matrix dialect {other}"),
    }
}

fn matrix_request(
    dialect: &str,
    row: &MatrixRow,
    events: Arc<Mutex<Vec<LlmStreamEvent>>>,
) -> LlmRequest {
    let model = dialect_model(dialect);
    let model_capability = lash_core::ModelCapability {
        stream_termination: Some(StreamTermination::RequireTerminalEvidence),
        ..Default::default()
    };
    LlmRequest {
        model: model.to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability,
        generation: lash_core::GenerationOptions {
            stop_sequences: (row.variation == "stop_consumed")
                .then(|| LASHLANG_CLOSE_DELIMITER.to_string())
                .into_iter()
                .collect(),
            ..Default::default()
        },
        scope: LlmRequestScope::new("matrix-session", "matrix-frame", "matrix-request"),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(move |event| {
            events.lock_recover().push(event);
        })),
        provider_trace: None,
    }
}

fn recorded_script(dialect: &str, variation: &str, recording: &Recording) -> ProviderWireScript {
    let timeline = if let Some(message) = &recording.transport_error {
        vec![ProviderWireEvent::TransportError {
            at: 0,
            message: message.clone(),
            retryable: Some(recording.retryable),
        }]
    } else {
        let mut timeline = vec![ProviderWireEvent::ResponseStart {
            at: 0,
            status: recording.status,
            headers: recording.headers.clone(),
        }];
        if let Some(body) = &recording.body {
            assert!(recording.events.is_empty());
            timeline.push(ProviderWireEvent::Body {
                at: 1,
                data: body.clone(),
            });
        } else {
            timeline.extend(recording.events.iter().enumerate().map(|(index, event)| {
                ProviderWireEvent::Sse {
                    at: index as u64 + 1,
                    data: event.wire(),
                }
            }));
        }
        timeline.push(ProviderWireEvent::End {
            at: recording.events.len() as u64 + u64::from(recording.body.is_some()) + 1,
        });
        timeline
    };
    ProviderWireScript {
        schema: crate::provider::PROVIDER_WIRE_SCRIPT_SCHEMA.to_string(),
        name: format!("{dialect}.{variation}.{}", recording.case),
        provider_kind: dialect.to_string(),
        endpoint: ProviderWireEndpoint {
            method: "POST".to_string(),
            path: dialect_path(dialect).to_string(),
        },
        request_match: recording.request_match.clone(),
        timeline,
        expected_provider: None,
        provenance: None,
    }
}

fn materialize_recordings(dialect: &str, cell: &MatrixCell) -> Option<Vec<Recording>> {
    match cell {
        MatrixCell::RecordedStreams { recordings } => Some(recordings.clone()),
        MatrixCell::ProviderWireScripts { paths } => Some(
            paths
                .iter()
                .map(|path| recording_from_provider_wire_script(dialect, path))
                .collect(),
        ),
        MatrixCell::NotApplicable { .. } => None,
    }
}

fn recording_from_provider_wire_script(dialect: &str, path: &str) -> Recording {
    let script_path = matrix_dir().join(path);
    let content = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", script_path.display()));
    let script = ProviderWireScript::from_json_str(&content)
        .unwrap_or_else(|error| panic!("parse {}: {error}", script_path.display()));
    let mut headers = Vec::new();
    let mut status = 200;
    let mut events = Vec::new();
    let mut body = None;
    let mut transport_error = None;
    let mut retryable = false;
    for event in script.timeline {
        match event {
            ProviderWireEvent::ResponseStart {
                status: response_status,
                headers: response_headers,
                ..
            } => {
                status = response_status;
                headers = response_headers;
            }
            ProviderWireEvent::Body { data, .. } => body = Some(data),
            ProviderWireEvent::Sse { data, .. } => events.push(RecordedPayload {
                json: None,
                raw: Some(data),
            }),
            ProviderWireEvent::TransportError {
                message,
                retryable: is_retryable,
                ..
            } => {
                transport_error = Some(message);
                retryable = is_retryable.unwrap_or(false);
            }
            ProviderWireEvent::End { .. } => {}
            other => panic!("{dialect} matrix reference contains unsupported event {other:?}"),
        }
    }
    Recording {
        case: script
            .expected_provider
            .as_ref()
            .and_then(|expected| expected.get("variation"))
            .and_then(Value::as_str)
            .unwrap_or(&script.name)
            .to_string(),
        status,
        request_match: script.request_match,
        expected_finish_reason: script
            .expected_provider
            .as_ref()
            .and_then(|expected| expected.get("provider_finish_reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        headers,
        events,
        body,
        close_after_events: false,
        transport_error,
        retryable,
    }
}

fn only_recording<'a>(
    recordings: &'a [Recording],
    dialect: &str,
    variation: &str,
) -> &'a Recording {
    assert_eq!(
        recordings.len(),
        1,
        "{variation} {dialect} must contain exactly one recording"
    );
    &recordings[0]
}

fn named_recording<'a>(recordings: &'a [Recording], case: &str) -> &'a Recording {
    recordings
        .iter()
        .find(|recording| recording.case == case)
        .unwrap_or_else(|| panic!("missing matrix recording case {case}"))
}

fn assert_current_success_contract(
    dialect: &str,
    completion: &ProviderCompletion,
    recording: &Recording,
) {
    assert_eq!(completion.call_record.attempts.len(), 1);
    let attempt = &completion.call_record.attempts[0];
    assert_eq!(attempt.outcome, AttemptOutcome::Completed);
    assert_eq!(
        attempt.protocol_position,
        ProtocolPosition::TerminalObserved
    );
    assert!(
        completion.http_summary.is_some(),
        "{dialect} must retain transport evidence"
    );
    let evidence = completion
        .execution_evidence
        .as_ref()
        .unwrap_or_else(|| panic!("{dialect} must retain provider execution evidence"));
    assert_eq!(
        evidence.provider_finish_reason, recording.expected_finish_reason,
        "{dialect} must retain the exact provider-native finish reason"
    );
    assert_eq!(attempt.evidence, completion.execution_evidence);
}

fn assert_current_partial_contract(dialect: &str, partial: &lash_core::LlmResponse) {
    assert!(
        partial.http_summary.is_some(),
        "{dialect} partial must retain transport evidence"
    );
    assert!(
        partial.execution_evidence.is_some(),
        "{dialect} partial must retain execution evidence observed before EOF"
    );
    assert_replay_origins(&partial.parts, &completion_route(dialect));
}

fn assert_identity_evidence(completion: &ProviderCompletion, dialect: &str, recording: &Recording) {
    let evidence = completion
        .execution_evidence
        .as_ref()
        .unwrap_or_else(|| panic!("{dialect} dropped execution evidence"));
    assert_eq!(
        evidence.provider_response_id.as_deref(),
        Some("matrix-response-1")
    );
    assert_eq!(
        evidence.served_model.as_deref(),
        Some("matrix-served-model")
    );
    if dialect == "codex.responses-websocket" {
        assert_eq!(evidence.provider_request_id, None);
    } else {
        assert_eq!(
            evidence.provider_request_id.as_deref(),
            Some("matrix-request-1")
        );
    }
    assert_eq!(evidence.reasoning_output_tokens, Some(0));
    assert_eq!(
        evidence.provider_finish_reason, recording.expected_finish_reason,
        "{dialect} changed its provider-native finish reason"
    );
    assert_eq!(
        completion.call_record.attempts[0].evidence.as_ref(),
        Some(evidence)
    );
}

fn assert_failed_identity(dialect: &str, error: &ProviderCompletionError) {
    assert_eq!(error.call_record.attempts.len(), 1);
    let attempt = &error.call_record.attempts[0];
    let evidence = attempt
        .evidence
        .as_ref()
        .unwrap_or_else(|| panic!("{dialect} failed attempt dropped execution evidence"));
    if dialect == "codex.responses-websocket" {
        let partial = error
            .error
            .partial_response
            .as_deref()
            .expect("failed WebSocket response retains parsed partial state");
        assert_eq!(partial.execution_evidence.as_ref(), Some(evidence));
        assert_eq!(
            evidence.provider_response_id.as_deref(),
            Some("matrix-failed-response")
        );
        assert_eq!(
            evidence.served_model.as_deref(),
            Some("matrix-served-model")
        );
        assert_eq!(evidence.reasoning_output_tokens, Some(0));
        assert_eq!(evidence.provider_finish_reason.as_deref(), Some("failed"));
    } else {
        assert_eq!(error.error.status, Some(400));
        assert_eq!(
            evidence.provider_request_id.as_deref(),
            Some("matrix-failed-request")
        );
        assert_eq!(
            attempt
                .error
                .as_ref()
                .and_then(|error| error.provider_request_id.as_deref()),
            Some("matrix-failed-request")
        );
    }
}

fn assert_no_empty_stream_output(dialect: &str, case: &str, events: &[LlmStreamEvent]) {
    assert!(
        events.iter().all(|event| match event {
            LlmStreamEvent::Delta(text) | LlmStreamEvent::ReasoningDelta(text) => !text.is_empty(),
            LlmStreamEvent::Part(LlmOutputPart::Text { text, .. })
            | LlmStreamEvent::Part(LlmOutputPart::Reasoning { text, .. }) => !text.is_empty(),
            _ => true,
        }),
        "{dialect} {case} leaked an empty downstream output event: {events:?}"
    );
}

fn assert_retry_stream_reduction(
    dialect: &str,
    events: &[LlmStreamEvent],
    completion: &ProviderCompletion,
) {
    let segments = events
        .split(|event| matches!(event, LlmStreamEvent::AttemptReset))
        .collect::<Vec<_>>();
    assert_eq!(
        segments.len(),
        3,
        "{dialect} must place exactly one reset between each attempt: {events:?}"
    );
    for (index, segment) in segments[..2].iter().enumerate() {
        assert!(
            segment
                .iter()
                .any(|event| matches!(event, LlmStreamEvent::Evidence(_))),
            "{dialect} attempt {} emitted no attempt-local evidence",
            index + 1
        );
        assert!(
            segment.iter().all(|event| {
                !matches!(
                    event,
                    LlmStreamEvent::Delta(_)
                        | LlmStreamEvent::ReasoningDelta(_)
                        | LlmStreamEvent::Part(_)
                        | LlmStreamEvent::Usage(_)
                )
            }),
            "{dialect} attempt {} unexpectedly emitted billable output",
            index + 1
        );
        let identity_kind = if dialect == "codex.responses-websocket" {
            "response"
        } else {
            "request"
        };
        let expected_identity = format!("matrix-{identity_kind}-attempt-{}", index + 1);
        assert!(
            segment.iter().any(|event| match event {
                LlmStreamEvent::Evidence(evidence) =>
                    evidence.execution_evidence.as_ref().and_then(|evidence| {
                        if dialect == "codex.responses-websocket" {
                            evidence.provider_response_id.as_deref()
                        } else {
                            evidence.provider_request_id.as_deref()
                        }
                    }) == Some(expected_identity.as_str()),
                _ => false,
            }),
            "{dialect} attempt {} identity was not observable before reset",
            index + 1
        );
    }
    assert!(
        segments[2]
            .iter()
            .any(|event| matches!(event, LlmStreamEvent::Evidence(_))),
        "{dialect} final attempt emitted no evidence"
    );

    let mut accumulated_text = String::new();
    let mut accumulated_evidence = LlmStreamEvidence::default();
    for event in events {
        match event {
            LlmStreamEvent::AttemptReset => {
                accumulated_text.clear();
                accumulated_evidence = LlmStreamEvidence::default();
            }
            LlmStreamEvent::Delta(delta) => accumulated_text.push_str(delta),
            LlmStreamEvent::Evidence(evidence) => accumulated_evidence
                .merge(evidence.clone())
                .expect("matrix retry evidence remains monotonic within an attempt"),
            LlmStreamEvent::ReasoningDelta(_)
            | LlmStreamEvent::Part(_)
            | LlmStreamEvent::Usage(_)
            | LlmStreamEvent::RetryStatus { .. } => {}
        }
    }
    assert_eq!(accumulated_text, completion.full_text);
    assert_eq!(
        accumulated_evidence.execution_evidence, completion.execution_evidence,
        "{dialect} reset reduction retained stale execution evidence"
    );
    assert_eq!(
        accumulated_evidence.provider_usage, completion.provider_usage,
        "{dialect} reset reduction retained stale provider usage"
    );
}

fn assert_lookup_tool(parts: &[LlmOutputPart], dialect: &str, variation: &str) {
    let tool = parts.iter().find_map(|part| match part {
        LlmOutputPart::ToolCall {
            tool_name,
            input_json,
            ..
        } => Some((tool_name, input_json)),
        _ => None,
    });
    let (tool_name, input_json) =
        tool.unwrap_or_else(|| panic!("{variation} {dialect} produced no tool call: {parts:?}"));
    assert_eq!(tool_name, "lookup");
    assert_eq!(
        serde_json::from_str::<Value>(input_json).expect("matrix tool JSON"),
        json!({"q": "x"})
    );
}

fn assert_reasoning(parts: &[LlmOutputPart], expected: &str, dialect: &str, variation: &str) {
    let reasoning = parts.iter().find_map(|part| match part {
        LlmOutputPart::Reasoning { text, .. } => Some(text.as_str()),
        _ => None,
    });
    assert_eq!(
        reasoning,
        Some(expected),
        "{variation} {dialect} lost reasoning on the normalized path: {parts:?}"
    );
}

fn assert_reasoning_rendered(parts: &[LlmOutputPart], dialect: &str) {
    let replayable = parts.iter().any(|part| {
        matches!(part, LlmOutputPart::Reasoning { replay: Some(replay), .. } if !replay.is_empty())
    });
    let expected_replayable = dialect != "openai.chat-completions";
    assert_eq!(
        replayable, expected_replayable,
        "{dialect} did not preserve the fixture's expected reasoning replay state"
    );
    let history_parts = parts
        .iter()
        .enumerate()
        .map(|(index, part)| match part {
            LlmOutputPart::Text {
                text,
                response_meta,
            } => lash_sansio::session_model::Part::prose(
                format!("matrix-assistant.p{index}"),
                text.clone(),
                response_meta.clone(),
            ),
            LlmOutputPart::Reasoning { text, replay } => {
                lash_sansio::reasoning_part("matrix-assistant", index, text.clone(), replay.clone())
            }
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => lash_sansio::session_model::Part::tool_call(
                format!("matrix-assistant.p{index}"),
                input_json.clone(),
                call_id.clone(),
                tool_name.clone(),
                replay.clone(),
            ),
        })
        .collect();
    let rendered = render_prompt(&[Message {
        id: "matrix-assistant".to_string(),
        role: MessageRole::Assistant,
        parts: shared_parts(history_parts),
        origin: None,
    }]);
    let rendered_reasoning = rendered
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| {
            matches!(block, lash_sansio::llm::types::LlmContentBlock::Reasoning { text, .. } if text == "matrix reasoning")
        });
    assert_eq!(
        rendered_reasoning, expected_replayable,
        "{dialect} rendered reasoning must preserve replayable state and drop display-only state: {:?}",
        rendered.messages
    );
    assert!(
        rendered.messages.iter().flat_map(|message| message.blocks.iter()).all(|block| {
            !matches!(block, lash_sansio::llm::types::LlmContentBlock::Text { text, .. } if text.contains("matrix reasoning"))
        }),
        "{dialect} reasoning leaked into visible assistant text"
    );
}

fn assert_replay_origins(parts: &[LlmOutputPart], route: &lash_core::ProviderRouteIdentity) {
    for part in parts {
        let origin = match part {
            LlmOutputPart::Text {
                response_meta: Some(meta),
                ..
            } if !meta.is_empty() => meta.origin.as_ref(),
            LlmOutputPart::Reasoning {
                replay: Some(replay),
                ..
            } if !replay.is_empty() => replay.origin.as_ref(),
            LlmOutputPart::ToolCall {
                replay: Some(replay),
                ..
            } if !replay.is_empty() => replay.origin.as_ref(),
            _ => continue,
        };
        assert_eq!(
            origin,
            Some(route),
            "provider-owned replay state was not stamped"
        );
    }
}

fn completion_route(dialect: &str) -> lash_core::ProviderRouteIdentity {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts([]));
    http_provider(dialect, transport).route_identity(dialect_model(dialect))
}

fn dialect_model(dialect: &str) -> &'static str {
    match dialect {
        "anthropic.messages" => "claude-matrix",
        "google.generate-content" => "gemini-matrix",
        "openai.chat-completions" => "openai/matrix",
        "openai.responses" => "gpt-matrix",
        "codex.responses-sse" | "codex.responses-websocket" => "gpt-matrix-codex",
        other => panic!("unknown matrix dialect {other}"),
    }
}

fn dialect_path(dialect: &str) -> &'static str {
    match dialect {
        "anthropic.messages" => "/v1/messages",
        "google.generate-content" => "/v1internal:streamGenerateContent",
        "openai.chat-completions" => "/chat/completions",
        "openai.responses" => "/responses",
        "codex.responses-sse" => "/backend-api/codex/responses",
        other => panic!("unknown HTTP matrix dialect {other}"),
    }
}

fn matrix_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("provider-scripts/variation-matrix/v1")
}

fn matrix_generator() -> PathBuf {
    matrix_dir()
        .parent()
        .expect("version directory has generator parent")
        .join("generate.py")
}
