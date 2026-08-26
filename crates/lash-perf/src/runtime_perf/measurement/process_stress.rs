use lash_core::TestProcessRegistryWriteExt;

const PROCESS_LIST_STRESS_BATCH: usize = 128;

async fn run_once_process_list_stress(chat_turns: usize) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::ProcessListStress;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let session_scope = lash_core::SessionScope::new("runtime-perf-process-list");
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let process_count = chat_turns.max(1) * PROCESS_LIST_STRESS_BATCH;
    for index in 0..process_count {
        let process_id = format!("process-list-stress-{index:05}");
        registry
            .register_process(process_list_stress_registration(
                process_id.clone(),
                session_scope.clone(),
                index,
            ))
            .await?;
        registry
            .add_observer(
                &session_scope.session_id,
                &process_id,
                lash_core::ProcessObserverBy::host(format!(
                    "process-list-stress-{index:05}"
                )),
            )
            .await?;
        if index % 2 == 1 {
            registry
                .complete_process(
                    &process_id,
                    lash_core::ProcessAwaitOutput::from_tool_output(
                        lash_core::ToolCallOutput::success(serde_json::json!({ "index": index })),
                    ),
                    lash_core::ProcessCompletionAuthority::external_owner(),
                )
                .await?;
        }
    }
    // Dedicated long-lived process for the signal/wait phases: its event log
    // grows across turns, so the phases also expose append-cost growth with
    // log length (the durable-suspension hot path).
    let signal_process_id = "process-signal-stress";
    let signal_event_type = lash_core::facade_support::process_signal_event_type("stress")?;
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                signal_process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({ "label": "signal stress" }),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_event_types(vec![lash_core::ProcessEventType {
                name: signal_event_type.clone(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await?;
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut rendered_payload_bytes = 0usize;
    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();

        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        let live_entries = registry
            .list_live_observed_by(&session_scope.session_id)
            .await?;
        phase_profile.insert(
            "process_list_stress.list_live".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );
        if live_entries.iter().any(lash_core::ProcessRecord::is_terminal) {
            anyhow::bail!("process_list_stress live listing included a terminal process");
        }

        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        let all_entries = registry.list_observed_by(&session_scope.session_id).await?;
        phase_profile.insert(
            "process_list_stress.list_all".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );
        if all_entries.len() != process_count {
            anyhow::bail!(
                "process_list_stress all listing expected {process_count} entries, got {}",
                all_entries.len()
            );
        }

        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        let global_records = registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await?;
        phase_profile.insert(
            "process_list_stress.list_global".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );
        if global_records.len() != process_count + 1 {
            anyhow::bail!(
                "process_list_stress global listing expected {} records, got {}",
                process_count + 1,
                global_records.len()
            );
        }

        const SIGNALS_PER_TURN: usize = 32;
        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        for signal_index in 0..SIGNALS_PER_TURN {
            registry
                .append_event(
                    signal_process_id,
                    lash_core::ProcessEventAppendRequest::new(
                        signal_event_type.clone(),
                        serde_json::json!({ "turn": turn_index, "n": signal_index }),
                    )
                    .with_replay_key(format!(
                        "process:{signal_process_id}:signal.stress:{turn_index}:{signal_index}"
                    )),
                )
                .await?;
        }
        phase_profile.insert(
            "process_list_stress.signal_append".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );

        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        let waiting = registry
            .set_process_wait(
                signal_process_id,
                lash_core::WaitState {
                    since_ms: turn_index as u64 + 1,
                    kind: lash_core::WaitKind::Signal {
                        name: "stress".to_string(),
                        event_type: signal_event_type.clone(),
                        key: format!(
                            "process:{signal_process_id}:signal.stress:{}",
                            turn_index + 1
                        ),
                        ordinal: turn_index as u64 + 1,
                    },
                },
            )
            .await?;
        if waiting.wait.is_none() {
            anyhow::bail!("process_list_stress wait facet did not round-trip");
        }
        registry.clear_process_wait(signal_process_id).await?;
        phase_profile.insert(
            "process_list_stress.wait_roundtrip".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );

        // Env-spec hashing is the new per-start cost (content-addressed
        // capture); measure it standalone so regressions in stable_hash or
        // spec encoding surface here rather than inside start latency.
        const ENV_HASHES_PER_TURN: usize = 64;
        let phase_started = Instant::now();
        let phase_before_alloc = allocator_stats();
        let phase_before_memory = process_memory_sample();
        for hash_index in 0..ENV_HASHES_PER_TURN {
            let mut options = lash_core::PluginOptions::default();
            options.plugins.insert(
                "stress".to_string(),
                serde_json::json!({ "turn": turn_index, "n": hash_index }),
            );
            let spec = lash_core::ProcessExecutionEnvSpec::new(
                options,
                lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            );
            let env_ref = spec
                .stable_ref()
                .map_err(|err| anyhow::anyhow!("env spec hashing failed: {err}"))?;
            if env_ref.as_str().is_empty() {
                anyhow::bail!("process_list_stress env hash produced an empty ref");
            }
        }
        phase_profile.insert(
            "process_list_stress.env_spec_hash".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(phase_started),
                allocations: alloc_delta(phase_before_alloc, allocator_stats()),
                rss_growth_kb: diff_opt_i64(
                    phase_before_memory.rss_kb,
                    process_memory_sample().rss_kb,
                ),
            },
        );

        let (live_payload_len, live_render_phase) =
            measure_runtime_perf_phase("process_list_stress.render_live_json", || {
                serde_json::to_string(&process_list_tool_payload(&live_entries))
                    .map(|payload| payload.len())
                    .map_err(anyhow::Error::from)
            })?;
        phase_profile.insert(live_render_phase.0, live_render_phase.1);
        let (all_payload_len, all_render_phase) =
            measure_runtime_perf_phase("process_list_stress.render_all_json", || {
                serde_json::to_string(&process_list_tool_payload(&all_entries))
                    .map(|payload| payload.len())
                    .map_err(anyhow::Error::from)
            })?;
        phase_profile.insert(all_render_phase.0, all_render_phase.1);
        rendered_payload_bytes += live_payload_len + all_payload_len;

        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();

        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        tokio::task::yield_now().await;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let turn_total_alloc =
            sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);

        turns.push(RuntimePerfTurnResult {
            turn_index,
            run_turn_ms,
            await_background_work_ms,
            total_ms: round3(run_turn_ms + await_background_work_ms),
            memory: RuntimePerfTurnMemoryRunResult {
                rss_before_kb: turn_before_memory.rss_kb,
                rss_after_turn_kb: after_turn_memory.rss_kb,
                rss_after_await_kb: after_await_memory.rss_kb,
                peak_hwm_before_kb: turn_before_memory.hwm_kb,
                peak_hwm_after_await_kb: after_await_memory.hwm_kb,
                rss_growth_kb: diff_opt_i64(turn_before_memory.rss_kb, after_await_memory.rss_kb),
                hwm_growth_kb: diff_opt_i64(turn_before_memory.hwm_kb, after_await_memory.hwm_kb),
            },
            allocations: RuntimePerfTurnAllocationRunResult {
                run_turn: run_turn_alloc,
                await_background_work: await_background_work_alloc,
                total: turn_total_alloc,
            },
            phase_profile,
            turn_usage: TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let _export_shape = serde_json::json!({
        "process_count": process_count,
        "rendered_payload_bytes": rendered_payload_bytes,
    })
    .to_string();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms,
        seed_state_ms,
        run_turn_ms: round3(turns.iter().map(|turn| turn.run_turn_ms).sum()),
        await_background_work_ms: round3(
            turns.iter().map(|turn| turn.await_background_work_ms).sum(),
        ),
        export_state_ms,
        total_ms: elapsed_ms(total_started),
        session_nodes: process_count,
        active_path_messages: process_count / 2,
        extra_counters: BTreeMap::from([
            ("process_count".to_string(), process_count as u64),
            (
                "rendered_payload_bytes".to_string(),
                rendered_payload_bytes as u64,
            ),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms: BTreeMap::new(),
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: after_build_memory.rss_kb,
            rss_after_seed_kb: after_seed_memory.rss_kb,
            rss_after_turn_kb: last_turn_memory.and_then(|memory| memory.rss_after_turn_kb),
            rss_after_await_kb: last_turn_memory.and_then(|memory| memory.rss_after_await_kb),
            rss_after_export_kb: after_export_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_export_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_export_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_export_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: build_runtime_alloc,
            seed_state: seed_state_alloc,
            run_turn: sum_allocation_deltas(turns.iter().map(|turn| &turn.allocations.run_turn)),
            await_background_work: sum_allocation_deltas(
                turns
                    .iter()
                    .map(|turn| &turn.allocations.await_background_work),
            ),
            export_state: export_state_alloc,
            total: total_alloc,
        },
        phase_profile: sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turns,
        cumulative_usage: SessionUsageReport::default(),
    })
}

fn process_list_stress_registration(
    process_id: String,
    session_scope: lash_core::SessionScope,
    index: usize,
) -> lash_core::ProcessRegistration {
    lash_core::ProcessRegistration::new(
        process_id,
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "index": index }),
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::session(session_scope),
    )
}

fn process_list_tool_payload(entries: &[lash_core::ProcessRecord]) -> serde_json::Value {
    serde_json::json!(
        entries
            .iter()
            .map(|record| {
                lash_core::ProcessHandleView::new(
                    record.id.clone(),
                    record.identity.clone(),
                    record.status,
                )
            })
            .collect::<Vec<_>>()
    )
}

const OPENAI_RESPONSES_SSE_CHUNK_COUNT: usize = 256;
const OPENAI_RESPONSES_SSE_CHUNK_BYTES: usize = 96;

fn openai_responses_sse_payload(turn_index: usize) -> String {
    let alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
    let mut full_text = String::new();
    let mut body = String::new();
    let message_id = format!("msg-runtime-perf-{turn_index}");
    let reasoning_id = format!("rs-runtime-perf-{turn_index}");
    let function_item_id = format!("fc-runtime-perf-{turn_index}");
    let function_call_id = format!("call-runtime-perf-{turn_index}");

    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "id": message_id.as_str(),
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "id": reasoning_id.as_str(),
                "type": "reasoning",
                "summary": []
            }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": reasoning_id.as_str(),
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": reasoning_id.as_str(),
            "summary_index": 0,
            "delta": "parser benchmark reasoning "
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": reasoning_id.as_str(),
            "summary_index": 0,
            "text": "parser benchmark reasoning "
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": reasoning_id.as_str(),
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "parser benchmark reasoning " }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "id": reasoning_id.as_str(),
                "type": "reasoning",
                "summary": [
                    { "type": "summary_text", "text": "parser benchmark reasoning " }
                ]
            }
        }),
    );

    for index in 0..OPENAI_RESPONSES_SSE_CHUNK_COUNT {
        let prefix = format!("responses-chunk-{index:03}: ");
        let fill_len = OPENAI_RESPONSES_SSE_CHUNK_BYTES.saturating_sub(prefix.len() + 1);
        let fill = alphabet
            .chars()
            .cycle()
            .skip(index % alphabet.len())
            .take(fill_len)
            .collect::<String>();
        let delta = format!("{prefix}{fill}\n");
        full_text.push_str(&delta);
        push_sse_event(
            &mut body,
            serde_json::json!({
                "type": "response.output_text.delta",
                "item_id": message_id.as_str(),
                "content_index": 0,
                "delta": delta
            }),
        );
    }
    full_text.push_str("runtime perf benchmark ok");

    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": message_id.as_str(),
            "content_index": 0,
            "delta": "runtime perf benchmark ok"
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_text.done",
            "item_id": message_id.as_str(),
            "content_index": 0,
            "text": full_text.as_str()
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "id": message_id.as_str(),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": full_text.as_str()
                    }
                ]
            }
        }),
    );

    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "id": function_item_id.as_str(),
                "type": "function_call",
                "call_id": function_call_id.as_str(),
                "name": "benchmark_echo",
                "arguments": ""
            }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": function_item_id.as_str(),
            "delta": "{\"value\":"
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": function_item_id.as_str(),
            "delta": "\"runtime perf benchmark ok\"}"
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": function_item_id.as_str(),
            "arguments": "{\"value\":\"runtime perf benchmark ok\"}"
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "id": function_item_id.as_str(),
                "type": "function_call",
                "call_id": function_call_id.as_str(),
                "name": "benchmark_echo",
                "arguments": "{\"value\":\"runtime perf benchmark ok\"}"
            }
        }),
    );
    push_sse_event(
        &mut body,
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": format!("resp-runtime-perf-{turn_index}"),
                "type": "response",
                "status": "completed",
                "output": [
                    {
                        "id": message_id,
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            {
                                "type": "output_text",
                                "text": full_text.as_str()
                            }
                        ]
                    }
                ],
                "usage": {
                    "input_tokens": 1024,
                    "output_tokens": 64,
                    "input_tokens_details": {
                        "cached_tokens": 512
                    },
                    "output_tokens_details": {
                        "reasoning_output_tokens": 48
                    }
                }
            }
        }),
    );
    body.push_str("data: [DONE]\n\n");
    body
}

fn push_sse_event(body: &mut String, event: serde_json::Value) {
    body.push_str("data: ");
    body.push_str(&event.to_string());
    body.push_str("\n\n");
}

fn direct_llm_client_request(turn_index: usize) -> lash::direct::DirectRequest {
    lash::direct::DirectRequest::json_schema(
        "mock-model",
        format!(
            "Direct LLM client runtime perf turn {}. Return the benchmark marker.",
            turn_index + 1
        ),
        lash::direct::DirectJsonSchema {
            name: "runtime_perf_direct_completion".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "value", "error"],
                "properties": {
                    "kind": { "type": "string", "enum": ["value", "error"] },
                    "value": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "null" }
                        ]
                    },
                    "error": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "null" }
                        ]
                    }
                }
            })
            .into(),
            strict: true,
        },
    )
}

fn validate_direct_llm_response(turn_index: usize, response: &LlmResponse) -> anyhow::Result<()> {
    let value: serde_json::Value = serde_json::from_str(&response.full_text())
        .with_context(|| format!("parse direct_llm_client turn {} JSON", turn_index + 1))?;
    if value.get("value").and_then(serde_json::Value::as_str) == Some("runtime perf benchmark ok") {
        return Ok(());
    }
    anyhow::bail!(
        "runtime perf scenario direct_llm_client turn {} produced unexpected response: {}",
        turn_index + 1,
        response.full_text()
    );
}
