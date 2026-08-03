use super::*;
use lash_llm_transport::{LlmHttpRequest, LlmHttpResponse};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat};

#[derive(Debug)]
struct CapturingLlmHttpTransport {
    inner: ScriptedLlmHttpTransport,
    bodies: Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl LlmHttpTransport for CapturingLlmHttpTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.bodies
            .lock()
            .expect("capture lock")
            .push(request.body.to_vec());
        self.inner.send(request, timeout).await
    }
}

fn stable_prompt_prefix_bytes(body: &Value, message_count: usize) -> Vec<u8> {
    // `cache_control` is the provider's deliberately moving breakpoint
    // directive, not prompt content. Remove only that annotation before
    // comparing the serialized prompt prefix; roles, part shape, and text
    // remain byte-for-byte significant.
    let mut prefix = body["messages"].as_array().expect("messages array")[..message_count].to_vec();
    for message in &mut prefix {
        if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                part.as_object_mut()
                    .map(|object| object.remove("cache_control"));
            }
        }
    }
    serde_json::to_vec(&prefix).expect("serialize stable prompt prefix")
}

fn wire_message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        content => panic!("unexpected message content: {content}"),
    }
}

#[tokio::test]
async fn cache_dialect_rlm_prompt_prefix_is_byte_stable_across_iterations() {
    for (model, cache_control) in [
        (
            "anthropic/claude-sonnet-4.6",
            lash_core::CacheControlDialect::Anthropic,
        ),
        (
            "google/gemini-3.1-pro-preview",
            lash_core::CacheControlDialect::Gemini,
        ),
    ] {
        let responses = vec![
            "<lashlang>\nscratch_note = \"saved\"\nprint scratch_note\n</lashlang>".to_string(),
            "RLM work is complete.".to_string(),
        ];
        let mut scripts = runtime_provider_scripts_for_texts(OPENAI_COMPATIBLE, &responses)
            .expect("OpenRouter RLM scripts");
        for script in &mut scripts {
            script.endpoint.path = "/api/v1/chat/completions".to_string();
            script
                .request_match
                .body
                .get_mut("model")
                .expect("model request matcher")
                .equals = Some(json!(model));
        }
        let capture = Arc::new(CapturingLlmHttpTransport {
            inner: ScriptedLlmHttpTransport::from_scripts(scripts),
            bodies: Mutex::new(Vec::new()),
        });
        let provider = OpenAiCompatibleProvider::new("test-key", OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_transport(capture.clone());
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::default(),
            Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
        );
        let core = lash::LashCore::rlm_builder(factory)
            .effect_host(Arc::new(
                lash::durability::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .store_factory(Arc::new(
                lash::persistence::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
                as Arc<dyn lash_core::ProcessRegistry>)
            .provider(ProviderHandle::new(provider.into_components()))
            .model(
                lash_core::ModelSpec::from_token_limits(model, Default::default(), 200_000, None)
                    .expect("model limits")
                    .with_capability(lash_core::ModelCapability {
                        cache_control: Some(cache_control),
                        ..lash_core::ModelCapability::default()
                    }),
            )
            .build()
            .expect("RLM prefix-stability core");
        let session = core
            .session(format!("prefix-stability-{}", model.replace('/', "-")))
            .open()
            .await
            .expect("RLM prefix-stability session");

        let output = session
            .turn(lash::TurnInput::text("inspect"))
            .run()
            .await
            .expect("RLM prefix-stability turn");
        assert!(
            output.is_success(),
            "RLM turn failed for {model}: {output:?}"
        );

        let bodies = capture.bodies.lock().expect("capture lock");
        assert_eq!(bodies.len(), 2, "expected two RLM iterations for {model}");
        let previous_body: Value = serde_json::from_slice(&bodies[0]).expect("first request JSON");
        let next_body: Value = serde_json::from_slice(&bodies[1]).expect("second request JSON");
        let previous_messages = previous_body["messages"].as_array().expect("messages");
        let next_messages = next_body["messages"].as_array().expect("messages");
        let stable_message_count = previous_messages.len() - 1;

        assert_eq!(
            stable_prompt_prefix_bytes(&previous_body, stable_message_count),
            stable_prompt_prefix_bytes(&next_body, stable_message_count),
            "RLM prompt prefix changed for {model}"
        );
        assert!(
            next_messages
                .iter()
                .take(stable_message_count)
                .all(|message| message["content"].is_array()),
            "stable prefix text shape changed for {model}"
        );
        assert_eq!(
            wire_message_text(&previous_messages[0]),
            wire_message_text(&next_messages[0]),
            "RLM system prompt changed for {model}"
        );
        assert!(
            wire_message_text(previous_messages.last().expect("first volatile tail"))
                .contains("=== CURRENT ITERATION: 1 ==="),
            "first volatile suffix was not last for {model}"
        );
        let next_tail = wire_message_text(next_messages.last().expect("second volatile tail"));
        assert!(
            next_tail.contains("=== CURRENT ITERATION: 2 ===")
                && next_tail.contains("- `scratch_note` = saved"),
            "updated volatile suffix was not last for {model}: {next_tail}"
        );
        assert!(
            next_messages
                .iter()
                .take(stable_message_count)
                .all(|message| !wire_message_text(message).contains("scratch_note")),
            "volatile bound variables leaked into the stable prefix for {model}"
        );
    }
}

#[tokio::test]
async fn attachment_owner_sweep_is_deterministic_across_memory_and_sqlite() {
    lash_core::testing::conformance::attachment_ownership_isolation(std::sync::Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    lash_core::testing::conformance::attachment_ownership_isolation_with_store(
        std::sync::Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            tmp.path().join("sessions"),
        )),
        std::sync::Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            tmp.path().join("attachments"),
        )),
    )
    .await;
}

#[tokio::test]
async fn divergent_seed_cross_backend_durable_state_agrees() {
    // Regression guard for full-random seed 14123330213291275571, whose durable
    // cross-backend re-run previously hung (a `next_turn` queued ingress ran an
    // unmodeled inline turn under serialized execution) and then diverged (a
    // slow async store let later boundaries overtake a live turn's completion,
    // drifting the seeded delivery order). The serialized in-memory reference
    // and the SQLite durable re-run share the serialize-provider-turn discipline
    // and differ only in the store, so their abstract durable-state summaries
    // must be byte-identical.
    let seed = 14_123_330_213_291_275_571u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let reference = replay_workload_serialized_reference(&workload)
        .await
        .expect("serialized in-memory reference");
    let tmp = tempfile::tempdir().expect("tempdir");
    let sqlite_summary = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("sqlite re-run");
    assert!(
        replay_determinism(&reference, &sqlite_summary).is_passed(),
        "cross-backend semantic durable state diverged for seed {seed}: reference={reference:#?} sqlite={sqlite_summary:#?}"
    );
    println!(
        "OK seed={seed} sessions={} digest={}",
        reference.session_count, reference.digest
    );
}

#[tokio::test]
async fn absolute_fence_drift_seed_cross_backend_semantics_agree() {
    // Regression for weekly-full seed 14526660659617982248. SQLite consumed one
    // fewer opaque fencing token for two workers while ownership transitions,
    // stale-writer rejection, and all user-visible durable state matched.
    let seed = 14_526_660_659_617_982_248u64;
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let reference = replay_workload_serialized_reference(&workload)
        .await
        .expect("serialized in-memory reference");
    let tmp = tempfile::tempdir().expect("tempdir");
    let sqlite_summary = replay_workload_on_sqlite(&workload, &tmp.path().join("sqlite-store"))
        .await
        .expect("sqlite re-run");
    assert!(
        replay_determinism(&reference, &sqlite_summary).is_passed(),
        "cross-backend semantic durable state diverged for seed {seed}: reference={reference:#?} sqlite={sqlite_summary:#?}"
    );
}

#[test]
fn full_random_seed_12_keeps_modeled_provider_exchange_slots_owned_by_scheduler() {
    let seed = generated_seed("full-random", 12);
    assert_eq!(seed, 8_740_143_186_674_533_974);
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let provider_turn = workload
        .boundaries
        .iter()
        .find(|event| event.boundary_id == "session-001:provider:003")
        .expect("session-001 provider turn 3");
    assert_eq!(
        provider_turn.payload.get("script").and_then(Value::as_str),
        Some("openai-compatible.chat-runtime-text-stream")
    );
    assert_eq!(
        provider_turn
            .payload
            .get("expected_provider_exchange_count")
            .and_then(Value::as_u64),
        Some(3)
    );

    let trace = run_on_sim_harness_stack(
        "full-random-seed-12-modeled-provider-exchange-slots",
        SIM_HARNESS_STACK_LIMIT_BYTES,
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            runtime.block_on(run_generated_workload_for_fixture(
                workload,
                "seed-12-regression",
            ))
        },
    )
    .expect("seed 12 generated workload");
    let delivered = trace
        .events
        .iter()
        .find(|event| event.boundary_id == "session-001:provider:003")
        .expect("delivered provider turn 3");
    assert_eq!(
        delivered.observed.get("success").and_then(Value::as_bool),
        Some(true),
        "success-required modeled provider turn must not be converted into provider-error terminalization"
    );
    assert_eq!(
        delivered
            .observed
            .get("provider_exchange_count")
            .and_then(Value::as_u64),
        Some(3),
        "autonomous queued turns must not consume provider scripts before modeled turn 3"
    );
}

#[tokio::test]
async fn runtime_completion_serialization_mutation_guard() {
    let seed = generated_seed("full-random", 12);
    let workload = generate_workload(seed, "full-random", 384).expect("workload");
    let clock = SimClock::new();
    let mut world = GeneratedRuntimeWorld::with_backend(
        Arc::new(lash::persistence::InMemorySessionStoreFactory::with_clock(
            clock.clone(),
        )),
        RuntimeEffectReplayStore::Memory,
        Arc::new(lash::persistence::InMemoryAttachmentStore::new()),
        Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::new()),
        true,
        clock,
    );

    let (events, _summary) = drive_generated_workload(&mut world, &workload)
        .await
        .expect("serialized generated workload");
    let provider_completions = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .count();

    assert!(
        provider_completions > 1,
        "serialization guard must exercise more than one provider turn"
    );
    assert_eq!(
        peak_concurrent_live_turns(&events),
        1,
        "delivered evidence must not show overlapping provider turns under serialized replay"
    );
}

#[tokio::test]
async fn generated_park_resume_transcript_is_readable_and_logical_size_labeled() {
    let workload = generate_workload(1, "fast", 72).expect("workload");
    let trace = run_generated_workload_for_fixture(workload, "park-resume-transcript")
        .await
        .expect("generated workload");
    assert!(
        trace
            .durable_writes
            .iter()
            .any(CheckpointWriteEvent::has_unchanged_ref),
        "generated multi-turn sessions must naturally produce a ref-only checkpoint component: {:#?}",
        trace.durable_writes
    );
    assert!(
        trace
            .durable_writes
            .iter()
            .any(|write| write.session_id.starts_with("suspend-")),
        "generated park/resume must commit durable state after resolution: {:#?}",
        trace.durable_writes
    );
    let contract_writes = trace
        .durable_writes
        .iter()
        .filter(|write| write.cause_boundary_id.is_some())
        .collect::<Vec<_>>();
    assert!(
        !contract_writes.is_empty(),
        "real Agent façade contract executions must emit checkpoint observations"
    );
    assert!(contract_writes.iter().all(|write| {
        trace.events.iter().any(|event| {
            Some(event.boundary_id.as_str()) == write.cause_boundary_id.as_deref()
                && event.kind == BoundaryKind::Trigger
                && event.actor_alias == write.attributed_session()
        })
    }));
    assert!(
        contract_writes
            .iter()
            .flat_map(|write| &write.components)
            .all(|component| {
                !matches!(
                    component.kind,
                    CheckpointComponentWriteKind::Stored {
                        logical_bytes: Some(_)
                    }
                )
            })
    );
    let transcript = trace.render_session_transcript("suspend-tool");
    for seed in [2, 3] {
        let workload = generate_workload(seed, "fast", 72).expect("workload");
        let varied = run_generated_workload_for_fixture(workload, "park-resume-transcript")
            .await
            .expect("generated seed variation");
        assert_eq!(
            varied.render_session_transcript("suspend-tool"),
            transcript,
            "park/resume transcript changed for seed {seed}"
        );
    }
    insta::assert_snapshot!(transcript, @r#"
    suspend-tool  ingress   session.open.suspend    turn=1
    suspend-tool  park      session.park
    suspend-tool  resume    session.resume
    suspend-tool  tool      suspend.tool.resume     name="await_tool"
    suspend-tool  commit    checkpoint.commit       rev=0->1
    suspend-tool              turn_state            stored logical=210B
    suspend-tool              tool_state            stored logical=2.2KB
    suspend-tool              plugin_snapshot       stored logical=443B
    "#);
}

#[test]
fn standard_protocol_full_text_mutation_guard() {
    let result = run_standard_protocol_contract(
        "standard.full_text_mutation_guard",
        "answer with two chunks",
        None,
        vec![
            StandardContractStep::Llm {
                text_streamed: true,
                parts: vec![
                    standard_text_part("first chunk"),
                    standard_text_part("second chunk"),
                ],
            },
            StandardContractStep::Checkpoint,
        ],
    )
    .expect("standard full-text contract");

    assert_eq!(
        result
            .get("llm_response_full_texts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        vec![json!("first chunk\nsecond chunk")],
        "fixed Standard execution must preserve LlmResponse.full_text, not only streamed parts"
    );
    assert_eq!(
        result
            .get("llm_response_parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        vec![json!([
            {"kind": "text", "text": "first chunk"},
            {"kind": "text", "text": "second chunk"},
        ])],
        "fixed Standard execution must preserve concrete response parts"
    );
    assert_eq!(
        result.get("llm_call_count").and_then(Value::as_u64),
        Some(1),
        "the guard must drive a real Standard LLM turn"
    );
    assert_eq!(
        result.get("done").and_then(Value::as_bool),
        Some(true),
        "the guarded Standard turn must still complete"
    );
}

#[test]
fn rlm_protocol_response_shape_mutation_guard() {
    let result = run_rlm_protocol_contract(
        "rlm.response_shape_mutation_guard",
        "answer naturally",
        RlmTermination::Natural,
        None,
        None,
        vec![
            RlmContractStep::Llm(vec![rlm_text_part("RLM final prose")]),
            RlmContractStep::Checkpoint,
        ],
    )
    .expect("rlm response-shape contract");

    assert_eq!(
        result
            .get("llm_response_full_texts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        vec![json!("RLM final prose")],
        "fixed RLM execution must preserve LlmResponse.full_text"
    );
    assert_eq!(
        result
            .get("llm_response_part_counts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        vec![json!(1)],
        "fixed RLM execution must preserve concrete response parts"
    );
    assert_eq!(
        result
            .get("llm_response_parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        vec![json!([{"kind": "text", "text": "RLM final prose"}])],
        "fixed RLM execution must preserve the concrete text part"
    );
    assert_eq!(
        result.get("done").and_then(Value::as_bool),
        Some(true),
        "the guarded RLM turn must still complete"
    );
}

#[tokio::test]
async fn fixed_texts_provider_response_shape_mutation_guard() {
    let mut provider =
        fixed_texts_provider("lash-sim-fixed-text-guard", vec!["facade response text"]);
    let response = provider
        .complete(openai_compatible_request(false))
        .await
        .expect("fixed text provider response");

    assert_eq!(response.full_text, "facade response text");
    assert!(
        matches!(
            response.parts.as_slice(),
            [LlmOutputPart::Text { text, .. }] if text == "facade response text"
        ),
        "fixed text provider must return a matching text part"
    );
}

#[tokio::test]
async fn rlm_final_value_provider_response_shape_mutation_guard() {
    let mut provider = rlm_final_value_provider();
    let response = provider
        .complete(openai_compatible_request(true))
        .await
        .expect("rlm final-value provider response");

    assert!(response.full_text.contains("semantic-channel"));
    assert!(
        response_text_part(&response).is_some_and(|text| text.contains("semantic-channel")),
        "rlm final-value provider must return the semantic text part"
    );
}

#[tokio::test]
async fn pending_tool_roundtrip_provider_response_shape_mutation_guard() {
    let mut provider = pending_tool_roundtrip_provider();
    let tool_response = provider
        .complete(openai_compatible_request(false))
        .await
        .expect("pending tool provider tool-call response");
    assert!(
        matches!(
            tool_response.parts.as_slice(),
            [LlmOutputPart::ToolCall { call_id, tool_name, input_json, .. }]
                if call_id == "call-1" && tool_name == "app_lookup" && input_json == "{}"
        ),
        "pending tool provider must start with the concrete tool-call part"
    );

    let final_response = provider
        .complete(openai_compatible_request(false))
        .await
        .expect("pending tool provider final response");
    assert_eq!(final_response.full_text, "done");
    assert_eq!(response_text_part(&final_response), Some("done"));
}

#[tokio::test]
async fn fixed_script_profile_writes_deterministic_manifest() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let manifest = Box::pin(run_fixed_script_profile(tmp.path()))
        .await
        .expect("profile");

    assert_eq!(manifest.profile, FIXED_SCRIPT_PROFILE);
    assert_eq!(
        manifest.timeline_at_semantics,
        FIXED_SCRIPT_TIMELINE_AT_SEMANTICS
    );
    assert_eq!(manifest.summary.total_scripts, 14);
    assert_eq!(manifest.summary.total_proofs, 16);
    assert_eq!(manifest.summary.total_events, 17);
    assert_eq!(manifest.summary.passed, 16);
    // Codex HTTP/SSE execution rides the injectable LlmHttpTransport and is
    // in the scripted matrix; the exclusion that remains for codex.rs is
    // scoped to the provider-native websocket transport, and the OAuth
    // device-code auth flow stays out of the LLM DST.
    assert!(
        manifest
            .provider_transport_exclusions
            .iter()
            .any(|exclusion| exclusion.path.contains("codex/oauth.rs"))
    );
    assert!(
        manifest
            .provider_transport_exclusions
            .iter()
            .any(
                |exclusion| exclusion.path == "crates/lash-provider-openai/src/codex.rs"
                    && exclusion.replacement_lane.contains("websocket")
            )
    );
    assert!(manifest.manifest_path.ends_with(FIXED_SCRIPT_MANIFEST));
    assert!(manifest.summary_path.ends_with(FIXED_SCRIPT_SUMMARY));

    let body = std::fs::read_to_string(tmp.path().join(FIXED_SCRIPT_MANIFEST)).expect("manifest");
    assert!(body.contains("script_bundle_hash"));
    assert!(body.contains("anthropic.messages-text-stream"));
    assert!(body.contains("openai.responses-text-stream"));
    assert!(body.contains("openai-compatible.chat-response-start-timeout"));
    assert!(body.contains("openai-compatible.chat-stream-chunk-timeout"));
    assert!(body.contains("openai-compatible.cancel-before-response-start"));
    assert!(body.contains("openai-compatible.retry-exhaustion"));
    assert!(body.contains("google.stream-generate-content-text-stream"));
    assert!(body.contains("google.generate-content-text"));
    assert!(body.contains("codex.responses-text-stream"));
    assert!(body.contains("codex.responses-tool-call-stream"));
    assert!(body.contains("codex.responses-rate-limit-429"));
    assert!(body.contains("codex.responses-mid-stream-disconnect"));

    let summary_body =
        std::fs::read_to_string(tmp.path().join(FIXED_SCRIPT_SUMMARY)).expect("summary");
    let summary: serde_json::Value = serde_json::from_str(&summary_body).expect("summary JSON");
    assert_eq!(summary["schema"], "lash.sim.summary.v1");
    assert_eq!(summary["profile"], FIXED_SCRIPT_PROFILE);
    assert_eq!(summary["fixed_script_manifest"], FIXED_SCRIPT_MANIFEST);
    assert_eq!(summary["counts"]["generated_seeds"], 0);
    assert_eq!(summary["counts"]["fixed_replays"], 16);
    assert_eq!(summary["counts"]["oracle_passes"], 16);
    assert_eq!(
        summary["provider_set"],
        json!([
            "anthropic",
            "codex",
            "google_oauth",
            "openai",
            "openai-compatible"
        ])
    );
}

#[tokio::test]
async fn fixed_script_manifest_schema_contains_required_proofs_and_artifact_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Box::pin(run_fixed_script_profile(tmp.path()))
        .await
        .expect("profile");

    let body = std::fs::read_to_string(tmp.path().join(FIXED_SCRIPT_MANIFEST)).expect("manifest");
    let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
    assert_eq!(manifest["schema"], "lash.sim.fixed-script-manifest.v1");
    assert_eq!(manifest["profile"], FIXED_SCRIPT_PROFILE);
    assert_eq!(
        manifest["timeline_at_semantics"],
        FIXED_SCRIPT_TIMELINE_AT_SEMANTICS
    );
    assert_eq!(
        manifest["summary"],
        json!({
            "total_scripts": 14,
            "total_proofs": 16,
            "total_events": 17,
            "passed": 16
        })
    );
    assert_eq!(
        manifest["script_bundle_hash"]
            .as_str()
            .expect("script bundle hash")
            .len(),
        64
    );

    let proofs = manifest["proofs"].as_array().expect("proofs array");
    let required = [
        "openai-compatible.chat-tool-call-split-stream",
        "openai.responses-text-stream",
        "codex.responses-text-stream",
        "codex.responses-tool-call-stream",
        "codex.responses-rate-limit-429",
        "codex.responses-mid-stream-disconnect",
        "anthropic.messages-text-stream",
        "openai-compatible.chat-rate-limit-429",
        "openai-compatible.chat-validation-error",
        "openai-compatible.chat-mid-stream-disconnect",
        "openai-compatible.chat-response-start-timeout",
        "openai-compatible.chat-stream-chunk-timeout",
        "openai-compatible.cancel-before-response-start",
        "openai-compatible.retry-exhaustion",
        "google.stream-generate-content-text-stream",
        "google.generate-content-text",
    ];
    for name in required {
        let proof = proofs
            .iter()
            .find(|proof| proof["name"] == name)
            .unwrap_or_else(|| panic!("missing proof {name}"));
        assert_eq!(proof["outcome"], "passed");
        assert!(
            proof["endpoint"]
                .as_str()
                .expect("endpoint")
                .starts_with('/')
        );
        assert_eq!(
            proof["transcript_sha256"]
                .as_str()
                .expect("transcript hash")
                .len(),
            64
        );
        let transcript_path = proof["transcript_path"].as_str().expect("transcript path");
        assert!(transcript_path.starts_with("proofs/"));

        let transcript_body =
            std::fs::read_to_string(tmp.path().join(transcript_path)).expect("transcript");
        let transcript: serde_json::Value =
            serde_json::from_str(&transcript_body).expect("transcript JSON");
        assert_eq!(
            transcript["schema"],
            "lash.sim.fixed-script-proof-transcript.v1"
        );
        assert_eq!(transcript["proof"], name);
        assert_eq!(
            transcript["timeline_at_semantics"],
            FIXED_SCRIPT_TIMELINE_AT_SEMANTICS
        );
        assert!(
            transcript["request_match"]["body_paths"]
                .as_array()
                .expect("body paths")
                .iter()
                .all(|path| path.as_str().is_some())
        );
        assert!(
            !transcript["response_events"]
                .as_array()
                .expect("response events")
                .is_empty()
        );
        let exchanges = transcript["http_exchanges"]
            .as_array()
            .expect("http exchanges");
        assert!(
            !exchanges.is_empty(),
            "transcript {name} should contain a sanitized HTTP exchange"
        );
        for exchange in exchanges {
            assert_eq!(exchange["request"]["method"], "POST");
            assert!(
                exchange["request"]["path"]
                    .as_str()
                    .expect("request path")
                    .starts_with('/')
            );
            assert!(
                exchange["request"]["body_bytes"]
                    .as_u64()
                    .expect("body bytes")
                    > 0
            );
            assert_eq!(exchange["request"]["body_shape"]["type"], "object");
            let request_headers = exchange["request"]["headers"]
                .as_array()
                .expect("request headers");
            let auth_header = request_headers
                .iter()
                .find(|header| {
                    header["name"].as_str().is_some_and(|name| {
                        name.eq_ignore_ascii_case("authorization")
                            || name.eq_ignore_ascii_case("x-api-key")
                    })
                })
                .expect("provider auth header");
            assert_eq!(auth_header["value"], "[redacted]");
            assert!(
                exchange["response"]["status"].is_null() || exchange["response"]["status"].is_u64()
            );
            assert!(exchange["response"]["headers"].is_array());
            assert!(exchange["response"]["event_names"].is_array());
        }
        let terminal = &transcript["terminal"];
        match terminal["classification"].as_str().expect("terminal class") {
            "success" => {
                assert!(terminal["provider_result"]["terminal_reason"].is_string());
                assert!(terminal["provider_result"]["full_text_bytes"].is_u64());
                assert!(terminal["provider_result"]["part_count"].is_u64());
            }
            "error" => {
                let envelope = &terminal["error_envelope"];
                assert!(envelope["kind"].is_string());
                assert!(envelope["retryable"].is_boolean());
                assert!(envelope["terminal_reason"].is_string());
                assert!(envelope["headers"].is_array());
            }
            "cancelled_before_response_start" => {}
            other => panic!("unexpected terminal classification {other}"),
        }
        if name.ends_with("timeout") {
            let envelope = &terminal["error_envelope"];
            assert_eq!(envelope["kind"], "Timeout");
            assert_eq!(envelope["code"], "timeout");
            assert_eq!(envelope["retryable"], true);
        }
        if name == "openai-compatible.chat-stream-chunk-timeout" {
            assert_eq!(transcript["observed"]["stream_events_committed"], 0);
            assert_eq!(
                transcript["observed"]["reported_successful_partial_response"],
                false
            );
        }
        assert!(!transcript_body.contains("test-key"));
        assert!(!transcript_body.contains("Bearer"));
        assert!(!transcript_body.contains("lookup x"));
        assert!(!transcript_body.contains("answer directly"));
        assert!(
            transcript["observed"]["classification"].is_string()
                || transcript["observed"]["classification"].is_object()
        );
    }
    let matrix = manifest["provider_matrix"]
        .as_array()
        .expect("provider matrix");
    let google = matrix
        .iter()
        .find(|row| row["provider_kind"] == "google_oauth")
        .expect("google provider matrix row");
    assert_eq!(google["success_proofs"], 2);
    assert_eq!(google["error_proofs"], 0);
    assert_eq!(
        google["endpoints"],
        json!([
            "/v1internal:generateContent",
            "/v1internal:streamGenerateContent"
        ])
    );
    let codex = matrix
        .iter()
        .find(|row| row["provider_kind"] == "codex")
        .expect("codex provider matrix row");
    assert_eq!(codex["success_proofs"], 2);
    assert_eq!(codex["error_proofs"], 2);
    assert_eq!(codex["endpoints"], json!(["/backend-api/codex/responses"]));
}

#[tokio::test]
async fn fixed_script_timeout_proofs_preserve_timeout_envelopes() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Box::pin(run_fixed_script_profile(tmp.path()))
        .await
        .expect("profile");

    for name in [
        "openai-compatible.chat-response-start-timeout",
        "openai-compatible.chat-stream-chunk-timeout",
    ] {
        let transcript_path = tmp.path().join("proofs").join(format!("{name}.json"));
        let transcript_body = std::fs::read_to_string(transcript_path).expect("timeout transcript");
        let transcript: serde_json::Value =
            serde_json::from_str(&transcript_body).expect("timeout transcript JSON");
        let envelope = &transcript["terminal"]["error_envelope"];
        assert_eq!(envelope["kind"], "Timeout");
        assert_eq!(envelope["code"], "timeout");
        assert_eq!(envelope["retryable"], true);
        assert!(envelope["status"].is_null());
        assert_eq!(
            transcript["observed"]["reported_successful_partial_response"],
            false
        );
        if name == "openai-compatible.chat-stream-chunk-timeout" {
            assert_eq!(transcript["observed"]["stream_events_committed"], 0);
            assert_eq!(transcript["http_exchanges"][0]["response"]["status"], 200);
            assert!(
                transcript["http_exchanges"][0]["response"]["event_names"]
                    .as_array()
                    .expect("event names")
                    .iter()
                    .any(|event| event == "timeout")
            );
        }
    }
}

#[tokio::test]
async fn runtime_facade_turn_uses_scripted_transport_and_checks_invariants() {
    let proof = prove_runtime_facade_turn().await.expect("runtime proof");

    assert_eq!(proof.provider_kind, "openai-compatible");
    assert_eq!(proof.session_id, "sim-runtime-session");
    assert_eq!(proof.turn_index, 1);
    assert_eq!(proof.assistant_message, "Runtime scripted answer.");
    assert_eq!(proof.provider_exchange_count, 1);
    assert!(proof.runtime_invariant.is_passed());
    assert!(proof.provider_output_invariant.is_passed());
    assert!(
        proof
            .pending_tool_completion
            .turn_suspension_invariant
            .is_passed()
    );
    assert!(
        proof
            .pending_tool_completion
            .scheduler_resolution_invariant
            .is_passed()
    );
    assert!(
        proof
            .pending_tool_completion
            .final_result_invariant
            .is_passed()
    );
    assert!(
        proof
            .final_value_semantic_channel
            .semantic_channel_invariant
            .is_passed()
    );
}

#[tokio::test]
async fn pending_tool_completion_proof_uses_scheduler_delivered_tool_boundary() {
    let proof = prove_pending_tool_completion_through_turn()
        .await
        .expect("pending tool proof");

    assert_eq!(proof.session_id, "sim-pending-tool-session");
    assert_eq!(proof.turn_index, 1);
    assert_eq!(proof.assistant_message, "done");
    assert_eq!(proof.tool_name, "app_lookup");
    assert!(proof.scheduler_controlled);
    assert!(proof.turn_suspended_before_completion);
    assert_eq!(proof.completed_event_count_before_resolution, 0);
    assert!(proof.completed_event_count_after_resolution > 0);
    assert!(matches!(
        proof.completion_outcome,
        lash_core::ResolveOutcome::Accepted
    ));
    assert!(matches!(
        proof.duplicate_completion_outcome,
        lash_core::ResolveOutcome::AlreadyResolved {
            terminal: lash_core::Resolution::Ok(_)
        }
    ));
    assert!(proof.turn_suspension_invariant.is_passed());
    assert!(proof.scheduler_resolution_invariant.is_passed());
    assert!(proof.final_result_invariant.is_passed());
}

#[tokio::test]
async fn final_value_semantic_channel_proof_uses_runtime_outcome_and_event() {
    let proof = prove_final_value_semantic_channel()
        .await
        .expect("final value proof");

    assert_eq!(proof.session_id, "sim-final-value-session");
    assert_eq!(proof.turn_index, 1);
    assert_eq!(proof.facts.outcome_kind, "final_value");
    assert_eq!(
        proof.final_value,
        json!({
            "source": "semantic-channel",
            "ok": true,
            "count": 3,
        })
    );
    assert!(proof.final_value_event_count > 0);
    assert!(proof.assistant_prose_delta_count > 0);
    assert!(!proof.facts.transcript_inference_required);
    assert!(proof.semantic_channel_invariant.is_passed());
}

#[tokio::test]
async fn live_provider_failure_oracle_bites_on_a_committing_turn() {
    // END-TO-END NEGATIVE: drive a REAL `session.turn().run()` against a VALID
    // success script that streams AND COMMITS the leak prose (the same prose a
    // failure turn must NOT commit). The live-failure oracle MUST fail on it —
    // proving the "no committed output" assertion bites end-to-end, not just on
    // synthetic facts.
    let script = runtime_script_for_text(OPENAI_COMPATIBLE, LIVE_FAILURE_LEAK_PROSE)
        .expect("valid success control script");
    let facts = run_live_turn_facts(7, OPENAI_COMPATIBLE, script, "success_control", 1)
        .await
        .expect("drive committing control turn");

    // The control turn really did commit the prose (the runtime CAN commit).
    assert!(
        facts.committed_assistant_message_nonempty,
        "the success control turn should have committed the assistant prose: {facts:?}"
    );
    assert!(
        !facts.terminalized_failure,
        "the success control turn should not terminalize as a failure: {facts:?}"
    );
    assert!(
        facts.committed_prose_in_transcript,
        "the success control turn should leave the prose in the transcript: {facts:?}"
    );

    // The oracle catches the committed output.
    let verdict = crate::oracles::live_provider_failure_terminalizes(&facts);
    assert!(
        !verdict.is_passed(),
        "a turn that commits output MUST fail the live-provider-failure oracle: {}",
        verdict.message
    );
}

#[test]
fn generated_sim_search_mode_keeps_summary_lean_and_labels_shards() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let artifact_root = tmp.path().to_path_buf();
    let report = run_on_sim_harness_stack(
        "generated-sim-search-test",
        SIM_HARNESS_STACK_LIMIT_BYTES,
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            runtime.block_on(run_generated_sim_profile(
                artifact_root,
                "fast-random",
                4,
                24,
                SimShard::new(1, 2).expect("shard"),
                SimRunMode::Search,
            ))
        },
    )
    .expect("generated sim search");

    assert_eq!(report.mode, "search");
    assert_eq!(report.shard, "1/2");
    assert_eq!(report.configured_seeds, 4);
    // Shard 1/2 of 4 configured seeds owns seed indices 0 and 2.
    assert_eq!(report.counts.generated_seeds, 2);
    assert_eq!(report.events_path, None);
    assert_eq!(report.events_sha256, None);
    assert!(report.replay_reports.is_empty());
    assert_eq!(report.counts.replay_reports, 0);
    assert_eq!(report.counts.minimized_replays, 0);
    assert_eq!(report.counts.backend_replays, 0);
    assert_eq!(report.counts.oracle_failures, 0);
    assert!(report.counts.oracle_passes > 0);
    assert!(report.counts.boundary_events >= 4);
    assert!(report.scenario_contract_slices.is_empty());
    assert!(report.scenario_contract_packages.is_empty());
    assert!(report.generated_backend_regression_fixtures.is_empty());
    assert!(!report.generated_runtime_provider_matrix.is_empty());
    // A fully passing search run persists no per-seed failure packages;
    // the failures directory holds only the `_shape.json` descriptor.
    let failures_dir = tmp.path().join("failures");
    let seed_packages = std::fs::read_dir(&failures_dir)
        .expect("failures dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("seed-"))
        .count();
    assert_eq!(
        seed_packages, 0,
        "passing search run must not persist failure packages"
    );
    assert!(tmp.path().join(GENERATED_SIM_SUMMARY).exists());
    assert!(
        !tmp.path().join(GENERATED_SIM_EVENTS).exists(),
        "search mode must not retain the delivered-boundary log"
    );
}

#[test]
fn generated_sim_profile_writes_trace_replay_and_provider_artifacts() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let artifact_root = tmp.path().to_path_buf();
    let report = run_on_sim_harness_stack(
        "generated-sim-profile-test",
        SIM_HARNESS_STACK_LIMIT_BYTES,
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            runtime.block_on(run_generated_sim_profile(
                artifact_root,
                "fast-random",
                2,
                24,
                SimShard::FULL,
                SimRunMode::Evidence,
            ))
        },
    )
    .expect("generated sim");

    assert_eq!(report.profile, "fast-random");
    assert_eq!(report.counts.generated_seeds, 2);
    assert_eq!(report.counts.replay_reports, 2);
    assert_eq!(report.counts.minimized_replays, 2);
    for replay in &report.replay_reports {
        let transcript_path = replay
            .transcript_path
            .as_ref()
            .expect("generated transcript path");
        let transcript_sha256 = replay
            .transcript_sha256
            .as_ref()
            .expect("generated transcript hash");
        let absolute_path = tmp.path().join(transcript_path);
        assert!(absolute_path.exists(), "{}", absolute_path.display());
        let content = std::fs::read_to_string(&absolute_path).expect("generated transcript");
        assert!(
            !content.is_empty(),
            "generated transcript must be non-empty"
        );
        assert_eq!(
            &file_sha256(&absolute_path).expect("hash generated transcript"),
            transcript_sha256
        );
    }
    assert!(report.counts.boundary_events >= 4);
    assert_eq!(report.counts.oracle_failures, 0);
    assert!(
        report.counts.interleaving_depth_max >= 2,
        "generated lane must drive >= 2 provider turns concurrently, got {}",
        report.counts.interleaving_depth_max
    );
    assert!(report.counts.interleaving_depth_min >= 1);
    assert!(report.counts.interleaving_depth_min <= report.counts.interleaving_depth_max);
    assert_eq!(report.scenario_contracts.len(), 4);
    let expected_contract_slices = report
        .scenario_contracts
        .iter()
        .map(|manifest| manifest.contract_count)
        .sum::<usize>();
    assert_eq!(
        report.scenario_contract_slices.len(),
        expected_contract_slices
    );
    assert_eq!(
        report.counts.scenario_contract_slices,
        expected_contract_slices
    );
    assert_eq!(
        report.scenario_contract_packages.len(),
        expected_contract_slices
    );
    assert_eq!(
        report.counts.scenario_contract_packages,
        expected_contract_slices
    );
    let package_oracle_ids = report
        .scenario_contract_packages
        .iter()
        .map(|package| package.oracle_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        package_oracle_ids.len(),
        report.scenario_contract_packages.len(),
        "scenario packages must not share backing oracle ids"
    );
    assert!(
        report.scenario_contract_packages.iter().all(|package| {
            package.classification == "per_contract_oracle"
                && package.oracle_id.ends_with(package.test_name)
                && !package.oracle_id.ends_with(":coverage-manifest")
        }),
        "every scenario package must be backed by its own per-contract oracle"
    );
    let mut package_fact_graphs = BTreeMap::new();
    for slice in &report.scenario_contract_slices {
        let fingerprint =
            transition_fact_graph_fingerprint(&slice.generated_shape.transition_facts);
        assert!(
            package_fact_graphs
                .insert(fingerprint, slice.test_name)
                .is_none(),
            "scenario packages must not share identical generated transition fact graphs"
        );
    }
    assert_eq!(report.generated_backend_regression_fixtures.len(), 8);
    assert_eq!(report.counts.generated_backend_regression_fixtures, 8);
    let backend_regression_ids = report
        .generated_backend_regression_fixtures
        .iter()
        .map(|fixture| fixture.fixture_id)
        .collect::<BTreeSet<_>>();
    for fixture_id in [
        "queued-active-turn-cancel-race",
        "trigger-wakeup-routes-process",
        "duplicate-process-wake-idempotency",
        "worker-stale-completion-fenced",
        "durable-effect-crash-reopen-replay",
        "backend-retry-terminalization",
        "provider-protocol-terminalization",
        "rlm-standard-protocol-terminal-boundaries",
    ] {
        assert!(
            backend_regression_ids.contains(fixture_id),
            "missing generated backend regression fixture {fixture_id}"
        );
    }
    for fixture in &report.generated_backend_regression_fixtures {
        assert_eq!(fixture.status, "generated_cross_backend_valid_trace");
        assert!(tmp.path().join(&fixture.trace_path).exists());
        assert!(tmp.path().join(&fixture.package_path).exists());
        assert_eq!(fixture.replay_backends, vec!["model"]);
        assert_eq!(
            fixture.static_backend_replay_policy,
            "not_claimed_for_generated_scheduler_traces"
        );
        assert!(
            tmp.path()
                .join(&fixture.source_sqlite_replay_report_path)
                .exists()
        );
        assert!(
            fixture
                .semantic_oracles
                .contains(&"sim.oracle.state-machine-semantic-invariants.v1")
        );
    }
    assert!(
        report
            .scenario_contracts
            .iter()
            .any(|manifest| manifest.suite == "runtime" && manifest.contract_count == 8)
    );
    for suite in ["runtime", "standard", "rlm", "agent"] {
        assert!(
            report
                .scenario_contract_slices
                .iter()
                .any(|slice| slice.suite == suite),
            "missing generated scenario slice suite {suite}"
        );
        assert!(
            report
                .scenario_contract_packages
                .iter()
                .any(|package| package.suite == suite),
            "missing generated scenario package suite {suite}"
        );
    }
    let negative_fixtures = report
        .scenario_contract_packages
        .iter()
        .map(|slice| (slice.suite, slice.negative.fixture_id))
        .collect::<BTreeSet<_>>();
    for (suite, fixture_id) in [
        ("runtime", "operational-coverage-missing-cancellation"),
        ("runtime", "queued-input-operational-missing"),
        ("runtime", "trigger-wakeup-operational-missing"),
        ("standard", "standard-provider-error-missing-parser-matrix"),
        ("rlm", "rlm-lashlang-cell-missing-exec-outcome"),
        ("agent", "agent-parallel-join-missing-wake-session"),
    ] {
        assert!(
            negative_fixtures.contains(&(suite, fixture_id)),
            "missing {suite} negative fixture {fixture_id}"
        );
    }
    let operational_cases = report
        .scenario_contract_packages
        .iter()
        .flat_map(|package| package.operational_cases.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for case in [
        "queueing-inputs",
        "active-turn-input-queueing",
        "triggers-wakeups",
        "cancellation",
        "duplicate-replayed-inputs",
        "backend-retry",
        "lease-fencing",
        "provider-failure",
        "worker-failover",
        "rlm-lashlang-exec",
        "tool-loop",
        "durable-effect",
    ] {
        assert!(
            operational_cases.contains(case),
            "scenario packages did not cover operational case {case}"
        );
    }
    for package in &report.scenario_contract_packages {
        assert_eq!(package.status, "generated_replay_package_written");
        assert!(package.oracle_id.starts_with("sim.oracle.scenario."));
        assert!(!package.operational_cases.is_empty());
        assert!(!package.positive.selected_boundary_ids.is_empty());
        assert_eq!(package.positive.oracle_status, OracleStatus::Passed);
        assert!(package.positive.selected_event_count > 0);
        assert!(!package.positive.source_trace_paths.is_empty());
        for path in package
            .positive
            .source_trace_paths
            .iter()
            .chain(package.positive.replay_report_paths.iter())
            .chain(package.positive.sqlite_replay_report_paths.iter())
        {
            assert!(
                tmp.path().join(path).exists(),
                "missing positive replay artifact {path} for {}",
                package.package_id
            );
        }
        assert!(
            package.negative.fixture_path.ends_with(".json"),
            "negative fixture path should be concrete for {}",
            package.package_id
        );
        let package_path = tmp.path().join(&package.package_path);
        assert!(
            package_path.exists(),
            "missing scenario package artifact {package_path:?}"
        );
        let body = std::fs::read_to_string(&package_path).expect("package artifact");
        let artifact: serde_json::Value = serde_json::from_str(&body).expect("package JSON");
        assert_eq!(artifact["schema"], "lash.sim.scenario-contract-package.v1");
        assert_eq!(artifact["package_id"], package.package_id);
        assert_eq!(artifact["classification"], "per_contract_oracle");
        assert_eq!(artifact["positive"]["oracle_status"], "passed");
        assert!(
            !artifact["generated_shape"]["transition_facts"]
                .as_array()
                .unwrap()
                .is_empty(),
            "scenario package {} must include transition facts",
            package.package_id
        );
        assert!(!artifact["events"].as_array().unwrap().is_empty());
    }
    for slice in &report.scenario_contract_slices {
        assert_eq!(slice.status, "generated_trace_slice_written");
        assert!(slice.oracle_id.starts_with("sim.oracle.scenario."));
        assert_eq!(
            slice.generated_shape.schema,
            "lash.sim.scenario-contract-generated-shape.v1"
        );
        assert_eq!(slice.generated_shape.semantic_oracle, slice.semantic_oracle);
        assert!(
            slice.generated_shape.transition_kind.contains(slice.suite),
            "transition kind should name suite for {}",
            slice.test_name
        );
        assert!(
            slice
                .generated_shape
                .required_evidence
                .iter()
                .all(|evidence| evidence.selected_event_count > 0),
            "required evidence map must point to generated events for {}",
            slice.test_name
        );
        let fixture_path =
            std::path::Path::new(slice.generated_shape.negative_fixture.fixture_path);
        let manifest_relative_fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            slice
                .generated_shape
                .negative_fixture
                .fixture_path
                .strip_prefix("crates/lash-sim/")
                .unwrap_or(slice.generated_shape.negative_fixture.fixture_path),
        );
        assert!(
            fixture_path.exists() || manifest_relative_fixture.exists(),
            "negative fixture path must exist for {}",
            slice.test_name
        );
        assert!(slice.selected_event_count > 0);
        assert!(!slice.selected_evidence.is_empty());
        let slice_path = tmp.path().join(&slice.artifact_path);
        assert!(slice_path.exists(), "missing slice artifact {slice_path:?}");
        let body = std::fs::read_to_string(&slice_path).expect("slice artifact");
        let artifact: serde_json::Value = serde_json::from_str(&body).expect("slice JSON");
        assert_eq!(artifact["schema"], "lash.sim.scenario-contract-slice.v1");
        assert_eq!(artifact["contract"]["suite"], slice.suite);
        assert_eq!(artifact["contract"]["test_name"], slice.test_name);
        assert_eq!(artifact["oracle_id"], slice.oracle_id);
        assert_eq!(
            artifact["generated_shape"]["transition_kind"],
            slice.generated_shape.transition_kind
        );
        assert!(
            !artifact["generated_shape"]["transition_facts"]
                .as_array()
                .unwrap()
                .is_empty(),
            "scenario slice {} must include transition facts",
            slice.test_name
        );
        assert!(!artifact["events"].as_array().unwrap().is_empty());
        assert!(!artifact["verdicts"].as_array().unwrap().is_empty());
    }
    let runtime_transition_facts = report
        .scenario_contract_slices
        .iter()
        .filter(|slice| slice.suite == "runtime")
        .map(|slice| {
            (
                slice.semantic_oracle,
                slice
                    .generated_shape
                    .transition_facts
                    .iter()
                    .map(|fact| fact.fact.as_str())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        runtime_transition_facts.len(),
        8,
        "every runtime scenario contract must have generated transition facts"
    );
    for (semantic_oracle, facts) in &runtime_transition_facts {
        assert!(
            !facts.contains("generated_transition_evidence_present"),
            "runtime contract {semantic_oracle} must not use generic selected-event fallback"
        );
    }
    assert!(
        runtime_transition_facts
            .get("runtime.command_only_queue_drain")
            .is_some_and(|facts| facts.contains("command_queue_drains_with_real_lease_fence")),
        "command-only runtime contract must assert queued source keys plus real lease fencing"
    );
    assert!(
        runtime_transition_facts
            .get("runtime.observation_replay_preserves_input")
            .is_some_and(|facts| facts.contains("observer_reconnect_replays_original_input_state")),
        "observer replay runtime contract must assert concrete reconnect observation state"
    );
    let backend_linked_contracts = report
        .scenario_contract_slices
        .iter()
        .filter(|slice| slice.generated_shape.generated_backend_regression.is_some())
        .count();
    assert!(
        backend_linked_contracts >= 8,
        "high-risk scenario contracts should link to generated backend regression fixtures"
    );
    assert!(
        report
            .model_only_boundary_reviews
            .iter()
            .any(|review| review.boundary_kind == "worker")
    );
    assert!(
        report
            .provider_transport_exclusions
            .iter()
            .any(|exclusion| exclusion.path.contains("codex/oauth.rs"))
    );
    assert!(
        report
            .provider_transport_exclusions
            .iter()
            .any(
                |exclusion| exclusion.path == "crates/lash-provider-openai/src/codex.rs"
                    && exclusion.replacement_lane.contains("websocket")
            )
    );
    assert!(report.oracle_verdicts.iter().any(|verdict| {
        verdict.oracle_id == "sim.oracle.operational-coverage.v1"
            && verdict.status == OracleStatus::Passed
    }));
    assert!(tmp.path().join(GENERATED_SIM_SUMMARY).exists());
    assert!(tmp.path().join(GENERATED_SIM_EVENTS).exists());
    assert!(tmp.path().join(GENERATED_SIM_PROVIDER_MANIFEST).exists());
    assert!(tmp.path().join(GENERATED_SIM_FAILURE_SHAPE).exists());
    for replay in &report.replay_reports {
        assert!(tmp.path().join(&replay.trace_path).exists());
        assert!(tmp.path().join(&replay.replay_report_path).exists());
        assert!(tmp.path().join(&replay.minimized_trace_path).exists());
        assert!(tmp.path().join(&replay.failure_package_path).exists());
        assert!(tmp.path().join(&replay.minimize_report_path).exists());
        assert!(tmp.path().join(&replay.sqlite_database_path).exists());
        assert!(tmp.path().join(&replay.sqlite_replay_report_path).exists());
        assert!(
            replay
                .replay_command
                .contains("lash-sim --locked -- replay")
        );
        assert!(
            replay
                .sqlite_replay_command
                .contains("lash-sim --locked -- replay-sqlite")
        );
    }

    let summary_body =
        std::fs::read_to_string(tmp.path().join(GENERATED_SIM_SUMMARY)).expect("summary");
    let summary: serde_json::Value = serde_json::from_str(&summary_body).expect("summary JSON");
    assert_eq!(summary["schema"], "lash.sim.profile-summary.v1");
    assert_eq!(
        summary["runtime_proof"]["assistant_message"],
        "Runtime scripted answer."
    );
    assert_eq!(summary["counts"]["oracle_failures"], 0);
    assert_eq!(summary["counts"]["backend_replays"], 2);
    assert_eq!(summary["counts"]["minimized_replays"], 2);
    assert_eq!(summary["scenario_contracts"].as_array().unwrap().len(), 4);
    assert_eq!(
        summary["scenario_contract_slices"]
            .as_array()
            .unwrap()
            .len(),
        expected_contract_slices
    );
    assert_eq!(
        summary["counts"]["scenario_contract_slices"],
        expected_contract_slices
    );
    assert_eq!(
        summary["scenario_contract_packages"]
            .as_array()
            .unwrap()
            .len(),
        expected_contract_slices
    );
    assert_eq!(
        summary["counts"]["scenario_contract_packages"],
        expected_contract_slices
    );
    assert_eq!(
        summary["generated_backend_regression_fixtures"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        summary["counts"]["generated_backend_regression_fixtures"],
        8
    );
    assert!(
        summary["model_only_boundary_reviews"]
            .as_array()
            .unwrap()
            .iter()
            .any(|review| review["boundary_kind"] == "worker")
    );
}

#[test]
fn generated_transcript_write_failure_is_best_effort() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let transcript_path = tmp.path().join("transcript.txt");
    std::fs::create_dir(&transcript_path).expect("directory at transcript path");

    assert_eq!(
        write_generated_transcript_best_effort(tmp.path(), &transcript_path, "review evidence"),
        None
    );
}

#[test]
fn postgres_effect_history_native_claim_is_consistent_across_reviews_docs_and_gate() {
    let repo_root = repo_root_for_test();
    let mut corpus = vec![(
        "model_only_boundary_reviews".to_string(),
        serde_json::to_string(&model_only_boundary_reviews()).expect("reviews JSON"),
    )];
    for relative in [
        "scripts/confidence-gate.sh",
        "docs/adr/0008-confidence-gate.md",
        "docs/adr/0009-deterministic-simulation-harness.md",
        "CONTEXT.md",
    ] {
        corpus.push((
            relative.to_string(),
            std::fs::read_to_string(repo_root.join(relative))
                .unwrap_or_else(|err| panic!("read {relative}: {err}")),
        ));
    }

    for (label, body) in &corpus {
        for stale in [
            "Postgres has no native Postgres effect-history controller",
            "permanent_non_goal_without_postgres_runtime_effect_controller",
            "without_postgres_runtime_effect_controller",
            "not_available_in_lash_postgres_store",
            "postgres-effect-history-exclusion",
        ] {
            assert!(
                !body.contains(stale),
                "{label} still contains stale Postgres effect-history exclusion wording: {stale}"
            );
        }
    }

    let reviews = &corpus[0].1;
    assert!(reviews.contains("PostgresRuntimeEffectController"));
    assert!(reviews.contains("lash_runtime_effect_replay"));
    let script = corpus
        .iter()
        .find(|(label, _)| label == "scripts/confidence-gate.sh")
        .map(|(_, body)| body)
        .expect("confidence gate corpus");
    assert!(script.contains("native_postgres_runtime_effect_controller"));
    assert!(script.contains("postgres-effect-history-status.json"));
}

fn repo_root_for_test() -> std::path::PathBuf {
    let mut cursor = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if cursor.join("scripts/confidence-gate.sh").is_file()
            && cursor.join("docs/adr/0008-confidence-gate.md").is_file()
        {
            return cursor;
        }
        if !cursor.pop() {
            panic!(
                "could not locate repository root from CARGO_MANIFEST_DIR={}",
                env!("CARGO_MANIFEST_DIR")
            );
        }
    }
}

#[test]
fn runtime_completion_ready_gates_provider_tool_durable_worker_boundaries() {
    let mut state = RuntimeCompletionState::default();
    let provider_one = BoundaryEvent::new(
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        4,
        "provider.chat.stream",
        json!({"turn_index": 1, "provider_kind": "openai-compatible", "text": "one"}),
    );
    let provider_two = BoundaryEvent::new(
        "session-001:provider:002",
        "session-001",
        BoundaryKind::Provider,
        5,
        "provider.chat.stream",
        json!({"turn_index": 2, "provider_kind": "openai-compatible", "text": "two"}),
    );
    let tool = BoundaryEvent::new(
        "session-001:tool:001",
        "session-001",
        BoundaryKind::Tool,
        6,
        "tool.return",
        json!({}),
    );
    let durable_first = BoundaryEvent::new(
        "session-001:durable:001",
        "session-001",
        BoundaryKind::DurableEffect,
        7,
        "durable.effect.complete",
        json!({"durable_key": "sleep/session-001/001"}),
    );
    let durable_replay = BoundaryEvent::new(
        "session-001:durable:001:replay",
        "session-001",
        BoundaryKind::DurableEffect,
        8,
        "durable.effect.replay",
        json!({"durable_key": "sleep/session-001/001"}),
    );
    let worker = BoundaryEvent::new(
        "worker-001:stale-completion",
        "worker-001",
        BoundaryKind::Worker,
        9,
        "worker.stale-completion-rejected",
        json!({"session": "session-001"}),
    );

    assert!(!runtime_completion_ready(&provider_one, &state));
    assert!(!runtime_completion_ready(&tool, &state));
    assert!(!runtime_completion_ready(&worker, &state));
    state.observe(&test_delivered(
        0,
        "session-001:ingress",
        "session-001",
        BoundaryKind::Ingress,
        json!({}),
    ));
    assert!(runtime_completion_ready(&provider_one, &state));
    assert!(!runtime_completion_ready(&provider_two, &state));
    assert!(!runtime_completion_ready(&tool, &state));
    assert!(runtime_completion_ready(&durable_first, &state));
    assert!(!runtime_completion_ready(&durable_replay, &state));
    assert!(runtime_completion_ready(&worker, &state));

    state.observe(&test_delivered(
        1,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"provider_output": "one"}),
    ));
    assert!(runtime_completion_ready(&provider_two, &state));
    assert!(runtime_completion_ready(&tool, &state));

    state.observe(&test_delivered(
        2,
        "session-001:durable:001",
        "session-001",
        BoundaryKind::DurableEffect,
        json!({
            "durable_key": "sleep/session-001/001",
            "replayed": false
        }),
    ));
    assert!(runtime_completion_ready(&durable_replay, &state));
}

#[test]
fn provider_runtime_completion_registration_does_not_schedule_turn_completion_immediately() {
    let mut state = RuntimeCompletionState::default();
    state.observe(&test_delivered(
        0,
        "session-001:ingress",
        "session-001",
        BoundaryKind::Ingress,
        json!({}),
    ));
    let registered_after = test_delivered(
        0,
        "session-001:ingress",
        "session-001",
        BoundaryKind::Ingress,
        json!({}),
    );
    let mut queue = RuntimeCompletionQueue::new([
        BoundaryEvent::new(
            "session-001:provider:001",
            "session-001",
            BoundaryKind::Provider,
            2,
            "provider.chat.stream",
            json!({
                "provider_kind": "openai-compatible",
                "text": "scheduled answer",
                "turn_index": 1
            }),
        ),
        BoundaryEvent::new(
            "session-001:provider:002",
            "session-001",
            BoundaryKind::Provider,
            3,
            "provider.chat.stream",
            json!({
                "provider_kind": "openai-compatible",
                "text": "later answer",
                "turn_index": 2
            }),
        ),
    ]);
    let ready = queue.take_ready(|event| runtime_completion_ready(event, &state));
    assert_eq!(ready.len(), 1);
    let event = ready.into_iter().next().expect("provider ready");
    let units = runtime_completion_units(&event).expect("provider completion units");
    let (_pending, completion_event) =
        queue.register_pending_event(event, &registered_after, "provider_turn_completion", units);

    assert_eq!(queue.registered_len(), 1);
    assert_eq!(queue.pending_ids(), vec!["session-001:provider:002"]);
    assert_eq!(completion_event.boundary_id, "session-001:provider:001");
    assert!(completion_event.payload.get("runtime_completion").is_some());
    assert_eq!(
        completion_event
            .payload
            .pointer("/runtime_completion/completion_family")
            .and_then(Value::as_str),
        Some("provider_turn_completion")
    );
    assert!(
        completion_event
            .payload
            .pointer("/runtime_completion/completion_units")
            .and_then(Value::as_array)
            .is_some_and(|units| units.iter().any(|unit| unit
                .get("unit")
                .and_then(Value::as_str)
                .is_some_and(|name| name.contains("provider:"))))
    );
}

#[test]
fn runtime_completion_backend_mutation_idle_session_mutation_guard() {
    let backend_failure = BoundaryEvent::new(
        "session-001:backend-failure:001",
        "session-001",
        BoundaryKind::BackendFailure,
        4,
        "backend.failure",
        json!({}),
    );
    let provider_mutation = BoundaryEvent::new(
        "session-001:provider-mutation:001",
        "session-001",
        BoundaryKind::ProviderMutation,
        5,
        "provider.mutation",
        json!({}),
    );
    let mut state = RuntimeCompletionState::default();

    assert!(
        !runtime_completion_ready(&backend_failure, &state),
        "backend failure must not run before the session opens"
    );
    assert!(
        !runtime_completion_ready(&provider_mutation, &state),
        "provider mutation must not run before the session opens"
    );

    state.observe(&test_delivered(
        0,
        "session-001:ingress",
        "session-001",
        BoundaryKind::Ingress,
        json!({}),
    ));
    assert!(
        runtime_completion_ready(&backend_failure, &state),
        "backend failure is ready once its session is open and idle"
    );
    assert!(
        runtime_completion_ready(&provider_mutation, &state),
        "provider mutation is ready once its session is open and idle"
    );

    state.provider_started("session-001");
    assert!(
        !runtime_completion_ready(&backend_failure, &state),
        "backend failure must not interleave with an active provider turn for the same session"
    );
    assert!(
        !runtime_completion_ready(&provider_mutation, &state),
        "provider mutation must not interleave with an active provider turn for the same session"
    );
}

#[test]
fn script_bundle_hash_is_stable_for_current_bundle() {
    let scripts = script_hash_manifest().expect("scripts");
    let hash = script_bundle_hash(&scripts);

    assert_eq!(hash.len(), 64);
    assert_eq!(hash, script_bundle_hash(&scripts));
}

fn test_delivered(
    sequence: usize,
    boundary_id: &str,
    actor_alias: &str,
    kind: BoundaryKind,
    observed: Value,
) -> crate::scheduler::DeliveredBoundary {
    crate::scheduler::DeliveredBoundary {
        schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
        sequence,
        scheduler: crate::scheduler::SchedulerDeliveryEvidence {
            scheduler_controlled: true,
            delivered_at: sequence as u64,
            ..crate::scheduler::SchedulerDeliveryEvidence::default()
        },
        boundary_id: boundary_id.to_string(),
        actor_alias: actor_alias.to_string(),
        kind,
        at: sequence as u64,
        label: format!("{kind:?}"),
        payload: json!({}),
        observed,
    }
}
