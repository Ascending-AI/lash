const HARDENING_IDENTITY_ITERATIONS: usize = 64;
const HARDENING_OCCURRENCE_ITERATIONS: usize = 256;
const HARDENING_PRUNE_BATCH: usize = 16;
const HARDENING_LEASE_TTL_MS: u64 = 60_000;

#[derive(Clone, Copy)]
struct StoreHardeningPhaseNames {
    claim_session_lease: &'static str,
    claim_queued_work: &'static str,
    complete_queued_work: &'static str,
    attachment_intent: &'static str,
    attachment_adopt: &'static str,
    append_receipt_usage_fresh: &'static str,
    append_receipt_replay: &'static str,
}

const MEMORY_HARDENING_PHASES: StoreHardeningPhaseNames = StoreHardeningPhaseNames {
    claim_session_lease: "store_hardening.memory.claim_session_lease",
    claim_queued_work: "store_hardening.memory.claim_queued_work",
    complete_queued_work: "store_hardening.memory.complete_queued_work",
    attachment_intent: "store_hardening.memory.attachment_intent",
    attachment_adopt: "store_hardening.memory.attachment_adopt",
    append_receipt_usage_fresh: "store_hardening.memory.append_receipt_usage_fresh",
    append_receipt_replay: "store_hardening.memory.append_receipt_replay",
};

const SQLITE_HARDENING_PHASES: StoreHardeningPhaseNames = StoreHardeningPhaseNames {
    claim_session_lease: "store_hardening.sqlite.claim_session_lease",
    claim_queued_work: "store_hardening.sqlite.claim_queued_work",
    complete_queued_work: "store_hardening.sqlite.complete_queued_work",
    attachment_intent: "store_hardening.sqlite.attachment_intent",
    attachment_adopt: "store_hardening.sqlite.attachment_adopt",
    append_receipt_usage_fresh: "store_hardening.sqlite.append_receipt_usage_fresh",
    append_receipt_replay: "store_hardening.sqlite.append_receipt_replay",
};

const POSTGRES_HARDENING_PHASES: StoreHardeningPhaseNames = StoreHardeningPhaseNames {
    claim_session_lease: "store_hardening.postgres.claim_session_lease",
    claim_queued_work: "store_hardening.postgres.claim_queued_work",
    complete_queued_work: "store_hardening.postgres.complete_queued_work",
    attachment_intent: "store_hardening.postgres.attachment_intent",
    attachment_adopt: "store_hardening.postgres.attachment_adopt",
    append_receipt_usage_fresh: "store_hardening.postgres.append_receipt_usage_fresh",
    append_receipt_replay: "store_hardening.postgres.append_receipt_replay",
};

async fn run_once_store_hardening_hot_paths(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::StoreHardeningHotPaths;
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let postgres_url = std::env::var("LASH_POSTGRES_DATABASE_URL").map_err(|_| ());
    let postgres_url = postgres_url
        .or_else(|_| std::env::var("DATABASE_URL").map_err(|_| ()))
        .map_err(|_| {
            anyhow::anyhow!(
                "{} requires LASH_POSTGRES_DATABASE_URL or DATABASE_URL (the full perf workflows provide it)",
                scenario.name()
            )
        })?;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let memory_factory = lash::persistence::InMemorySessionStoreFactory::new();
    let sqlite_root = make_temp_bench_dir("lash-runtime-perf-store-hardening")?;
    let sqlite_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(&sqlite_root);
    let postgres = lash_postgres_store::PostgresStorage::connect_with(
        &postgres_url,
        lash_postgres_store::PostgresStoreConfig {
            min_connections: 1,
            max_connections: 4,
            ..lash_postgres_store::PostgresStoreConfig::default()
        },
    )
    .await?;
    let postgres_factory = postgres.session_store_factory_with_shared_process_registry();

    let memory_session_id = format!("perf-hardening-memory-{run_id}");
    let sqlite_session_id = format!("perf-hardening-sqlite-{run_id}");
    let postgres_session_id = format!("perf-hardening-postgres-{run_id}");
    let memory_store = memory_factory
        .create_store(&store_hardening_create_request(&memory_session_id))
        .await?;
    let sqlite_store = sqlite_factory
        .create_store(&store_hardening_create_request(&sqlite_session_id))
        .await?;
    let postgres_store = postgres_factory
        .create_store(&store_hardening_create_request(&postgres_session_id))
        .await?;

    let memory_registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let sqlite_registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &sqlite_root.join("process-registry.sqlite"),
            sqlite_root.join("process-sessions"),
        )
        .await?,
    );
    let postgres_registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(postgres.process_registry());
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    // Warm the existing pool once. Timed open arms below share this pool so
    // their difference isolates structural verification rather than TCP/TLS.
    let preverified = lash_postgres_store::PostgresStorage::from_preverified_pool_for_testing(
        postgres.pool().clone(),
    )
    .await?;
    drop(preverified);
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let owner = lash_core::LeaseOwnerIdentity::opaque("lash-perf", &run_id);
    let mut turns = Vec::with_capacity(chat_turns);
    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();

        measure_hardening_identity_phases(turn_index, &mut phase_profile)?;

        let (_, phase) =
            measure_runtime_perf_async_phase("store_hardening.postgres.open_preverified", async {
                lash_postgres_store::PostgresStorage::from_preverified_pool_for_testing(
                    postgres.pool().clone(),
                )
                .await
                .map(drop)
                .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);

        let (_, phase) =
            measure_runtime_perf_async_phase("store_hardening.postgres.open_enforce", async {
                // Deliberately HostProvisioned: this arm measures the structural
                // verification gate without LashManaged's version preflight and
                // idempotent DDL. The report calls out that narrower open mode.
                lash_postgres_store::PostgresStorage::from_pool_with(
                    postgres.pool().clone(),
                    lash_postgres_store::PostgresStoreConfig {
                        schema_provisioning:
                            lash_postgres_store::SchemaProvisioning::HostProvisioned,
                        schema_check: lash_postgres_store::SchemaCheck::Enforce,
                        ..lash_postgres_store::PostgresStoreConfig::default()
                    },
                )
                .await
                .map(drop)
                .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);

        phase_profile.extend(
            measure_store_hardening_backend_turn(
                &memory_store,
                &memory_session_id,
                &owner,
                turn_index,
                MEMORY_HARDENING_PHASES,
            )
            .await?,
        );
        phase_profile.extend(
            measure_store_hardening_backend_turn(
                &sqlite_store,
                &sqlite_session_id,
                &owner,
                turn_index,
                SQLITE_HARDENING_PHASES,
            )
            .await?,
        );
        phase_profile.extend(
            measure_store_hardening_backend_turn(
                &postgres_store,
                &postgres_session_id,
                &owner,
                turn_index,
                POSTGRES_HARDENING_PHASES,
            )
            .await?,
        );

        measure_process_prune(
            &memory_registry,
            "store_hardening.memory.prune_terminal_processes",
            "memory",
            &run_id,
            turn_index,
            &mut phase_profile,
        )
        .await?;
        measure_process_prune(
            &sqlite_registry,
            "store_hardening.sqlite.prune_terminal_processes",
            "sqlite",
            &run_id,
            turn_index,
            &mut phase_profile,
        )
        .await?;
        measure_process_prune(
            &postgres_registry,
            "store_hardening.postgres.prune_terminal_processes",
            "postgres",
            &run_id,
            turn_index,
            &mut phase_profile,
        )
        .await?;

        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();
        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        tokio::task::yield_now().await;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let total_alloc = sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);
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
                total: total_alloc,
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
        "backends": 3,
        "identity_iterations": HARDENING_IDENTITY_ITERATIONS,
        "occurrence_iterations": HARDENING_OCCURRENCE_ITERATIONS,
        "pruned_processes_per_backend_turn": HARDENING_PRUNE_BATCH,
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
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters: BTreeMap::from([
            ("backends".to_string(), 3),
            (
                "identity_iterations".to_string(),
                (chat_turns * HARDENING_IDENTITY_ITERATIONS) as u64,
            ),
            (
                "occurrence_iterations".to_string(),
                (chat_turns * HARDENING_OCCURRENCE_ITERATIONS) as u64,
            ),
        ]),
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

fn measure_hardening_identity_phases(
    turn_index: usize,
    phase_profile: &mut BTreeMap<String, RuntimePerfPhaseRunResult>,
) -> anyhow::Result<()> {
    let registration =
        lash_core::runtime::prepare_process_registration(lash_core::ProcessRegistration::new(
            format!("identity-process-{turn_index}"),
            lash_core::ProcessInput::External {
                metadata: serde_json::json!({
                    "nested": {"z": 3, "a": [1, 2, 3]},
                    "turn": turn_index,
                }),
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        ))?;
    let (_, phase) =
        measure_runtime_perf_phase("store_hardening.identity.process_registration", || {
            for index in 0..HARDENING_IDENTITY_ITERATIONS {
                std::hint::black_box(lash_core::runtime::process_registration_fingerprint(
                    &registration,
                    &[format!("observer-{index}")],
                ));
            }
            Ok(())
        })?;
    phase_profile.insert(phase.0, phase.1);

    let occurrence = lash_core::TriggerOccurrenceRequest::new(
        "perf-source",
        "perf-source-key",
        serde_json::json!({"payload": [1, 2, 3]}),
        format!("occurrence-{turn_index}"),
    );
    let (_, phase) =
        measure_runtime_perf_phase("store_hardening.identity.trigger_occurrence", || {
            for _ in 0..HARDENING_OCCURRENCE_ITERATIONS {
                std::hint::black_box(lash_core::facade_support::deterministic_occurrence_id(
                    &occurrence,
                ));
            }
            Ok(())
        })?;
    phase_profile.insert(phase.0, phase.1);

    let usage = store_hardening_usage(turn_index);
    let (_, phase) = measure_runtime_perf_phase("store_hardening.identity.usage_delta", || {
        for ordinal in 0..HARDENING_IDENTITY_ITERATIONS {
            std::hint::black_box(lash_core::store::RuntimeUsageDeltaIdentity::for_entry(
                format!("perf-operation-{turn_index}"),
                ordinal as u64,
                &usage,
            ));
        }
        Ok(())
    })?;
    phase_profile.insert(phase.0, phase.1);
    Ok(())
}

async fn measure_store_hardening_backend_turn(
    store: &Arc<dyn lash_core::RuntimePersistence>,
    session_id: &str,
    owner: &lash_core::LeaseOwnerIdentity,
    turn_index: usize,
    names: StoreHardeningPhaseNames,
) -> anyhow::Result<BTreeMap<String, RuntimePerfPhaseRunResult>> {
    let mut phases = BTreeMap::new();
    let (lease, phase) = measure_runtime_perf_async_phase(names.claim_session_lease, async {
        store
            .try_claim_session_execution_lease(
                session_id,
                owner,
                "measure-store-hardening-backend-turn-executor",
                HARDENING_LEASE_TTL_MS,
            )
            .await?
            .acquired()
            .ok_or_else(|| anyhow::anyhow!("store-hardening session lease was busy"))
    })
    .await?;
    phases.insert(phase.0, phase.1);

    store
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                vec![QueuedWorkPayload::agent_frame_task(
                    "perf-frame",
                    format!("hardening task {turn_index}"),
                    None,
                )],
            )
            .with_source_key(format!("hardening:{session_id}:{turn_index}")),
        )
        .await?;
    let (claim, phase) = measure_runtime_perf_async_phase(names.claim_queued_work, async {
        store
            .claim_ready_queued_work(
                session_id,
                &lease.fence(),
                owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(1),
            )
            .await?
            .claim()
            .ok_or_else(|| anyhow::anyhow!("store-hardening expected queued-work claim"))
    })
    .await?;
    phases.insert(phase.0, phase.1);

    let mut state = load_store_hardening_state(store, session_id).await?;
    let (_, phase) = measure_runtime_perf_async_phase(names.complete_queued_work, async {
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_queue_claim(claim.completion());
        let result = store.commit_runtime_state(commit).await?;
        state.apply_persisted_commit_result(result);
        Ok::<(), anyhow::Error>(())
    })
    .await?;
    phases.insert(phase.0, phase.1);

    let attachment_id =
        lash_core::AttachmentId::parse(format!("hardening-attachment-{session_id}-{turn_index}"))
            .expect("valid attachment id");
    let (_, phase) = measure_runtime_perf_phase(names.attachment_intent, || {
        Ok(store.record_intent(AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: session_id.to_string(),
            canonical_uri: format!("sha256:{attachment_id}"),
            intent_at_epoch_ms: turn_index as u64 + 1,
            owner_kind: Some(AttachmentOwnerKind::Turn),
            owner_id: Some(format!("hardening-turn-{turn_index}")),
        })?)
    })?;
    phases.insert(phase.0, phase.1);

    state = load_store_hardening_state(store, session_id).await?;
    let operation_id = format!("hardening-append-{turn_index}");
    let nodes = vec![lash_core::SessionAppendNode::plugin(
        "perf-hardening",
        serde_json::json!({"turn": turn_index, "payload": [1, 2, 3, 4]}),
    )];
    let mut commit = lash_core::store::append_request_commit_for_testing(
        &mut state,
        &operation_id,
        &nodes,
        None,
    )?;
    let usage = store_hardening_usage(turn_index);
    let operation_storage_key = lash_core::OperationId::storage_key(&commit.turn_commit.operation)?;
    commit.usage_deltas = vec![lash_core::store::RuntimeUsageDelta {
        identity: lash_core::store::RuntimeUsageDeltaIdentity::for_entry(
            operation_storage_key,
            0,
            &usage,
        ),
        entry: usage,
    }];
    let replay_commit = commit.clone();
    let (_, phase) = measure_runtime_perf_async_phase(names.append_receipt_usage_fresh, async {
        let result = store.commit_runtime_state(commit).await?;
        if result.receipt_replayed {
            anyhow::bail!("fresh hardening append unexpectedly replayed");
        }
        Ok::<(), anyhow::Error>(())
    })
    .await?;
    phases.insert(phase.0, phase.1);

    let (_, phase) = measure_runtime_perf_async_phase(names.append_receipt_replay, async {
        let result = store.commit_runtime_state(replay_commit).await?;
        if !result.receipt_replayed {
            anyhow::bail!("hardening append retry did not use its durable receipt");
        }
        Ok::<(), anyhow::Error>(())
    })
    .await?;
    phases.insert(phase.0, phase.1);

    state = load_store_hardening_state(store, session_id).await?;
    let (_, phase) = measure_runtime_perf_async_phase(names.attachment_adopt, async {
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_committed_attachments([attachment_id]);
        let result = store.commit_runtime_state(commit).await?;
        state.apply_persisted_commit_result(result);
        Ok::<(), anyhow::Error>(())
    })
    .await?;
    phases.insert(phase.0, phase.1);
    Ok(phases)
}

async fn measure_process_prune(
    registry: &Arc<dyn lash_core::ProcessRegistry>,
    phase_name: &'static str,
    backend: &str,
    run_id: &str,
    turn_index: usize,
    phase_profile: &mut BTreeMap<String, RuntimePerfPhaseRunResult>,
) -> anyhow::Result<()> {
    for index in 0..HARDENING_PRUNE_BATCH {
        let process_id = format!("perf-prune-{backend}-{run_id}-{turn_index}-{index}");
        registry
            .register_process(lash_core::ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({"index": index}),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            ))
            .await?;
        registry
            .complete_process(
                &process_id,
                lash_core::ProcessAwaitOutput::Success {
                    value: serde_json::json!({"done": index}),
                    control: None,
                },
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await?;
    }
    let (report, phase) = measure_runtime_perf_async_phase(phase_name, async {
        registry
            .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
            .await
            .map_err(anyhow::Error::from)
    })
    .await?;
    if report.pruned_processes != HARDENING_PRUNE_BATCH {
        anyhow::bail!(
            "{backend} process prune removed {}, expected {HARDENING_PRUNE_BATCH}",
            report.pruned_processes
        );
    }
    phase_profile.insert(phase.0, phase.1);
    Ok(())
}

async fn load_store_hardening_state(
    store: &Arc<dyn lash_core::RuntimePersistence>,
    session_id: &str,
) -> anyhow::Result<RuntimeSessionState> {
    Ok(
        lash::persistence::load_persisted_session_state(store.as_ref())
            .await?
            .unwrap_or_else(|| RuntimeSessionState {
                session_id: session_id.to_string(),
                ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                ))
            }),
    )
}

fn store_hardening_create_request(session_id: &str) -> lash_core::SessionStoreCreateRequest {
    lash_core::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

fn store_hardening_usage(turn_index: usize) -> lash_core::TokenLedgerEntry {
    lash_core::TokenLedgerEntry {
        source: "store-hardening".to_string(),
        model: "perf-model".to_string(),
        usage: TokenUsage {
            input_tokens: turn_index as i64 + 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 4,
            reasoning_output_tokens: 5,
        },
    }
}
