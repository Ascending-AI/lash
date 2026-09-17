use super::*;

pub(super) async fn run_once_openai_responses_sse_parse(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::OpenAiResponsesSseParse;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let payloads = (0..chat_turns)
        .map(openai_responses_sse_payload)
        .collect::<Vec<_>>();
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let payload_bytes = payloads.iter().map(String::len).sum::<usize>();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut parsed_parts = 0usize;
    for (turn_index, payload) in payloads.iter().enumerate() {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();

        let (state, parse_phase) =
            measure_runtime_perf_phase("openai_responses_sse_parse.parse_payload", || {
                let mut state = lash_provider_openai::testing::ResponsesStreamParser::default();
                state.parse_payload("OpenAI", payload)?;
                Ok(state)
            })?;
        phase_profile.insert(parse_phase.0, parse_phase.1);

        if !state.full_text().contains("runtime perf benchmark ok") {
            anyhow::bail!(
                "runtime perf scenario {} turn {} failed to parse benchmark marker",
                scenario.name(),
                turn_index + 1
            );
        }

        let (parts_len, parts_phase) =
            measure_runtime_perf_phase("openai_responses_sse_parse.project_parts", || {
                Ok(state.response_parts_len())
            })?;
        parsed_parts += parts_len;
        phase_profile.insert(parts_phase.0, parts_phase.1);

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
            stages: turn_stages(
                RuntimePerfStageRunResult::measured(
                    run_turn_ms,
                    run_turn_alloc,
                    after_turn_memory.rss_kb,
                ),
                Some(RuntimePerfStageRunResult::measured(
                    await_background_work_ms,
                    await_background_work_alloc,
                    after_await_memory.rss_kb,
                )),
                RuntimePerfStageRunResult::measured(
                    round3(run_turn_ms + await_background_work_ms),
                    turn_total_alloc,
                    after_await_memory.rss_kb,
                ),
            ),
            memory: memory_span(turn_before_memory, after_await_memory),
            phase_profile,
            turn_usage: token_usage_from_llm_usage(state.usage()),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let _export_shape = serde_json::json!({
        "payload_bytes": payload_bytes,
        "parsed_parts": parsed_parts,
    })
    .to_string();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        stages: run_stages(
            [
                (
                    stage::BUILD_RUNTIME,
                    RuntimePerfStageRunResult::measured(
                        build_runtime_ms,
                        build_runtime_alloc,
                        after_build_memory.rss_kb,
                    ),
                ),
                (
                    stage::SEED_STATE,
                    RuntimePerfStageRunResult::measured(
                        seed_state_ms,
                        seed_state_alloc,
                        after_seed_memory.rss_kb,
                    ),
                ),
                (
                    stage::EXPORT_STATE,
                    RuntimePerfStageRunResult::measured(
                        export_state_ms,
                        export_state_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
                (
                    stage::TOTAL,
                    RuntimePerfStageRunResult::measured(
                        elapsed_ms(total_started),
                        total_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
            ],
            &turns,
        ),
        session_nodes: parsed_parts,
        active_path_messages: chat_turns,
        extra_counters: BTreeMap::from([
            ("payload_bytes".to_string(), payload_bytes as u64),
            ("parsed_parts".to_string(), parsed_parts as u64),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms: BTreeMap::new(),
        memory: memory_span(before_memory, after_export_memory),
        phase_profile: sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turns,
        cumulative_usage: SessionUsageReport::default(),
    })
}

pub(super) async fn run_once_direct_llm_client(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::DirectLlmClient;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let provider = crate::runtime_perf::providers::benchmark_provider(scenario).into_handle();
    let mut client = lash::direct::DirectLlmClient::new(provider);
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut response_bytes = 0usize;
    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let response = runtime_perf_timed(
            scenario,
            turn_index,
            "direct_llm_client.complete",
            None,
            async {
                client
                    .complete(direct_llm_client_request(turn_index))
                    .await
                    .map_err(anyhow::Error::from)
            },
        )
        .await
        .with_context(|| {
            format!(
                "run runtime perf scenario {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        validate_direct_llm_response(turn_index, &response)?;
        response_bytes += response.full_text().len();
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

        let mut phase_profile = BTreeMap::new();
        phase_profile.insert(
            "direct_llm_client.complete".to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: run_turn_ms,
                allocations: run_turn_alloc.clone(),
                rss_growth_kb: diff_opt_i64(turn_before_memory.rss_kb, after_turn_memory.rss_kb),
            },
        );

        turns.push(RuntimePerfTurnResult {
            turn_index,
            stages: turn_stages(
                RuntimePerfStageRunResult::measured(
                    run_turn_ms,
                    run_turn_alloc,
                    after_turn_memory.rss_kb,
                ),
                Some(RuntimePerfStageRunResult::measured(
                    await_background_work_ms,
                    await_background_work_alloc,
                    after_await_memory.rss_kb,
                )),
                RuntimePerfStageRunResult::measured(
                    round3(run_turn_ms + await_background_work_ms),
                    turn_total_alloc,
                    after_await_memory.rss_kb,
                ),
            ),
            memory: memory_span(turn_before_memory, after_await_memory),
            phase_profile,
            turn_usage: token_usage_from_llm_usage(&response.usage),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let _export_shape = serde_json::json!({
        "response_bytes": response_bytes,
        "responses": turns.len(),
    })
    .to_string();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        stages: run_stages(
            [
                (
                    stage::BUILD_RUNTIME,
                    RuntimePerfStageRunResult::measured(
                        build_runtime_ms,
                        build_runtime_alloc,
                        after_build_memory.rss_kb,
                    ),
                ),
                (
                    stage::SEED_STATE,
                    RuntimePerfStageRunResult::measured(
                        seed_state_ms,
                        seed_state_alloc,
                        after_seed_memory.rss_kb,
                    ),
                ),
                (
                    stage::EXPORT_STATE,
                    RuntimePerfStageRunResult::measured(
                        export_state_ms,
                        export_state_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
                (
                    stage::TOTAL,
                    RuntimePerfStageRunResult::measured(
                        elapsed_ms(total_started),
                        total_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
            ],
            &turns,
        ),
        session_nodes: 0,
        active_path_messages: chat_turns,
        extra_counters: BTreeMap::from([
            ("response_bytes".to_string(), response_bytes as u64),
            ("responses".to_string(), turns.len() as u64),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms: BTreeMap::new(),
        memory: memory_span(before_memory, after_export_memory),
        phase_profile: sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turns,
        cumulative_usage: SessionUsageReport::default(),
    })
}
