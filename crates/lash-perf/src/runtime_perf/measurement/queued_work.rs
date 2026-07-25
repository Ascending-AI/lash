const QUEUED_WORK_JOIN_BATCHES_PER_TURN: usize = 32;
const QUEUED_WORK_SEED_OTHER_SESSION_BATCHES: usize = 64;
const QUEUED_WORK_CLAIM_TTL_MS: u64 = 30_000;
const TURN_INPUT_INGRESS_ACTIVE_PER_TURN: usize = 32;
const TURN_INPUT_INGRESS_ACCEPTED_PER_TURN: usize = 16;
const TURN_INPUT_INGRESS_NEXT_PER_TURN: usize = 8;

async fn run_once_queued_work_claim_stress(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::QueuedWorkClaimStress;
    let session_id = format!("runtime-perf-{}", scenario.name());
    let other_session_id = "runtime-perf-queued-work-other";
    let owner = lash_core::LeaseOwnerIdentity::opaque("runtime-perf", "queued-work-stress");
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let store = Arc::new(RuntimePerfStore::default());
    let mut runtime = build_runtime_with_store(scenario, Some(Arc::clone(&store)), None).await?;
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    for index in 0..QUEUED_WORK_SEED_OTHER_SESSION_BATCHES {
        store
            .enqueue_queued_work(
                QueuedWorkBatchDraft::new(
                    other_session_id,
                    DeliveryPolicy::EarliestSafeBoundary,
                    SlotPolicy::Exclusive,
                    vec![QueuedWorkPayload::session_command(
                        SessionCommand::RefreshToolCatalog {
                            reason: format!("other queued work {index}"),
                        },
                    )],
                )
                .with_source_key(format!("other:{index}")),
            )
            .await?;
    }
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut enqueued_batches = QUEUED_WORK_SEED_OTHER_SESSION_BATCHES;
    let mut command_claims = 0usize;
    let mut join_claims = 0usize;
    let mut join_batches_claimed = 0usize;
    let mut exclusive_claims = 0usize;
    let mut completed_batches = 0usize;

    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();

        let (lease, phase) =
            measure_runtime_perf_async_phase("queued_work.claim_session_lease", async {
                store
                    .try_claim_session_execution_lease(
                        &session_id,
                        &owner,
                        QUEUED_WORK_CLAIM_TTL_MS,
                    )
                    .await?
                    .acquired()
                    .ok_or_else(|| anyhow::anyhow!("queued-work stress lease was busy"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);

        let (_, phase) =
            measure_runtime_perf_async_phase("queued_work.enqueue_mixed_batch", async {
                enqueue_queued_work_stress_turn(store.as_ref(), &session_id, turn_index).await
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        enqueued_batches += QUEUED_WORK_JOIN_BATCHES_PER_TURN + 2;

        let (command_claim, phase) =
            measure_runtime_perf_async_phase("queued_work.claim_session_command", async {
                store
                    .claim_leading_ready_session_command(&session_id, &lease.fence(), &owner)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("queued-work stress expected command claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        command_claims += 1;
        if command_claim.batches.len() != 1 || command_claim.exclusive_session_command().is_none() {
            anyhow::bail!("queued-work stress command claim did not contain one session command");
        }

        let (_, phase) =
            measure_runtime_perf_async_phase("queued_work.complete_session_command", async {
                store
                    .commit_runtime_state(queued_work_stress_commit(
                        &session_id,
                        &lease,
                        vec![command_claim.completion()],
                        false,
                    ))
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        completed_batches += 1;

        let (join_claim, phase) =
            measure_runtime_perf_async_phase("queued_work.claim_join_turn_work", async {
                store
                    .claim_ready_queued_work(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        QueuedWorkClaimBoundary::Idle,
                        QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("queued-work stress expected join claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        if join_claim.batches.len() != QUEUED_WORK_JOIN_BATCHES_PER_TURN {
            anyhow::bail!(
                "queued-work stress expected {} joined batches, got {}",
                QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                join_claim.batches.len()
            );
        }
        join_claims += 1;
        join_batches_claimed += join_claim.batches.len();
        let join_batch_ids = join_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>();

        let (_, phase) =
            measure_runtime_perf_async_phase("queued_work.abandon_join_claim", async {
                store
                    .abandon_queued_work_claim(&join_claim)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);

        let (join_claim, phase) =
            measure_runtime_perf_async_phase("queued_work.reclaim_by_batch_ids", async {
                store
                    .claim_ready_queued_work_by_batch_ids(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        QueuedWorkClaimBoundary::Idle,
                        &join_batch_ids,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("queued-work stress expected exact reclaim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        if join_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .ne(join_batch_ids.iter().map(String::as_str))
        {
            anyhow::bail!("queued-work stress exact reclaim returned different batches");
        }

        let (_, phase) =
            measure_runtime_perf_async_phase("queued_work.complete_join_turn_work", async {
                store
                    .commit_runtime_state(queued_work_stress_commit(
                        &session_id,
                        &lease,
                        vec![join_claim.completion()],
                        false,
                    ))
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        completed_batches += join_claim.batches.len();

        let (exclusive_claim, phase) =
            measure_runtime_perf_async_phase("queued_work.claim_exclusive_turn_work", async {
                store
                    .claim_ready_queued_work(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        QueuedWorkClaimBoundary::Idle,
                        QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("queued-work stress expected exclusive claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        if exclusive_claim.batches.len() != 1 {
            anyhow::bail!(
                "queued-work stress expected one exclusive batch, got {}",
                exclusive_claim.batches.len()
            );
        }
        exclusive_claims += 1;

        let (_, phase) =
            measure_runtime_perf_async_phase("queued_work.complete_exclusive_turn_work", async {
                store
                    .commit_runtime_state(queued_work_stress_commit(
                        &session_id,
                        &lease,
                        vec![exclusive_claim.completion()],
                        true,
                    ))
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        completed_batches += 1;

        let (pending, phase) =
            measure_runtime_perf_async_phase("queued_work.list_pending", async {
                store
                    .list_pending_queued_work(&session_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        if !pending.is_empty() {
            anyhow::bail!(
                "queued-work stress left {} pending batches for measured session",
                pending.len()
            );
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
    let remaining_measured = store.list_queued_work(&session_id).await?.len();
    let remaining_other = store.list_queued_work(other_session_id).await?.len();
    let _export_shape = serde_json::json!({
        "enqueued_batches": enqueued_batches,
        "completed_batches": completed_batches,
        "remaining_measured_batches": remaining_measured,
        "remaining_other_batches": remaining_other,
    })
    .to_string();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);
    runtime.close().await?;

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
        session_nodes: enqueued_batches,
        active_path_messages: completed_batches,
        extra_counters: BTreeMap::from([
            ("enqueued_batches".to_string(), enqueued_batches as u64),
            ("command_claims".to_string(), command_claims as u64),
            ("join_claims".to_string(), join_claims as u64),
            (
                "join_batches_claimed".to_string(),
                join_batches_claimed as u64,
            ),
            ("exclusive_claims".to_string(), exclusive_claims as u64),
            ("completed_batches".to_string(), completed_batches as u64),
            (
                "remaining_measured_batches".to_string(),
                remaining_measured as u64,
            ),
            (
                "remaining_other_batches".to_string(),
                remaining_other as u64,
            ),
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

async fn enqueue_queued_work_stress_turn(
    store: &RuntimePerfStore,
    session_id: &str,
    turn_index: usize,
) -> anyhow::Result<()> {
    store
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                vec![QueuedWorkPayload::session_command(
                    SessionCommand::RefreshToolCatalog {
                        reason: format!("queued-work-stress-{turn_index}"),
                    },
                )],
            )
            .with_source_key(format!("command:{turn_index}")),
        )
        .await?;

    for batch_index in 0..QUEUED_WORK_JOIN_BATCHES_PER_TURN {
        store
            .enqueue_queued_work(
                QueuedWorkBatchDraft::new(
                    session_id,
                    DeliveryPolicy::EarliestSafeBoundary,
                    SlotPolicy::Join,
                    vec![QueuedWorkPayload::process_wake(queued_work_stress_wake(
                        session_id,
                        &format!("queued work stress turn {turn_index} batch {batch_index}"),
                        (turn_index * QUEUED_WORK_JOIN_BATCHES_PER_TURN + batch_index + 1) as u64,
                    ))],
                )
                .with_source_key(format!("turn:{turn_index}:join:{batch_index}")),
            )
            .await?;
    }

    store
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                vec![QueuedWorkPayload::process_wake(queued_work_stress_wake(
                    session_id,
                    &format!("queued work stress exclusive {turn_index}"),
                    ((turn_index + 1) * 10_000) as u64,
                ))],
            )
            .with_source_key(format!("turn:{turn_index}:exclusive")),
        )
        .await?;
    Ok(())
}

fn queued_work_stress_wake(
    session_id: &str,
    input: &str,
    sequence: u64,
) -> lash_core::ProcessWakeDelivery {
    let process_id = format!("runtime-perf-process-{sequence}");
    lash_core::ProcessWakeDelivery {
        wake_id: format!("wake:{session_id}:{sequence}"),
        target_session_id: session_id.to_string(),
        target_scope_id: lash_core::SessionScopeId::new(format!("session:{session_id}")),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        dedupe_key: format!("wake:{session_id}:{process_id}:{sequence}"),
        input: input.to_string(),
        created_at_ms: sequence,
    }
}

async fn run_once_turn_input_ingress_interrupt(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::TurnInputIngressInterrupt;
    let session_id = format!("runtime-perf-{}", scenario.name());
    let other_session_id = "runtime-perf-turn-input-other";
    let owner = lash_core::LeaseOwnerIdentity::opaque("runtime-perf", "turn-input-ingress");
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let store = Arc::new(RuntimePerfStore::default());
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    for index in 0..QUEUED_WORK_SEED_OTHER_SESSION_BATCHES {
        store
            .enqueue_pending_turn_input(
                lash_core::PendingTurnInputDraft::new(
                    other_session_id,
                    lash_core::TurnInputIngress::NextTurn,
                    TurnInput::text(format!("other pending turn input {index}")),
                )
                .with_source_key(format!("other:{index}")),
            )
            .await?;
    }
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut active_enqueued = 0usize;
    let mut next_enqueued = 0usize;
    let mut active_claims = 0usize;
    let mut next_claims = 0usize;
    let mut abandoned_claims = 0usize;
    let mut completed_inputs = 0usize;
    let mut deferred_inputs = 0usize;

    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();
        let turn_id = format!("turn-input-ingress-{turn_index}");

        let (lease, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.claim_session_lease", async {
                store
                    .try_claim_session_execution_lease(
                        &session_id,
                        &owner,
                        QUEUED_WORK_CLAIM_TTL_MS,
                    )
                    .await?
                    .acquired()
                    .ok_or_else(|| anyhow::anyhow!("turn-input ingress lease was busy"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);

        let (_, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.enqueue_active", async {
                for input_index in 0..TURN_INPUT_INGRESS_ACTIVE_PER_TURN {
                    let mut input = TurnInput::text(format!(
                        "active steer turn {turn_index} input {input_index}"
                    ));
                    if input_index == 0 {
                        input = input.with_attachment(lash_core::AttachmentSource::inline(
                            lash_core::MediaType::parse("image/png").unwrap(),
                            vec![1, 2, 3, turn_index as u8],
                        ));
                    }
                    store
                        .enqueue_pending_turn_input(
                            lash_core::PendingTurnInputDraft::new(
                                &session_id,
                                lash_core::TurnInputIngress::active_turn(
                                    &turn_id,
                                    lash_core::TurnInputCheckpointBoundary::AfterWork,
                                ),
                                input,
                            )
                            .with_source_key(format!("active:{turn_index}:{input_index}")),
                        )
                        .await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        active_enqueued += TURN_INPUT_INGRESS_ACTIVE_PER_TURN;

        let (_, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.enqueue_next", async {
                for input_index in 0..TURN_INPUT_INGRESS_NEXT_PER_TURN {
                    let mut input =
                        TurnInput::text(format!("queued next turn {turn_index} input {input_index}"));
                    if input_index == 0 {
                        input = input.with_attachment(lash_core::AttachmentSource::inline(
                            lash_core::MediaType::parse("image/png").unwrap(),
                            vec![4, 5, 6, turn_index as u8],
                        ));
                    }
                    store
                        .enqueue_pending_turn_input(
                            lash_core::PendingTurnInputDraft::new(
                                &session_id,
                                lash_core::TurnInputIngress::NextTurn,
                                input,
                            )
                            .with_source_key(format!("next:{turn_index}:{input_index}")),
                        )
                        .await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        next_enqueued += TURN_INPUT_INGRESS_NEXT_PER_TURN;

        let (initial_active_claim, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.claim_active_initial", async {
                store
                    .claim_active_turn_inputs(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        &turn_id,
                        lash_core::CheckpointKind::AfterWork,
                        1,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("expected initial active input claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        active_claims += 1;

        let (_, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.abandon_active_claim", async {
                store
                    .abandon_turn_input_claim(&initial_active_claim)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        abandoned_claims += 1;

        let (active_claim, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.reclaim_active_inputs", async {
                store
                    .claim_active_turn_inputs(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        &turn_id,
                        lash_core::CheckpointKind::AfterWork,
                        TURN_INPUT_INGRESS_ACCEPTED_PER_TURN,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("expected active input claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        active_claims += 1;
        if active_claim.inputs.len() != TURN_INPUT_INGRESS_ACCEPTED_PER_TURN {
            anyhow::bail!(
                "turn-input ingress expected {} active inputs, got {}",
                TURN_INPUT_INGRESS_ACCEPTED_PER_TURN,
                active_claim.inputs.len()
            );
        }
        let active_turn_input = active_claim.materialize_for_turn();
        // Position-independent on purpose: a claim aggregates many inputs and
        // only the first carries the attachment, so the attachment is not the
        // last item. Assert survival, not placement.
        if !active_turn_input.items.iter().any(|item| {
            matches!(
                item,
                lash_core::InputItem::Attachment {
                    source: lash_core::AttachmentSource::Inline { bytes, .. }
                } if bytes == &vec![1, 2, 3, turn_index as u8]
            )
        }) {
            anyhow::bail!("turn-input ingress active claim lost attachment bytes");
        }

        let (_, phase) = measure_runtime_perf_async_phase(
            "turn_input_ingress.complete_active_and_defer",
            async {
                let state = RuntimeSessionState {
                    session_id: session_id.clone(),
                    ..RuntimeSessionState::default()
                };
                store
                    .commit_runtime_state(
                        RuntimeCommit::persisted_state(&state, &[])
                            .with_session_execution_lease(lease.fence())
                            .completing_turn_input_claim(active_claim.completion())
                            .deferring_interrupted_turn_inputs(turn_id.clone()),
                    )
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            },
        )
        .await?;
        phase_profile.insert(phase.0, phase.1);
        completed_inputs += TURN_INPUT_INGRESS_ACCEPTED_PER_TURN;
        deferred_inputs +=
            TURN_INPUT_INGRESS_ACTIVE_PER_TURN - TURN_INPUT_INGRESS_ACCEPTED_PER_TURN;

        let (next_claim, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.claim_next_turn_inputs", async {
                store
                    .claim_next_turn_inputs(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        TURN_INPUT_INGRESS_ACTIVE_PER_TURN + TURN_INPUT_INGRESS_NEXT_PER_TURN,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("expected next-turn input claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        next_claims += 1;
        let expected_next =
            TURN_INPUT_INGRESS_ACTIVE_PER_TURN - TURN_INPUT_INGRESS_ACCEPTED_PER_TURN
                + TURN_INPUT_INGRESS_NEXT_PER_TURN;
        if next_claim.inputs.len() != expected_next {
            anyhow::bail!(
                "turn-input ingress expected {expected_next} next-turn inputs, got {}",
                next_claim.inputs.len()
            );
        }

        let (_, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.abandon_next_claim", async {
                store
                    .abandon_turn_input_claim(&next_claim)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        abandoned_claims += 1;

        let (next_claim, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.reclaim_next_turn_inputs", async {
                store
                    .claim_next_turn_inputs(
                        &session_id,
                        &lease.fence(),
                        &owner,
                        TURN_INPUT_INGRESS_ACTIVE_PER_TURN + TURN_INPUT_INGRESS_NEXT_PER_TURN,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("expected reclaimed next-turn input claim"))
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        next_claims += 1;
        let next_turn_input = next_claim.materialize_for_turn();
        // Position-independent: see the active-claim assertion above.
        if !next_turn_input.items.iter().any(|item| {
            matches!(
                item,
                lash_core::InputItem::Attachment {
                    source: lash_core::AttachmentSource::Inline { bytes, .. }
                } if bytes == &vec![4, 5, 6, turn_index as u8]
            )
        }) {
            anyhow::bail!("turn-input ingress next claim lost attachment bytes");
        }

        let (_, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.complete_next_turn_inputs", async {
                let state = RuntimeSessionState {
                    session_id: session_id.clone(),
                    ..RuntimeSessionState::default()
                };
                store
                    .commit_runtime_state(
                        RuntimeCommit::persisted_state(&state, &[])
                            .with_session_execution_lease(lease.fence())
                            .releasing_session_execution_lease(lease.completion())
                            .completing_turn_input_claim(next_claim.completion()),
                    )
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        completed_inputs += next_claim.inputs.len();

        let (pending, phase) =
            measure_runtime_perf_async_phase("turn_input_ingress.list_pending", async {
                store
                    .list_pending_turn_inputs(&session_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        if !pending.is_empty() {
            anyhow::bail!(
                "turn-input ingress left {} pending inputs for measured session",
                pending.len()
            );
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
    let remaining_measured = store.list_pending_turn_inputs(&session_id).await?.len();
    let remaining_other = store
        .list_pending_turn_inputs(other_session_id)
        .await?
        .len();
    let _export_shape = serde_json::json!({
        "active_enqueued": active_enqueued,
        "next_enqueued": next_enqueued,
        "completed_inputs": completed_inputs,
        "deferred_inputs": deferred_inputs,
        "remaining_measured_inputs": remaining_measured,
        "remaining_other_inputs": remaining_other,
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
        session_nodes: active_enqueued + next_enqueued,
        active_path_messages: completed_inputs,
        extra_counters: BTreeMap::from([
            ("active_enqueued".to_string(), active_enqueued as u64),
            ("next_enqueued".to_string(), next_enqueued as u64),
            ("active_claims".to_string(), active_claims as u64),
            ("next_claims".to_string(), next_claims as u64),
            ("abandoned_claims".to_string(), abandoned_claims as u64),
            ("completed_inputs".to_string(), completed_inputs as u64),
            ("deferred_inputs".to_string(), deferred_inputs as u64),
            (
                "remaining_measured_inputs".to_string(),
                remaining_measured as u64,
            ),
            ("remaining_other_inputs".to_string(), remaining_other as u64),
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

fn queued_work_stress_commit(
    session_id: &str,
    lease: &SessionExecutionLease,
    completed_queue_claims: Vec<QueuedWorkCompletion>,
    release_lease: bool,
) -> RuntimeCommit {
    RuntimeCommit {
        session_id: session_id.to_string(),
        expected_head_revision: None,
        session_execution_lease: Some(lease.fence()),
        release_session_execution_lease: release_lease.then(|| lease.completion()),
        config: PersistedSessionConfig::default(),
        agent_frames: Vec::new(),
        current_agent_frame_id: String::new(),
        graph: GraphCommitDelta::Unchanged { leaf_node_id: None },
        checkpoint: HydratedSessionCheckpoint::default(),
        usage_deltas: Vec::new(),
        turn_commit: None,
        completed_queue_claims,
        completed_turn_input_claims: Vec::new(),
        enqueued_queue_batches: Vec::new(),
        interrupted_turn_input_turn_id: None,
        committed_attachment_ids: Vec::new(),
    }
}
