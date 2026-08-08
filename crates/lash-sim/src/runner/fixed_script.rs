use super::*;

#[derive(Clone, Debug, Serialize)]
struct FixedScriptEvent {
    schema: &'static str,
    sequence: usize,
    event: &'static str,
    profile: &'static str,
    proof_alias: String,
    exchange_alias: String,
    proof_name: String,
    provider_kind: String,
    outcome: String,
    transcript_path: String,
    request: FixedScriptEventRequest,
    response: FixedScriptEventResponse,
    terminal_classification: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct FixedScriptEventRequest {
    method: String,
    path: String,
    header_names: Vec<String>,
    body_bytes: usize,
    body_shape: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
struct FixedScriptEventResponse {
    status: Option<u16>,
    header_names: Vec<String>,
    event_names: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ProofRun {
    pub(super) name: String,
    pub(super) provider_kind: String,
    pub(super) endpoint: String,
    pub(super) outcome: String,
    pub(super) observed: serde_json::Value,
    pub(super) transcript: FixedScriptTranscript,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct FixedScriptTranscript {
    pub(super) schema: &'static str,
    pub(super) proof: String,
    pub(super) provider_kind: String,
    pub(super) endpoint: TranscriptEndpoint,
    pub(super) timeline_at_semantics: &'static str,
    pub(super) request_match: TranscriptRequestMatch,
    pub(super) http_exchanges: Vec<ScriptedLlmHttpExchange>,
    pub(super) response_events: Vec<TranscriptResponseEvent>,
    pub(super) terminal: TranscriptTerminal,
    pub(super) observed: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptEndpoint {
    pub(super) method: String,
    pub(super) path: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptRequestMatch {
    pub(super) body_paths: Vec<String>,
    pub(super) headers: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptResponseEvent {
    pub(super) at: u64,
    pub(super) event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) headers: Vec<TranscriptHeader>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptHeader {
    pub(super) name: String,
    pub(super) value: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptTerminal {
    pub(super) classification: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_result: Option<TranscriptProviderResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error_envelope: Option<TranscriptErrorEnvelope>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptProviderResult {
    pub(super) terminal_reason: String,
    pub(super) full_text_bytes: usize,
    pub(super) part_count: usize,
    pub(super) usage: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TranscriptErrorEnvelope {
    pub(super) kind: String,
    pub(super) code: Option<String>,
    pub(super) status: Option<u16>,
    pub(super) retryable: bool,
    pub(super) terminal_reason: String,
    pub(super) raw_body_bytes: Option<usize>,
    pub(super) headers: Vec<TranscriptHeader>,
    pub(super) retry_after_ms: Option<u64>,
    pub(super) request_body_snapshot: bool,
}

pub async fn run_fixed_script_profile(
    artifact_root: impl AsRef<Path>,
) -> Result<FixedScriptManifest, FixedScriptRunnerError> {
    lash_core::panic_containment::set_loud(true);
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root)?;

    let scripts = script_hash_manifest()?;
    let script_bundle_hash = script_bundle_hash(&scripts);
    let proof_runs = vec![
        prove_openai_compatible_tool_stream().await?,
        prove_openai_responses_text_stream().await?,
        prove_codex_responses_text_stream().await?,
        prove_codex_responses_tool_call_stream().await?,
        prove_codex_responses_rate_limit().await?,
        prove_codex_responses_disconnect().await?,
        prove_anthropic_messages_text_stream().await?,
        prove_openai_compatible_rate_limit().await?,
        prove_openai_compatible_validation().await?,
        prove_openai_compatible_disconnect().await?,
        prove_openai_compatible_response_start_timeout().await?,
        prove_openai_compatible_stream_chunk_timeout().await?,
        prove_openai_compatible_cancel_before_response_start().await?,
        prove_openai_compatible_retry_exhaustion().await?,
        prove_google_stream_generate_text().await?,
        prove_google_generate_text().await?,
    ];
    let provider_matrix = provider_matrix(&scripts, &proof_runs);
    let fixed_events = fixed_script_events(&proof_runs);
    let events_sha256 = write_events_artifact(artifact_root, &fixed_events)?;
    let proofs = write_proof_artifacts(artifact_root, proof_runs)?;
    let summary = FixedScriptSummary {
        total_scripts: scripts.len(),
        total_proofs: proofs.len(),
        total_events: fixed_events.len(),
        passed: proofs.len(),
    };
    let manifest_path = artifact_root.join(FIXED_SCRIPT_MANIFEST);
    let summary_path = artifact_root.join(FIXED_SCRIPT_SUMMARY);
    let manifest = FixedScriptManifest {
        schema: "lash.sim.fixed-script-manifest.v1",
        profile: FIXED_SCRIPT_PROFILE,
        timeline_at_semantics: FIXED_SCRIPT_TIMELINE_AT_SEMANTICS,
        script_bundle_hash,
        events_path: FIXED_SCRIPT_EVENTS,
        events_sha256,
        event_count: fixed_events.len(),
        scripts,
        provider_matrix,
        provider_transport_exclusions: provider_transport_exclusions(),
        proofs,
        summary,
        manifest_path: manifest_path.clone(),
        summary_path: summary_path.clone(),
    };
    let lane_summary = lane_summary_for_manifest(&manifest);
    let body = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(&manifest_path, body)?;
    let body = serde_json::to_vec_pretty(&lane_summary)?;
    std::fs::write(&summary_path, body)?;
    Ok(manifest)
}

fn lane_summary_for_manifest(manifest: &FixedScriptManifest) -> FixedScriptLaneSummary {
    let mut provider_set = manifest
        .scripts
        .iter()
        .map(|script| script.provider_kind.clone())
        .collect::<Vec<_>>();
    provider_set.sort();
    provider_set.dedup();

    FixedScriptLaneSummary {
        schema: "lash.sim.summary.v1",
        profile: FIXED_SCRIPT_PROFILE,
        generator_version: "fixed-provider-scripts.v1",
        replay_schema_version: manifest.schema,
        script_bundle_hash: manifest.script_bundle_hash.clone(),
        provider_set,
        backend_set: Vec::new(),
        counts: FixedScriptLaneCounts {
            generated_seeds: 0,
            fixed_script_events: manifest.event_count,
            fixed_replays: manifest.summary.total_proofs,
            minimized_replays: 0,
            backend_replays: 0,
            boundary_events: 0,
            oracle_passes: manifest.summary.passed,
            oracle_failures: manifest.summary.total_proofs - manifest.summary.passed,
            divergences: 0,
        },
        fixed_script_manifest: FIXED_SCRIPT_MANIFEST,
        fixed_script_events: FIXED_SCRIPT_EVENTS,
    }
}

pub(super) fn script_hash_manifest() -> Result<Vec<ScriptHashManifest>, FixedScriptRunnerError> {
    CANONICAL_SCRIPTS
        .iter()
        .map(|entry| {
            let script = ProviderWireScript::from_json_str(entry.content)?;
            Ok(ScriptHashManifest {
                path: entry.path.to_string(),
                name: script.name,
                provider_kind: script.provider_kind,
                sha256: sha256_hex(entry.content.as_bytes()),
                bytes: entry.content.len(),
            })
        })
        .collect()
}

pub(super) fn script_bundle_hash(scripts: &[ScriptHashManifest]) -> String {
    let mut hasher = Sha256::new();
    for script in scripts {
        hasher.update(script.path.as_bytes());
        hasher.update([0]);
        hasher.update(script.sha256.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    hex_digest(&digest)
}

fn write_proof_artifacts(
    artifact_root: &Path,
    proof_runs: Vec<ProofRun>,
) -> Result<Vec<FixedScriptProof>, FixedScriptRunnerError> {
    let proof_dir = artifact_root.join("proofs");
    std::fs::create_dir_all(&proof_dir)?;
    proof_runs
        .into_iter()
        .map(|run| {
            let relative_path =
                PathBuf::from("proofs").join(format!("{}.json", proof_file_stem(&run.name)));
            let body = serde_json::to_vec_pretty(&run.transcript)?;
            std::fs::write(artifact_root.join(&relative_path), &body)?;
            Ok(FixedScriptProof {
                name: run.name,
                provider_kind: run.provider_kind,
                endpoint: run.endpoint,
                outcome: run.outcome,
                transcript_path: relative_path
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                transcript_sha256: sha256_hex(&body),
                observed: run.observed,
            })
        })
        .collect()
}

fn fixed_script_events(proof_runs: &[ProofRun]) -> Vec<FixedScriptEvent> {
    let mut sequence = 0;
    let mut events = Vec::new();
    for (proof_index, run) in proof_runs.iter().enumerate() {
        let proof_alias = format!("proof-{proof_index:03}");
        let transcript_path = PathBuf::from("proofs")
            .join(format!("{}.json", proof_file_stem(&run.name)))
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        for (exchange_index, exchange) in run.transcript.http_exchanges.iter().enumerate() {
            events.push(FixedScriptEvent {
                schema: "lash.sim.fixed-script-event.v1",
                sequence,
                event: "fixed_script_exchange",
                profile: FIXED_SCRIPT_PROFILE,
                proof_alias: proof_alias.clone(),
                exchange_alias: format!("{proof_alias}.exchange-{exchange_index:03}"),
                proof_name: run.name.clone(),
                provider_kind: run.provider_kind.clone(),
                outcome: run.outcome.clone(),
                transcript_path: transcript_path.clone(),
                request: FixedScriptEventRequest {
                    method: exchange.request.method.clone(),
                    path: exchange.request.path.clone(),
                    header_names: exchange
                        .request
                        .headers
                        .iter()
                        .map(|header| header.name.clone())
                        .collect(),
                    body_bytes: exchange.request.body_bytes,
                    body_shape: exchange.request.body_shape.clone(),
                },
                response: FixedScriptEventResponse {
                    status: exchange.response.status,
                    header_names: exchange
                        .response
                        .headers
                        .iter()
                        .map(|header| header.name.clone())
                        .collect(),
                    event_names: exchange.response.event_names.clone(),
                },
                terminal_classification: run.transcript.terminal.classification,
            });
            sequence += 1;
        }
    }
    events
}

fn write_events_artifact(
    artifact_root: &Path,
    events: &[FixedScriptEvent],
) -> Result<String, FixedScriptRunnerError> {
    let mut body = Vec::new();
    for event in events {
        body.extend_from_slice(&serde_json::to_vec(event)?);
        body.push(b'\n');
    }
    std::fs::write(artifact_root.join(FIXED_SCRIPT_EVENTS), &body)?;
    Ok(sha256_hex(&body))
}

fn proof_file_stem(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn transcript_for_script(
    proof_name: &str,
    provider_kind: &str,
    script_content: &str,
    http_exchanges: Vec<ScriptedLlmHttpExchange>,
    terminal: TranscriptTerminal,
    observed: serde_json::Value,
) -> Result<FixedScriptTranscript, FixedScriptRunnerError> {
    let script = ProviderWireScript::from_json_str(script_content)?;
    let body_paths = script.request_match.body.keys().cloned().collect();
    let headers = script.request_match.headers.keys().cloned().collect();
    let response_events = script.timeline.iter().map(transcript_event).collect();
    Ok(FixedScriptTranscript {
        schema: "lash.sim.fixed-script-proof-transcript.v1",
        proof: proof_name.to_string(),
        provider_kind: provider_kind.to_string(),
        endpoint: TranscriptEndpoint {
            method: script.endpoint.method,
            path: script.endpoint.path,
        },
        timeline_at_semantics: FIXED_SCRIPT_TIMELINE_AT_SEMANTICS,
        request_match: TranscriptRequestMatch {
            body_paths,
            headers,
        },
        http_exchanges,
        response_events,
        terminal,
        observed,
    })
}

fn transcript_event(event: &ProviderWireEvent) -> TranscriptResponseEvent {
    match event {
        ProviderWireEvent::ResponseStart {
            at,
            status,
            headers,
        } => TranscriptResponseEvent {
            at: *at,
            event: event.event_name(),
            provider_event: None,
            status: Some(*status),
            headers: transcript_headers(headers),
        },
        ProviderWireEvent::HttpError {
            at,
            status,
            headers,
            ..
        } => TranscriptResponseEvent {
            at: *at,
            event: event.event_name(),
            provider_event: None,
            status: Some(*status),
            headers: transcript_headers(headers),
        },
        ProviderWireEvent::Sse { at, data } => TranscriptResponseEvent {
            at: *at,
            event: event.event_name(),
            provider_event: provider_event_name(data),
            status: None,
            headers: Vec::new(),
        },
        ProviderWireEvent::Body { at, .. }
        | ProviderWireEvent::Chunk { at, .. }
        | ProviderWireEvent::End { at }
        | ProviderWireEvent::Disconnect { at, .. }
        | ProviderWireEvent::Timeout { at, .. }
        | ProviderWireEvent::TransportError { at, .. } => TranscriptResponseEvent {
            at: *at,
            event: event.event_name(),
            provider_event: None,
            status: None,
            headers: Vec::new(),
        },
    }
}

fn transcript_headers(headers: &[ProviderWireHeader]) -> Vec<TranscriptHeader> {
    headers
        .iter()
        .map(|header| TranscriptHeader {
            name: header.name.clone(),
            value: redacted_header_value(&header.name, &header.value),
        })
        .collect()
}

fn provider_event_name(data: &str) -> Option<String> {
    let trimmed = data.trim();
    if trimmed == "[DONE]" {
        return Some("[DONE]".to_string());
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    value
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
}
