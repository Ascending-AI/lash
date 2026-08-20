
const LIVE_REPLAY_EVENTS_PER_TURN: usize = 96;
const LIVE_REPLAY_MAIN_CAPACITY: usize = 256;
const LIVE_REPLAY_TRIM_CAPACITY: usize = 8;

async fn run_once_live_replay_pressure(chat_turns: usize) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::LiveReplayPressure;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let store = lash_core::facade_support::InMemoryLiveReplayStore::with_bounds(
        LIVE_REPLAY_MAIN_CAPACITY,
        Duration::from_secs(120),
    );
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut appended_events = 0usize;
    let mut replayed_events = 0usize;
    let mut subscribed_buffered_events = 0usize;
    let mut subscribed_live_events = 0usize;
    let mut trim_gaps = 0usize;
    let mut unavailable_gaps = 0usize;

    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();
        let session_id = format!("runtime-perf-live-replay-{turn_index}");
        let revision = SessionRevision::new(turn_index as u64 + 1);
        let turn_id = format!("turn-{turn_index}");
        let start_cursor = store.current_cursor(&session_id, revision);

        let (first_cursor, append_phase) =
            measure_runtime_perf_phase("live_replay.append", || {
                let mut first_cursor = None;
                for event_index in 0..LIVE_REPLAY_EVENTS_PER_TURN {
                    let event = store.append(
                        &session_id,
                        revision,
                        Some(&turn_id),
                        live_replay_text_payload(format!("turn-{turn_index}-event-{event_index}")),
                    )?;
                    if first_cursor.is_none() {
                        first_cursor = Some(event.cursor.clone());
                    }
                }
                first_cursor.ok_or_else(|| anyhow::anyhow!("live replay append produced no cursor"))
            })?;
        appended_events += LIVE_REPLAY_EVENTS_PER_TURN;
        phase_profile.insert(append_phase.0, append_phase.1);

        let (current_cursor, current_phase) =
            measure_runtime_perf_phase("live_replay.current_cursor_parse", || {
                let cursor = store.current_cursor(&session_id, revision);
                match store.replay_after_cursor(&cursor)? {
                    LiveReplayOutcome::Replayed(events) if events.is_empty() => Ok(cursor),
                    LiveReplayOutcome::Replayed(events) => anyhow::bail!(
                        "current cursor replay unexpectedly returned {} events",
                        events.len()
                    ),
                    LiveReplayOutcome::Gap(reason) => {
                        anyhow::bail!("current cursor replay returned gap {reason:?}")
                    }
                }
            })?;
        phase_profile.insert(current_phase.0, current_phase.1);

        let (replay_count, replay_phase) =
            measure_runtime_perf_phase("live_replay.replay_after_cursor", || {
                match store.replay_after_cursor(&start_cursor)? {
                    LiveReplayOutcome::Replayed(events) => Ok(events.len()),
                    LiveReplayOutcome::Gap(reason) => {
                        anyhow::bail!("start cursor replay returned gap {reason:?}")
                    }
                }
            })?;
        if replay_count != LIVE_REPLAY_EVENTS_PER_TURN {
            anyhow::bail!(
                "live replay expected {} replayed events, got {replay_count}",
                LIVE_REPLAY_EVENTS_PER_TURN
            );
        }
        replayed_events += replay_count;
        phase_profile.insert(replay_phase.0, replay_phase.1);

        let ((buffered_count, live_count), subscribe_phase) =
            measure_runtime_perf_async_phase("live_replay.subscribe_buffered", async {
                let mut subscription = match store.subscribe_after_cursor(&first_cursor)? {
                    LiveReplaySubscribeOutcome::Subscribed(subscription) => subscription,
                    LiveReplaySubscribeOutcome::Gap(reason) => {
                        anyhow::bail!("subscribe after first cursor returned gap {reason:?}")
                    }
                };
                let mut buffered_count = 0usize;
                for _ in 1..LIVE_REPLAY_EVENTS_PER_TURN {
                    tokio::time::timeout(
                        Duration::from_secs(1),
                        futures_util::StreamExt::next(&mut subscription),
                    )
                        .await
                        .context("timed out reading buffered live replay event")?
                        .context("live replay subscription closed")??;
                    buffered_count += 1;
                }
                store.append(
                    &session_id,
                    revision,
                    Some(&turn_id),
                    live_replay_text_payload(format!("turn-{turn_index}-live-event")),
                )?;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    futures_util::StreamExt::next(&mut subscription),
                )
                    .await
                    .context("timed out reading live replay event")?
                    .context("live replay subscription closed")??;
                Ok((buffered_count, 1usize))
            })
            .await?;
        subscribed_buffered_events += buffered_count;
        subscribed_live_events += live_count;
        phase_profile.insert(subscribe_phase.0, subscribe_phase.1);

        let (trim_gap_count, trim_phase) =
            measure_runtime_perf_phase("live_replay.trim_by_capacity", || {
                let trim_store = lash_core::facade_support::InMemoryLiveReplayStore::with_bounds(
                    LIVE_REPLAY_TRIM_CAPACITY,
                    Duration::from_secs(120),
                );
                let trim_session_id = format!("runtime-perf-live-replay-trim-{turn_index}");
                let trim_turn_id = format!("trim-turn-{turn_index}");
                let trim_start = trim_store.current_cursor(&trim_session_id, revision);
                for event_index in 0..(LIVE_REPLAY_TRIM_CAPACITY * 3) {
                    trim_store.append(
                        &trim_session_id,
                        revision,
                        Some(&trim_turn_id),
                        live_replay_text_payload(format!("trim-{turn_index}-{event_index}")),
                    )?;
                }
                trim_store.trim_session(&trim_session_id)?;
                match trim_store.replay_after_cursor(&trim_start)? {
                    LiveReplayOutcome::Gap(lash_core::LiveReplayGapReason::Trimmed) => Ok(1usize),
                    LiveReplayOutcome::Gap(reason) => {
                        anyhow::bail!("capacity trim returned wrong gap {reason:?}")
                    }
                    LiveReplayOutcome::Replayed(events) => anyhow::bail!(
                        "capacity trim expected gap, got {} replayed events",
                        events.len()
                    ),
                }
            })?;
        trim_gaps += trim_gap_count;
        phase_profile.insert(trim_phase.0, trim_phase.1);

        let (unavailable_gap_count, gap_phase) =
            measure_runtime_perf_phase("live_replay.gap_handling", || {
                let ahead_cursor: lash_core::SessionCursor =
                    serde_json::from_value(serde_json::json!(format!(
                        "lashsc1:{}:999999:{}",
                        revision.as_u64(),
                        session_id
                    )))?;
                let mut gaps = 0usize;
                match store.replay_after_cursor(&ahead_cursor)? {
                    LiveReplayOutcome::Gap(lash_core::LiveReplayGapReason::Unavailable) => gaps += 1,
                    LiveReplayOutcome::Gap(reason) => {
                        anyhow::bail!("ahead replay returned wrong gap {reason:?}")
                    }
                    LiveReplayOutcome::Replayed(events) => anyhow::bail!(
                        "ahead replay expected gap, got {} replayed events",
                        events.len()
                    ),
                }
                match store.subscribe_after_cursor(&ahead_cursor)? {
                    LiveReplaySubscribeOutcome::Gap(lash_core::LiveReplayGapReason::Unavailable) => {
                        gaps += 1
                    }
                    LiveReplaySubscribeOutcome::Gap(reason) => {
                        anyhow::bail!("ahead subscribe returned wrong gap {reason:?}")
                    }
                    LiveReplaySubscribeOutcome::Subscribed(_) => {
                        anyhow::bail!("ahead subscribe expected gap")
                    }
                }
                Ok(gaps)
            })?;
        unavailable_gaps += unavailable_gap_count;
        phase_profile.insert(gap_phase.0, gap_phase.1);

        match store.replay_after_cursor(&current_cursor)? {
            LiveReplayOutcome::Replayed(events) if events.len() == 1 => {}
            LiveReplayOutcome::Replayed(events) => anyhow::bail!(
                "current cursor should see only the live event after subscribe, got {}",
                events.len()
            ),
            LiveReplayOutcome::Gap(reason) => {
                anyhow::bail!("current cursor after live append returned gap {reason:?}")
            }
        }

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
        "appended_events": appended_events,
        "replayed_events": replayed_events,
        "subscribed_buffered_events": subscribed_buffered_events,
        "subscribed_live_events": subscribed_live_events,
        "trim_gaps": trim_gaps,
        "unavailable_gaps": unavailable_gaps,
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
        session_nodes: appended_events,
        active_path_messages: replayed_events,
        extra_counters: BTreeMap::from([
            ("appended_events".to_string(), appended_events as u64),
            ("replayed_events".to_string(), replayed_events as u64),
            (
                "subscribed_buffered_events".to_string(),
                subscribed_buffered_events as u64,
            ),
            (
                "subscribed_live_events".to_string(),
                subscribed_live_events as u64,
            ),
            ("trim_gaps".to_string(), trim_gaps as u64),
            ("unavailable_gaps".to_string(), unavailable_gaps as u64),
        ]),
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

async fn run_once_trace_jsonl(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let trace_root = make_temp_bench_dir("lash-runtime-perf-trace-jsonl")?;
    let trace_path = trace_root.join("runtime-trace.jsonl");
    let lashlang_trace_path = matches!(scenario, RuntimePerfScenario::TraceJsonlExtended)
        .then(|| trace_root.join("lashlang-execution.jsonl"));
    let trace_config = RuntimePerfTraceConfig {
        trace_jsonl_path: Some(trace_path.clone()),
        lashlang_execution_jsonl_path: lashlang_trace_path.clone(),
        trace_level: if matches!(scenario, RuntimePerfScenario::TraceJsonlExtended) {
            lash::tracing::TraceLevel::Extended
        } else {
            lash::tracing::TraceLevel::Standard
        },
    };
    let mut runtime = build_runtime_with_store(scenario, None, Some(trace_config)).await?;
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    seed_runtime_state(&mut runtime, scenario).await?;
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    for turn_index in 0..chat_turns {
        prepare_turn(&mut runtime, scenario, turn_index).await?;

        let phase_probe = Arc::new(RuntimePerfPhaseProbe::default());
        runtime.set_turn_phase_probe(phase_probe.clone()).await;

        let before_turn_usage = runtime.usage_report();
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let turn_input = TurnInput {
            items: vec![InputItem::Text {
                text: benchmark_prompt(scenario, turn_index),
            }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        };
        let cancel = CancellationToken::new();
        let turn = runtime_perf_timed(
            scenario,
            turn_index,
            "run_turn",
            Some(cancel.clone()),
            runtime.run_turn(turn_input, cancel),
        )
        .await
        .with_context(|| {
            format!(
                "run runtime perf scenario {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        validate_runtime_perf_turn(scenario, turn_index, &turn)?;
        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();

        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        runtime_perf_timed(
            scenario,
            turn_index,
            "await_background_work",
            None,
            runtime.await_background_work(),
        )
        .await
        .with_context(|| {
            format!(
                "await background work for {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let turn_total_alloc =
            sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);

        let cumulative_usage = runtime.usage_report();
        let usage_delta_entries =
            lash_core::facade_support::diff_usage_reports(&before_turn_usage, &cumulative_usage)
                .map_err(anyhow::Error::msg)?;
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
            phase_profile: phase_probe.take_completed(),
            turn_usage: turn.usage,
            usage_delta: SessionUsageReport::from_entries(&usage_delta_entries),
            cumulative_usage,
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let state = runtime.export_state().await;
    let cumulative_usage = runtime.usage_report();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let (trace_counters, inspect_phase) =
        measure_runtime_perf_phase("trace_jsonl.inspect_files", || {
            inspect_trace_jsonl_files(&trace_path, lashlang_trace_path.as_deref())
        })?;
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);
    let mut phase_profile = sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile));
    phase_profile.insert(inspect_phase.0, inspect_phase.1);
    runtime.close().await?;
    let _ = std::fs::remove_dir_all(trace_root);

    if trace_counters
        .get("trace_records")
        .copied()
        .unwrap_or_default()
        == 0
    {
        anyhow::bail!("trace_jsonl scenario produced no runtime trace records");
    }
    if matches!(scenario, RuntimePerfScenario::TraceJsonlExtended)
        && trace_counters
            .get("lashlang_execution_trace_records")
            .copied()
            .unwrap_or_default()
            == 0
    {
        anyhow::bail!("extended trace_jsonl scenario produced no Lashlang execution records");
    }

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
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state.read_view().messages().len(),
        extra_counters: trace_counters,
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
        phase_profile,
        turns,
        cumulative_usage,
    })
}

fn live_replay_text_payload(text: impl Into<String>) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(lash_core::TurnActivity::independent(
        lash_core::TurnEvent::AssistantProseDelta {
            text: text.into().into(),
        },
    ))
}

fn inspect_trace_jsonl_files(
    trace_path: &std::path::Path,
    lashlang_trace_path: Option<&std::path::Path>,
) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut counters = BTreeMap::new();
    let (trace_bytes, trace_records) = jsonl_file_stats(trace_path)?;
    counters.insert("trace_bytes".to_string(), trace_bytes);
    counters.insert("trace_records".to_string(), trace_records);
    if let Some(path) = lashlang_trace_path {
        let (lashlang_bytes, lashlang_records) = jsonl_file_stats(path)?;
        counters.insert("lashlang_execution_trace_bytes".to_string(), lashlang_bytes);
        counters.insert(
            "lashlang_execution_trace_records".to_string(),
            lashlang_records,
        );
    }
    Ok(counters)
}

fn jsonl_file_stats(path: &std::path::Path) -> anyhow::Result<(u64, u64)> {
    let bytes = std::fs::metadata(path)
        .with_context(|| format!("stat trace file {}", path.display()))?
        .len();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read trace file {}", path.display()))?;
    let records = text.lines().filter(|line| !line.trim().is_empty()).count() as u64;
    Ok((bytes, records))
}
