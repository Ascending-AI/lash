use super::*;
use crate::scheduler::PendingRuntimeBoundary;
use lash_sansio::SessionId;

#[tokio::test]
async fn attachment_owner_sweep_is_deterministic_across_memory_and_sqlite() {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory store set");
    lash_conformance::attachment_reference_lifecycle_with_store(
        memory.session_store_factory(),
        memory.attachment_store(),
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    lash_conformance::attachment_reference_lifecycle_with_store(
        std::sync::Arc::new(
            lash_sqlite_store::SqliteStore::open(&tmp.path().join("sessions.db"))
                .await
                .expect("SQLite session store"),
        ),
        std::sync::Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            tmp.path().join("attachments"),
        )),
    )
    .await;
}

#[test]
fn standard_protocol_full_text_projection_guard() {
    let result = run_standard_protocol_contract(
        "standard.full_text_projection_guard",
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
        vec![json!("first chunksecond chunk")],
        "Standard full_text must be the visible-parts projection; parts are the sole authority"
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
        "the guard must execute a real Standard LLM turn"
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
        RlmTermination::Natural { schema: None },
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
        "fixed RLM execution must preserve LlmResponse.full_text()"
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

    assert_eq!(response.full_text(), "facade response text");
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

    assert!(response.full_text().contains("semantic-channel"));
    assert!(
        response_text_part(&response).is_some_and(|text| text.contains("semantic-channel")),
        "rlm final-value provider must return the semantic text part"
    );
}

#[tokio::test]
async fn pending_tool_roundtrip_provider_response_shape_mutation_guard() {
    let final_answer = Arc::new(tokio::sync::Notify::new());
    let mut provider = pending_tool_roundtrip_provider(Arc::clone(&final_answer));
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

    final_answer.notify_one();
    let final_response = provider
        .complete(openai_compatible_request(false))
        .await
        .expect("pending tool provider final response");
    assert_eq!(final_response.full_text(), "done");
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
            assert_eq!(envelope["code"], "lash:timeout");
            assert_eq!(envelope["retryable"], true);
        }
        if name == "openai-compatible.chat-stream-chunk-timeout" {
            assert_eq!(transcript["observed"]["stream_events_committed"], 1);
            assert_eq!(transcript["observed"]["evidence_events_committed"], 1);
            assert_eq!(
                transcript["observed"]["partial_response_events_committed"],
                0
            );
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
        assert_eq!(envelope["code"], "lash:timeout");
        assert_eq!(envelope["retryable"], true);
        assert!(envelope["status"].is_null());
        assert_eq!(
            transcript["observed"]["reported_successful_partial_response"],
            false
        );
        if name == "openai-compatible.chat-stream-chunk-timeout" {
            assert_eq!(transcript["observed"]["stream_events_committed"], 1);
            assert_eq!(transcript["observed"]["evidence_events_committed"], 1);
            assert_eq!(
                transcript["observed"]["partial_response_events_committed"],
                0
            );
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

#[test]
fn runtime_scenario_contracts_dispatch_to_contract_owned_facts() {
    let line = |sequence, kind, observed| {
        TraceEventLine::new(
            "runtime-contract-facts",
            1,
            "test",
            test_delivered(
                sequence,
                &format!("runtime-contract-fact:{sequence}"),
                "session-001",
                kind,
                observed,
            ),
        )
    };

    for contract in RUNTIME_SCENARIO_CONTRACTS {
        let (events, expected) = match contract.semantic_oracle {
            "runtime.command_before_turn_work" => (
                vec![
                    line(
                        1,
                        BoundaryKind::Trigger,
                        json!({
                            "trigger_delivered": true,
                            "started_process": true,
                            "reservation_count": 1,
                        }),
                    ),
                    line(
                        2,
                        BoundaryKind::QueuedIngress,
                        json!({
                            "source_key": "command-source",
                            "ingress_mode": "active_turn",
                            "input_state": "pending",
                        }),
                    ),
                ],
                vec![
                    "trigger_routes_process_wakeup",
                    "active_turn_input_queued_hidden",
                ],
            ),
            "runtime.command_only_queue_drain" => (
                vec![line(
                    1,
                    BoundaryKind::QueuedIngress,
                    json!({"source_key": "command-source"}),
                )],
                vec!["command_queue_drains_queued_source_keys"],
            ),
            "runtime.queued_work_keeps_pending_input" => (
                vec![line(
                    1,
                    BoundaryKind::QueuedIngress,
                    json!({
                        "source_key": "queued-source",
                        "ingress_mode": "active_turn",
                        "input_state": "pending",
                        "input_id": "input-001",
                    }),
                )],
                vec!["active_turn_input_queued_hidden"],
            ),
            "runtime.queued_turn_input_completion" => (
                vec![
                    line(
                        1,
                        BoundaryKind::QueuedIngress,
                        json!({
                            "source_key": "queued-source",
                            "ingress_mode": "active_turn",
                            "input_state": "pending",
                        }),
                    ),
                    line(
                        2,
                        BoundaryKind::Provider,
                        json!({"success": true, "provider_exchange_count": 1}),
                    ),
                ],
                vec![
                    "active_turn_input_queued_hidden",
                    "queued_turn_input_followed_by_provider_completion",
                ],
            ),
            "runtime.observation_replay_preserves_input" => (
                vec![line(
                    1,
                    BoundaryKind::Observer,
                    json!({
                        "reconnected": true,
                        "turn_index": 1,
                        "observer_invariants": {
                            "session_id": true,
                            "turn_index_converged": true,
                            "transcript_message_count_converged": true,
                        },
                    }),
                )],
                vec!["observer_reconnect_replays_original_input_state"],
            ),
            "runtime.checkpoint_redrive_cancel" => (
                vec![
                    line(
                        1,
                        BoundaryKind::QueuedIngress,
                        json!({
                            "source_key": "queued-source",
                            "ingress_mode": "active_turn",
                            "input_state": "pending",
                        }),
                    ),
                    line(
                        2,
                        BoundaryKind::Cancellation,
                        json!({"cancelled": true, "target": "queued-source"}),
                    ),
                ],
                vec![
                    "active_turn_input_queued_hidden",
                    "cancellation_terminalized_pending_input",
                ],
            ),
            semantic => panic!("missing runtime contract test fixture for {semantic}"),
        };

        let facts = scenario_transition_facts(contract, &events)
            .expect("runtime contract should yield transition facts");
        let actual = facts
            .iter()
            .map(|fact| fact.fact.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "unexpected facts for {}",
            contract.test_name
        );
        assert!(
            facts
                .iter()
                .all(|fact| fact.status == "passed" && !fact.boundary_ids.is_empty())
        );
    }
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
fn runtime_completion_ready_gates_provider_tool_and_durable_boundaries() {
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
    let durable = BoundaryEvent::new(
        "session-001:durable:001",
        "session-001",
        BoundaryKind::DurableEffect,
        7,
        "durable.sleep.crash-redrive",
        json!({"durable_key": "sleep/session-001/001"}),
    );

    assert!(!runtime_completion_ready(&provider_one, &state));
    assert!(!runtime_completion_ready(&tool, &state));
    assert!(!runtime_completion_ready(&durable, &state));
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
    assert!(runtime_completion_ready(&durable, &state));

    state.observe(&test_delivered(
        1,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({"provider_output": "one"}),
    ));
    assert!(runtime_completion_ready(&provider_two, &state));
    assert!(runtime_completion_ready(&tool, &state));

    // With provider-turn serialization a handler boundary never starts beside a
    // live provider turn of any session.
    let mut serialized = RuntimeCompletionState {
        serialize_provider_turns: true,
        ..RuntimeCompletionState::default()
    };
    for alias in ["session-001", "session-002"] {
        serialized.observe(&test_delivered(
            0,
            &format!("{alias}:ingress"),
            alias,
            BoundaryKind::Ingress,
            json!({}),
        ));
    }
    assert!(runtime_completion_ready(&durable, &serialized));
    serialized.provider_started("session-002");
    assert!(!runtime_completion_ready(&durable, &serialized));
    serialized.observe(&test_delivered(
        1,
        "session-002:provider:001",
        "session-002",
        BoundaryKind::Provider,
        json!({}),
    ));
    assert!(runtime_completion_ready(&durable, &serialized));
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
    let (_pending, completion_event) = queue.register_pending_event(
        event,
        &registered_after,
        RuntimeCompletionFamily::ProviderTurnCompletion,
        units,
    );

    assert_eq!(queue.registered_len(), 1);
    assert_eq!(queue.pending_ids(), vec!["session-001:provider:002"]);
    assert_eq!(completion_event.boundary_id, "session-001:provider:001");
    let evidence = PendingRuntimeBoundary::from_payload(&completion_event.payload)
        .expect("registered completion carries typed pending evidence");
    assert_eq!(
        evidence.completion_family,
        RuntimeCompletionFamily::ProviderTurnCompletion
    );
    assert!(
        evidence
            .completion_units
            .iter()
            .any(|unit| unit.unit.contains("provider:"))
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
fn runtime_completion_durable_and_observer_readiness_and_units() {
    let durable = BoundaryEvent::new(
        "session-001:durable-effect:001",
        "session-001",
        BoundaryKind::DurableEffect,
        1,
        "durable.sleep.crash-redrive",
        json!({"durable_key": "sleep/session-001/001"}),
    );
    let observer = BoundaryEvent::new(
        "session-001:observer:001",
        "session-001",
        BoundaryKind::Observer,
        2,
        "observer.snapshot",
        json!({"turn_index": 1}),
    );
    let mut state = RuntimeCompletionState::default();

    assert!(
        !runtime_completion_ready(&durable, &state),
        "a durable effect must not run before its session opens"
    );
    assert!(
        !runtime_completion_ready(&observer, &state),
        "observer must not run before its session opens"
    );

    state.observe(&test_delivered(
        0,
        "session-001:ingress",
        "session-001",
        BoundaryKind::Ingress,
        json!({}),
    ));
    assert!(runtime_completion_ready(&durable, &state));
    assert!(
        !runtime_completion_ready(&observer, &state),
        "observer is not ready until turn 1 completes"
    );

    state.provider_started("session-001");
    assert!(
        !runtime_completion_ready(&durable, &state),
        "a durable effect is not ready while its session's provider turn is active"
    );

    state.observe(&test_delivered(
        1,
        "session-001:provider:001",
        "session-001",
        BoundaryKind::Provider,
        json!({}),
    ));
    assert!(runtime_completion_ready(&durable, &state));
    assert!(
        runtime_completion_ready(&observer, &state),
        "observer is ready once turn 1 completes"
    );

    assert_eq!(
        runtime_completion_family(durable.kind),
        Some(RuntimeCompletionFamily::DurableEffectCompletion)
    );
    assert_eq!(
        runtime_completion_family(observer.kind),
        Some(RuntimeCompletionFamily::ObserverSnapshot)
    );

    let durable_units = runtime_completion_units(&durable).expect("durable units");
    assert_eq!(durable_units.len(), 1);
    assert_eq!(
        durable_units[0].unit,
        "runtime:durable_effect_crash_redrive"
    );

    let observer_units = runtime_completion_units(&observer).expect("observer units");
    assert_eq!(observer_units.len(), 1);
    assert_eq!(observer_units[0].unit, "runtime:observer_snapshot");
}

#[test]
fn is_scheduler_owned_runtime_completion_matches_kinds() {
    for kind in [
        BoundaryKind::Ingress,
        BoundaryKind::QueuedIngress,
        BoundaryKind::Provider,
        BoundaryKind::ProviderEvent,
        BoundaryKind::Tool,
        BoundaryKind::ExecCode,
        BoundaryKind::DurableEffect,
        BoundaryKind::Observer,
        BoundaryKind::Cancellation,
        BoundaryKind::Trigger,
        BoundaryKind::BackendFailure,
        BoundaryKind::ProviderMutation,
    ] {
        assert_eq!(
            is_scheduler_owned_runtime_completion(kind),
            SCHEDULER_OWNED_RUNTIME_COMPLETION_KINDS.contains(&kind),
            "is_scheduler_owned_runtime_completion divergence for {kind:?}"
        );
    }
}

#[test]
fn script_bundle_hash_is_stable_for_current_bundle() {
    // A literal pin: any change to the canonical bundle's paths or contents
    // turns this red, so the update is a deliberate act in the diff.
    let scripts = script_hash_manifest().expect("scripts");

    assert_eq!(
        script_bundle_hash(&scripts),
        "56d92b184abc621e0fcdc25baa72e64936aa491bac8bef04b7b0db6dad14cfad"
    );
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

/// A generated world reads a turn's activity from its live replay only once
/// the turn's shift has stopped, so the replay keeps it however long the host
/// takes to run the workload: a loaded run must see what an idle one sees
/// (FIG-5148).
#[tokio::test]
async fn a_world_live_replay_keeps_activity_however_long_the_host_runs() {
    use lash_core::{
        LiveReplayEventDraft, LiveReplayOutcome, LiveReplayStore as _,
        SessionObservationEventPayload, SessionRevision,
    };

    let clock = SimClock::new();
    let store = lash::observe::InMemoryLiveReplayStore::with_clock(
        world_live_replay_config(),
        clock.clone(),
    );
    let session = SessionId::fixture("slow-host");
    let revision = SessionRevision::new(1);
    let cursor = store.current_cursor(&session, revision);
    store
        .publish(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<lash_core::TurnId>,
                SessionObservationEventPayload::ResidentChanged,
            )],
        )
        .await
        .expect("publish one event");

    clock.advance_by(60 * 60 * 1000).await;
    store.expire_idle_sessions();

    match store
        .replay_after_cursor(&cursor)
        .await
        .expect("replay after the host's delay")
    {
        LiveReplayOutcome::Replayed(events) => assert_eq!(events.len(), 1),
        LiveReplayOutcome::Gap(reason) => {
            panic!("an hour of host time dropped the world's activity: {reason:?}")
        }
    }
}
