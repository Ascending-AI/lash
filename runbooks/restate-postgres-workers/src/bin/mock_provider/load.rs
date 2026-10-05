use super::*;
use lash_restate_postgres_workers_e2e::load::{
    CRON_SETUP_MARKER, QUEUED_MARKER, TURN_MARKER, WORKLOAD_MARKER, behavior,
};
use tokio::io::AsyncWriteExt as _;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LoadOperation {
    Turn { key: String, workload: String },
    Queued { key: String, workload: String },
    CronSetup { run: String, workload: String },
}
impl LoadOperation {
    fn key(&self) -> String {
        match self {
            Self::Turn { key, .. } | Self::Queued { key, .. } => key.clone(),
            Self::CronSetup { run, .. } => format!("{run}/cron"),
        }
    }
    fn workload(&self) -> &str {
        match self {
            Self::Turn { workload, .. }
            | Self::Queued { workload, .. }
            | Self::CronSetup { workload, .. } => workload,
        }
    }
    fn scenario(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "load_turn",
            Self::Queued { .. } => "load_queued",
            Self::CronSetup { .. } => "load_cron_setup",
        }
    }
    fn admission_text(&self) -> String {
        let (marker, value) = match self {
            Self::Turn { key, .. } => (TURN_MARKER, key),
            Self::Queued { key, .. } => (QUEUED_MARKER, key),
            Self::CronSetup { run, .. } => (CRON_SETUP_MARKER, run),
        };
        format!("{marker}{value} {WORKLOAD_MARKER}{}\n", self.workload())
    }
}
const ADMISSION_OPEN: &str = "<load-admission>\n";
const ADMISSION_CLOSE: &str = "</load-admission>\n";

fn admission_prefix(text: &str) -> String {
    format!("{ADMISSION_OPEN}{text}{ADMISSION_CLOSE}")
}
fn marker_value(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)? + marker.len();
    let value: String = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/'))
        .take(if marker == WORKLOAD_MARKER {
            64
        } else {
            usize::MAX
        })
        .collect();
    (!value.is_empty()).then_some(value)
}
pub(super) fn load_operations_text(text: &str) -> Vec<LoadOperation> {
    let mut positions: Vec<_> = [TURN_MARKER, QUEUED_MARKER, CRON_SETUP_MARKER]
        .into_iter()
        .flat_map(|marker| {
            text.match_indices(marker)
                .map(move |(position, _)| (position, marker))
        })
        .collect();
    positions.sort_by_key(|(position, _)| *position);
    positions
        .iter()
        .enumerate()
        .filter_map(|(index, (position, marker))| {
            let end = positions
                .get(index + 1)
                .map_or(text.len(), |(position, _)| *position);
            let tail = &text[*position..end];
            let value = marker_value(tail, marker)?;
            let workload = marker_value(tail, WORKLOAD_MARKER)?;
            Some(match *marker {
                TURN_MARKER => LoadOperation::Turn {
                    key: value,
                    workload,
                },
                QUEUED_MARKER => LoadOperation::Queued {
                    key: value,
                    workload,
                },
                _ => LoadOperation::CronSetup {
                    run: value,
                    workload,
                },
            })
        })
        .collect()
}
/// New inputs follow the last assistant response. An unfinished RLM cell
/// carries its admission into later iterations; iteration one never inherits
/// an earlier run's admission, including a run that failed without an answer.
fn admitted_user_texts(request: &Value) -> Vec<String> {
    let Some(messages) = request["messages"].as_array() else {
        return vec![];
    };
    let assistant = messages
        .iter()
        .rposition(|message| message["role"] == "assistant");
    let continuing = messages.last().is_some_and(|message| {
        message["role"] == "user"
            && message_content_text(message)
                .split_once("=== CURRENT ITERATION: ")
                .and_then(|(_, tail)| tail.split_once(" ==="))
                .and_then(|(iteration, _)| iteration.parse::<usize>().ok())
                .is_some_and(|iteration| iteration > 1)
    });
    let mut texts = Vec::new();
    if continuing && let Some(index) = assistant {
        let content = message_content_text(&messages[index]);
        if let Some((admission, _)) = content
            .strip_prefix(ADMISSION_OPEN)
            .and_then(|tail| tail.split_once(ADMISSION_CLOSE))
        {
            texts.push(admission.to_owned());
        }
    }
    texts.extend(
        messages[assistant.map_or(0, |index| index + 1)..]
            .iter()
            .filter(|message| message["role"] == "user")
            .map(message_content_text),
    );
    texts
}
fn admitted_behavior_text(request: &Value) -> Option<String> {
    admitted_user_texts(request)
        .into_iter()
        .rev()
        .find(|text| text.contains(behavior::MARKER))
}
pub(super) fn admitted_operations(request: &Value) -> Vec<LoadOperation> {
    let mut operations = Vec::new();
    for text in admitted_user_texts(request) {
        for operation in load_operations_text(&text) {
            if !operations
                .iter()
                .any(|existing: &LoadOperation| existing.key() == operation.key())
            {
                operations.push(operation);
            }
        }
    }
    operations
}
fn internal(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn chat(request_id: &str, model: &str, text: &str, tokens: u64) -> Value {
    json!({"id":request_id,"object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":tokens,"completion_tokens":31,"total_tokens":tokens+31}})
}
pub(super) async fn completion(
    state: &AppState,
    load: &LoadContext,
    operations: Vec<LoadOperation>,
    request: &Value,
    request_id: &str,
) -> Result<axum::response::Response, (StatusCode, String)> {
    for operation in &operations {
        load.require_workload(operation.workload())
            .map_err(internal)?;
    }
    let first = operations
        .first()
        .ok_or_else(|| internal("no admitted input"))?;
    let keys: Vec<_> = operations.iter().map(LoadOperation::key).collect();
    let mut retry_keys = Vec::new();
    for operation in &operations {
        if let LoadOperation::Turn { key, .. } = operation {
            let (id, _) = lash_perf::workload::OperationId::parse(key).map_err(internal)?;
            if load
                .generator(&id.run)
                .map_err(internal)?
                .plan(id.actor, id.ordinal)
                .map_err(internal)?
                .retryable_first_attempt
            {
                retry_keys.push(key.clone());
            }
        }
    }
    // A newly admitted primary input owns its retry decision even when an
    // earlier queued input already had a provider call under another run.
    let first_attempt: bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM unnest($1::text[]) AS input(key) WHERE NOT EXISTS (SELECT 1 FROM witness_provider_receipts WHERE workflow_id=input.key AND scenario IN ('load_turn','load_retryable')))")
        .bind(&retry_keys).fetch_one(&state.witness).await.map_err(internal)?;
    let attempt = if first_attempt { 1 } else { 2 };
    let mut response = match first {
        LoadOperation::CronSetup { run, .. } => load
            .generator(run)
            .map_err(internal)?
            .cron_setup_response()
            .map_err(internal)?,
        _ => {
            let (id, _) = lash_perf::workload::OperationId::parse(&keys[0]).map_err(internal)?;
            load.generator(&id.run)
                .map_err(internal)?
                .admitted_response(&keys, attempt)
                .map_err(internal)?
        }
    };
    let model = request["model"].as_str().unwrap_or("unknown");
    if response.retryable {
        let body = json!({"error":{"message":"synthetic retryable first attempt","type":"rate_limit_exceeded","code":"rate_limit_exceeded"}});
        for operation in &operations {
            witness::record_provider_receipt(
                &state.witness,
                &format!("{request_id}-{}", operation.key()),
                "load_retryable",
                &operation.key(),
                model,
                request,
                &body,
            )
            .await
            .map_err(internal)?;
        }
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            Json(body),
        )
            .into_response());
    }
    let admission = operations
        .iter()
        .map(LoadOperation::admission_text)
        .collect::<String>();
    let prefix = admission_prefix(&admission);
    response.text.insert_str(0, &prefix);
    if let Some(chunk) = response.chunks.first_mut() {
        chunk.text.insert_str(0, &prefix);
    }
    let body = chat(request_id, model, &response.text, 17);
    for operation in &operations {
        witness::record_provider_receipt(
            &state.witness,
            &format!("{request_id}-{}", operation.key()),
            operation.scenario(),
            &operation.key(),
            model,
            request,
            &body,
        )
        .await
        .map_err(internal)?;
    }
    if response.chunks.len() > 1 {
        return streamed(
            state,
            operations,
            request.clone(),
            request_id.to_owned(),
            response.chunks,
        )
        .await;
    }
    let delay = response.chunks.last().map_or(0, |chunk| chunk.due_ms);
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(delay))).await;
    Ok(Json(body).into_response())
}
async fn streamed(
    state: &AppState,
    operations: Vec<LoadOperation>,
    request: Value,
    id: String,
    chunks: Vec<lash_perf::workload::ProviderChunk>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let (writer, reader) = tokio::io::duplex(16 * 1024);
    let witness = state.witness.clone();
    let model = request["model"].as_str().unwrap_or("unknown").to_owned();
    tokio::spawn(async move {
        let mut writer = writer;
        let started = tokio::time::Instant::now();
        for chunk in &chunks {
            tokio::time::sleep_until(
                started + std::time::Duration::from_millis(u64::from(chunk.due_ms)),
            )
            .await;
            let data = json!({"id":id,"object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":chunk.text},"finish_reason":null}]});
            if writer
                .write_all(format!("data: {data}\n\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        let data = json!({"id":id,"object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":31,"total_tokens":48}});
        if writer
            .write_all(format!("data: {data}\n\ndata: [DONE]\n\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        for operation in &operations {
            if let Err(error) = witness::record_provider_receipt(
                &witness,
                &format!("{id}-stream-{}", operation.key()),
                &format!("load_stream_chunks_{}", chunks.len()),
                &operation.key(),
                &model,
                &request,
                &data,
            )
            .await
            {
                tracing::error!(%error,"receipt streamed chunks");
            }
        }
    });
    Ok((
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
    )
        .into_response())
}

pub(super) async fn behavior_completion(
    state: &AppState,
    load: &LoadContext,
    request: &Value,
    id: &str,
) -> Result<Option<axum::response::Response>, (StatusCode, String)> {
    let Some(latest) = admitted_behavior_text(request) else {
        return Ok(None);
    };
    let Some(value) = marker_value(&latest, behavior::MARKER) else {
        return Ok(None);
    };
    let (run, phase) = value
        .rsplit_once('/')
        .ok_or_else(|| internal("behavior has no phase"))?;
    if phase == "llm" {
        let response = chat(
            id,
            request["model"].as_str().unwrap_or("unknown"),
            &json!({"kind":"value","value":behavior::AUXILIARY_ANSWER,"error":null}).to_string(),
            17,
        );
        witness::record_provider_receipt(
            &state.witness,
            id,
            "load_auxiliary",
            &format!("{run}/behaviors/llm"),
            request["model"].as_str().unwrap_or("unknown"),
            request,
            &response,
        )
        .await
        .map_err(internal)?;
        return Ok(Some(Json(response).into_response()));
    }
    let workload = marker_value(&latest, WORKLOAD_MARKER)
        .ok_or_else(|| internal("behavior has no workload"))?;
    load.require_workload(&workload).map_err(internal)?;
    let script = behavior::script(run, phase).map_err(internal)?;
    let admission = format!("{}{value} {WORKLOAD_MARKER}{workload}\n", behavior::MARKER);
    let script = format!("{}{script}", admission_prefix(&admission));
    let response = chat(
        id,
        request["model"].as_str().unwrap_or("unknown"),
        &script,
        if phase == "pressure_usage" {
            100_001
        } else {
            17
        },
    );
    witness::record_provider_receipt(
        &state.witness,
        id,
        "load_behavior",
        &format!("{run}/behaviors/{phase}"),
        request["model"].as_str().unwrap_or("unknown"),
        request,
        &response,
    )
    .await
    .map_err(internal)?;
    Ok(Some(Json(response).into_response()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admitted_load_inputs_survive_an_unfinished_cell() {
        use lash::{
            direct::LlmOutputPart, provider::LlmContentBlock, provider::LlmResponse,
            provider::LlmRole,
        };
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::clone(&seen);
        let provider = lash::testing::TestProvider::builder()
            .kind("load-admission")
            .complete(move |request| {
                let messages: Vec<_> = request
                    .messages
                    .iter()
                    .map(|message| {
                        let content: Vec<_> = message
                            .blocks
                            .iter()
                            .filter_map(|block| match block {
                                LlmContentBlock::Text { text, .. } => {
                                    Some(json!({"type":"text","text":text}))
                                }
                                _ => None,
                            })
                            .collect();
                        let role = match message.role {
                            LlmRole::User => "user",
                            LlmRole::Assistant => "assistant",
                            LlmRole::System => "system",
                        };
                        json!({"role":role,"content":content})
                    })
                    .collect();
                let request = json!({"messages":messages});
                let operations = admitted_operations(&request);
                let keys: Vec<_> = operations.iter().map(LoadOperation::key).collect();
                let mut calls = calls.lock().unwrap();
                calls.push(keys.clone());
                // Fail before updating the previous turn's retained variable.
                let text = if calls.len() == 2 {
                    "<typescript>throw new Error(\"retry the load cell\");</typescript>".to_owned()
                } else {
                    let key = keys.last().map(String::as_str).unwrap_or("fallback");
                    format!(
                        "<typescript>const op={};finish({{synthetic:true,operation:op}});</typescript>",
                        serde_json::to_string(key).unwrap()
                    )
                };
                let admission = operations
                    .iter()
                    .map(LoadOperation::admission_text)
                    .collect::<String>();
                let text = format!("{}{text}", admission_prefix(&admission));
                async move {
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build();
        let restate = lash_restate_test::backend(4722, Default::default())
            .await
            .unwrap();
        let backend = restate.lash_backend();
        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            &backend,
        );
        let core = lash::LashCore::rlm_builder(backend, factory)
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .serve_test_llm_profile(
                provider.into_handle(),
                lash::LlmProfileMetadata::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .unwrap(),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "load-admission",
                "law",
            ))
            .unwrap();
        core.session(lash::SessionId::parse("load-admission").expect("nonblank host identity"))
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                "mock-model",
                lash::TurnBudget::bounded(3),
                lash::MaxToolCalls::new(16),
            )))
            .await
            .unwrap();
        let session = core
            .session(lash::SessionId::parse("load-admission").expect("nonblank host identity"))
            .open()
            .await
            .unwrap();
        for ordinal in [28, 29] {
            let key = format!("smoke-v1-20261001205043/2/{ordinal}");
            let input = format!(
                "Run the synthetic load turn. load_turn={key} load_workload={}\n{{\"payload\":\"oak\"}}\n## Synthetic load context\n\n{}",
                "a".repeat(64),
                "oak ".repeat(32_768),
            );
            let output = session
                .send(lash::TurnInput::text(input))
                .output()
                .await
                .unwrap();
            assert_eq!(
                output.final_value(),
                Some(&json!({"synthetic":true,"operation":key})),
                "the admitted turn must keep its operation on every model iteration: {:?}",
                *seen.lock().unwrap(),
            );
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                vec!["smoke-v1-20261001205043/2/28".to_owned()],
                vec!["smoke-v1-20261001205043/2/29".to_owned()],
                vec!["smoke-v1-20261001205043/2/29".to_owned()],
            ],
        );
    }

    #[test]
    fn admitted_continuation_merges_inputs_and_excludes_failed_history() {
        let mut request = json!({"messages":[
            {"role":"user","content":"load_turn=r/0/0 load_workload=w"},
            {"role":"assistant","content":"<load-admission>\nload_turn=r/0/0 load_workload=w\n</load-admission>\n<typescript>throw new Error('old failed run');</typescript>"},
            {"role":"user","content":"load_turn=r/0/1 load_workload=w"},
            {"role":"assistant","content":"<load-admission>\nload_turn=r/0/1 load_workload=w\nload_queued=r/0/1/queued/0 load_workload=w\n</load-admission>\n<typescript>throw new Error('active run');</typescript>"},
            {"role":"user","content":"history[3].error: active run"},
            {"role":"user","content":"load_queued=r/0/1/queued/1 load_workload=w"},
            {"role":"user","content":"=== CURRENT ITERATION: 2 ==="}
        ]});
        let keys = |request: &Value| {
            admitted_operations(request)
                .iter()
                .map(LoadOperation::key)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&request),
            ["r/0/1", "r/0/1/queued/0", "r/0/1/queued/1"]
        );
        request["messages"][6]["content"] = json!("=== CURRENT ITERATION: 1 ===");
        assert_eq!(keys(&request), ["r/0/1/queued/1"]);
    }

    #[test]
    fn admitted_prefix_excludes_history_and_includes_every_batched_input() {
        let request = json!({"messages":[{"role":"user","content":"load_turn=r/0/0 load_workload=w"},{"role":"assistant","content":"old answer"},{"role":"user","content":"load_turn=r/0/1 load_workload=w"},{"role":"user","content":"load_queued=r/0/0/queued/0 load_workload=w"}]});
        assert_eq!(
            admitted_operations(&request)
                .iter()
                .map(LoadOperation::key)
                .collect::<Vec<_>>(),
            ["r/0/1", "r/0/0/queued/0"]
        );
        let request = json!({"messages":[{"role":"user","content":"load_behavior=r/old load_workload=w"},{"role":"assistant","content":"old answer"},{"role":"user","content":"load_behavior=r/register load_workload=w"},{"role":"user","content":"CURRENT ITERATION: 1"}]});
        assert_eq!(
            admitted_behavior_text(&request).as_deref(),
            Some("load_behavior=r/register load_workload=w")
        );
        let historical = json!({"messages":[{"role":"user","content":"load_behavior=r/old load_workload=w"},{"role":"assistant","content":"old answer"},{"role":"user","content":"CURRENT ITERATION: 1"}]});
        assert_eq!(admitted_behavior_text(&historical), None);
        let hash = "a".repeat(64);
        // The runtime may concatenate an admitted prefix without separators.
        let merged = format!(
            "load_queued=r/0/0/queued/0 load_workload={hash}Run the synthetic turn. load_turn=r/0/1 load_workload={hash}"
        );
        let parsed = load_operations_text(&merged);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.iter().all(|operation| operation.workload() == hash));
    }
}
