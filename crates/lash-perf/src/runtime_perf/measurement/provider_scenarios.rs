use super::*;

pub(super) async fn run_once_openai_responses_sse_parse(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::OpenAiResponsesSseParse;
    let mut run = RunRecorder::start(scenario, chat_turns);
    let payloads = run
        .build(async {
            Ok((0..chat_turns)
                .map(openai_responses_sse_payload)
                .collect::<Vec<_>>())
        })
        .await?;
    let payload_bytes = run
        .seed(async { Ok(payloads.iter().map(String::len).sum::<usize>()) })
        .await?;

    let mut parsed_parts = 0usize;
    for (turn_index, payload) in payloads.iter().enumerate() {
        run.turn(
            turn_index,
            async {
                let mut phase_profile = BTreeMap::new();

                let (state, parse_phase) =
                    measure_runtime_perf_phase("openai_responses_sse_parse.parse_payload", || {
                        let mut state =
                            lash_provider_openai::testing::ResponsesStreamParser::default();
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

                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
                        turn_usage: token_usage_from_llm_usage(state.usage()),
                        ..TurnTail::default()
                    },
                })
            },
            async {
                tokio::task::yield_now().await;
                Ok(())
            },
        )
        .await?;
    }

    run.export(async {
        let _export_shape = serde_json::json!({
            "payload_bytes": payload_bytes,
            "parsed_parts": parsed_parts,
        })
        .to_string();
        Ok(())
    })
    .await?;

    Ok(run.finish(RunTail {
        session_nodes: parsed_parts,
        active_path_messages: chat_turns,
        extra_counters: BTreeMap::from([
            ("payload_bytes".to_string(), payload_bytes as u64),
            ("parsed_parts".to_string(), parsed_parts as u64),
        ]),
        ..RunTail::default()
    }))
}

pub(super) async fn run_once_direct_llm_client(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::DirectLlmClient;
    let mut run = RunRecorder::start(scenario, chat_turns);
    let mut client = run
        .build(async {
            let provider =
                crate::runtime_perf::providers::benchmark_provider(scenario).into_handle();
            Ok(lash::direct::DirectLlmClient::new(provider))
        })
        .await?;
    run.seed(async { Ok(()) }).await?;

    let mut response_bytes = 0usize;
    for turn_index in 0..chat_turns {
        run.turn_then(
            turn_index,
            async {
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
                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        turn_usage: token_usage_from_llm_usage(&response.usage),
                        ..TurnTail::default()
                    },
                })
            },
            async {
                tokio::task::yield_now().await;
                Ok(())
            },
            |_, spans, tail| {
                tail.phase_profile.insert(
                    "direct_llm_client.complete".to_string(),
                    RuntimePerfPhaseRunResult {
                        samples: 1,
                        duration_ms: spans.run.duration_ms,
                        allocations: spans.run.allocations.clone(),
                        rss_growth_kb: diff_opt_i64(
                            spans.run.memory_before.rss_kb,
                            spans.run.memory_after.rss_kb,
                        ),
                    },
                );
                Ok(())
            },
        )
        .await?;
    }

    let turns_count = run.turns().len();
    run.export(async {
        let _export_shape = serde_json::json!({
            "response_bytes": response_bytes,
            "responses": turns_count,
        })
        .to_string();
        Ok(())
    })
    .await?;

    Ok(run.finish(RunTail {
        active_path_messages: chat_turns,
        extra_counters: BTreeMap::from([
            ("response_bytes".to_string(), response_bytes as u64),
            ("responses".to_string(), turns_count as u64),
        ]),
        ..RunTail::default()
    }))
}
