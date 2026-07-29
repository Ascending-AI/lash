use super::*;

pub(crate) async fn run_generated_workload_for_fixture(
    workload: GeneratedWorkload,
    script_bundle_hash: &str,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    let trace = run_generated_workload(
        workload,
        script_bundle_hash,
        &SimShard::FULL.label(),
        Path::new("trace.json"),
    )
    .await?;
    if !trace.oracle.is_passed() {
        return Err(FixedScriptRunnerError::Assertion(
            trace.oracle.message.clone(),
        ));
    }
    Ok(trace)
}

/// Drive a generated workload through the scheduler-driven, concurrency-faithful
/// runtime world and return the delivered boundary log plus the abstract world
/// summary. This is the single driver shared by the reference in-memory run and
/// the cross-backend SQLite re-run (`replay_workload_on_sqlite`): the only thing
/// that varies is the `world`'s backend. Driving the SAME workload through the
/// SAME dynamic scheduler — rather than re-deriving a recorded trace in fixed
/// order with provider events gated to recorded counts — is what makes the
/// cross-backend comparison apples-to-apples.
pub(super) async fn drive_generated_workload(
    world: &mut GeneratedRuntimeWorld,
    workload: &GeneratedWorkload,
) -> Result<
    (
        Vec<crate::scheduler::DeliveredBoundary>,
        AbstractWorldSummary,
    ),
    FixedScriptRunnerError,
> {
    let (initial_boundaries, mut completion_queue) =
        split_runtime_completion_boundaries(workload.boundaries.clone());
    let mut scheduler = BoundaryScheduler::with_events(workload.seed, initial_boundaries);
    let mut completion_state = RuntimeCompletionState {
        serialize_provider_turns: world.serialize_provider_turns,
        ..RuntimeCompletionState::default()
    };
    let mut store = ModelStore::default();
    let mut log = BoundaryDeliveryLog::default();
    let mut suspend_ready_at = 1_000_000u64;
    loop {
        // Serialized cross-backend barrier: while a provider turn is live, never
        // deliver a boundary scheduled at or after that turn's completion time
        // until the completion itself has been scheduled. The turn's own provider
        // releases all fall strictly before `final_ready_at`, so they still flow
        // through and drive the turn forward; only boundaries that would otherwise
        // jump ahead of the not-yet-scheduled completion are held. This removes
        // the sole source of backend-dependent delivery drift (a slow async store
        // letting a later boundary overtake the completion), so the in-memory and
        // durable serialized runs produce a byte-identical delivery order. The
        // SEARCH lane keeps full concurrency and is unaffected.
        if world.serialize_provider_turns
            && let Some(barrier) = world.min_active_final_ready_at()
            && scheduler
                .min_pending_at()
                .is_none_or(|next_at| next_at >= barrier)
        {
            world
                .schedule_finished_provider_turns(&mut scheduler)
                .await?;
            suspend_ready_at += 1;
            world
                .schedule_parked_suspend_resolutions(&mut scheduler, suspend_ready_at)
                .await?;
            // Spin until the live turn finishes and lands its completion (lowering
            // `min_pending_at` below the barrier), or it is gone. The provider
            // release deliveries that unblock the turn run on later iterations
            // because they are scheduled strictly before the barrier.
            if world.active_provider_turn_count() > 0
                && scheduler
                    .min_pending_at()
                    .is_none_or(|next_at| next_at >= barrier)
            {
                continue;
            }
        }
        let Some(mut delivered) = scheduler.deliver_next(Value::Null) else {
            world
                .schedule_finished_provider_turns(&mut scheduler)
                .await?;
            suspend_ready_at += 1;
            world
                .schedule_parked_suspend_resolutions(&mut scheduler, suspend_ready_at)
                .await?;
            if world.active_provider_turn_count() > 0 || world.pending_suspend_turn_count() > 0 {
                continue;
            }
            if scheduler.is_empty() {
                break;
            }
            continue;
        };
        let event = delivered.as_event();
        world.advance_time_for_boundary(&event).await;
        let observed = world.deliver_boundary(&event).await?;
        store.apply_observed_boundary(&event, &observed);
        delivered.observed = observed;
        completion_state.observe(&delivered);
        completion_queue.mark_completed(&delivered.boundary_id);
        register_ready_runtime_completions(
            &mut completion_queue,
            &mut completion_state,
            &mut scheduler,
            &delivered,
            world,
        )
        .await?;
        world
            .schedule_finished_provider_turns(&mut scheduler)
            .await?;
        suspend_ready_at += 1;
        world
            .schedule_parked_suspend_resolutions(&mut scheduler, suspend_ready_at)
            .await?;
        log.push(delivered);
    }
    if !completion_queue.is_empty() {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "runtime completion queue ended with {} unresolved pending completions {:?} after registering {} and completing {}",
            completion_queue.pending_len(),
            completion_queue.pending_ids(),
            completion_queue.registered_len(),
            completion_queue.completed_len()
        )));
    }
    let mut events = log.into_vec();
    append_contract_execution_boundaries(&mut events, &mut store, workload.seed).await?;
    store.apply_durable_writes(&world.durable_write_events());
    let final_summary = store.summary();
    Ok((events, final_summary))
}

/// The serialized in-memory reference summary for the cross-backend check.
///
/// This drives the workload through the SAME `serialize_provider_turns` discipline
/// as the durable re-run, differing ONLY in the backend store (ephemeral
/// in-memory here vs the real durable store in `replay_workload_on_sqlite`). That
/// is what makes the comparison a well-posed durable-state equivalence: both runs
/// share one scheduling discipline, so any difference is a real store divergence
/// rather than an artifact of serialized-vs-concurrent execution. (The
/// concurrency-preserving SEARCH lane summary — `run_generated_workload`'s
/// `serialize_provider_turns == false` run — is a different discipline and is used
/// for oracle fuzzing, not for this backend equivalence comparison.)
pub async fn replay_workload_serialized_reference(
    workload: &GeneratedWorkload,
) -> Result<AbstractWorldSummary, FixedScriptRunnerError> {
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
    let (_events, final_summary) = drive_generated_workload(&mut world, workload).await?;
    Ok(final_summary)
}

/// Cross-backend check: re-drive the SAME generated workload through the SAME
/// dynamic runtime driver under the SAME serialized-provider-turn discipline, but
/// backed by the real `lash-sqlite-store` session store factory and the SQLite
/// durable-effect replay controller, and return the resulting abstract world
/// summary. The caller compares it against the serialized in-memory reference
/// (`replay_workload_serialized_reference`); equality proves the SQLite store
/// reproduces identical observable runtime behavior. Both runs serialize provider
/// turns and the serialized driver holds boundary delivery across a live turn's
/// completion, so the delivery order is fully determined by the workload and is
/// independent of the store's async timing — there is no fixed-order,
/// exchange-count-gated re-drive to deadlock, and no concurrency- or
/// async-timing-induced divergence.
pub async fn replay_workload_on_sqlite(
    workload: &GeneratedWorkload,
    db_root: &Path,
) -> Result<AbstractWorldSummary, FixedScriptRunnerError> {
    if db_root.exists() {
        if db_root.is_dir() {
            std::fs::remove_dir_all(db_root)?;
        } else {
            std::fs::remove_file(db_root)?;
        }
    }
    std::fs::create_dir_all(db_root)?;
    let clock = SimClock::new();
    let store_factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(db_root.to_path_buf())
            .with_clock(clock.clone()),
    );
    let effect_replay_store =
        RuntimeEffectReplayStore::sqlite_file(db_root.join("runtime-effects.sqlite"));
    // A durable session store requires durable attachment + process-env stores,
    // so back them with the real SQLite/file stores (the in-memory reference uses
    // their ephemeral counterparts). These facets are not under cross-backend
    // comparison; only the session store's observable Lash state is.
    let attachment_store: Arc<dyn lash::persistence::AttachmentStore> = Arc::new(
        lash::persistence::FileAttachmentStore::new(db_root.join("attachments")),
    );
    let process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore> = Arc::new(
        lash_sqlite_store::Store::open_with_clock(
            &db_root.join("process-env.sqlite"),
            clock.clone(),
        )
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?,
    );
    let mut world = GeneratedRuntimeWorld::with_backend(
        store_factory,
        effect_replay_store,
        attachment_store,
        process_env_store,
        // Serialize live provider turns for the durable re-run so async-store
        // interleaving cannot change committed outcomes vs the sync in-memory
        // reference; the comparison is then a well-posed durable-state equivalence.
        true,
        clock,
    );
    let (_events, final_summary) = drive_generated_workload(&mut world, workload).await?;
    Ok(final_summary)
}

/// Cross-backend check for Postgres using the same dynamic generated workload
/// driver and serialized-provider-turn discipline as the SQLite equivalence
/// lane. This is intentionally not fixed-order trace replay: generated traces
/// can contain provider-exchange scheduling that only the generated driver owns.
pub async fn replay_workload_on_postgres(
    workload: &GeneratedWorkload,
    database_url: &str,
    artifact_root: &Path,
) -> Result<AbstractWorldSummary, FixedScriptRunnerError> {
    let storage = Arc::new(
        lash_postgres_store::PostgresStorage::connect(database_url)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?,
    );
    let clock = SimClock::new();
    crate::postgres_replay::reset_postgres_for_replay(storage.as_ref())
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let attachment_root = artifact_root.join("attachments");
    if attachment_root.exists() {
        std::fs::remove_dir_all(&attachment_root)?;
    }
    std::fs::create_dir_all(&attachment_root)?;
    let store_factory: Arc<dyn SessionStoreFactory> = Arc::new(storage.session_store_factory());
    let effect_replay_store = RuntimeEffectReplayStore::postgres(Arc::clone(&storage));
    let attachment_store: Arc<dyn lash::persistence::AttachmentStore> =
        Arc::new(lash::persistence::FileAttachmentStore::new(attachment_root));
    let process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore> =
        Arc::new(storage.process_env_store());
    let mut world = GeneratedRuntimeWorld::with_backend(
        store_factory,
        effect_replay_store,
        attachment_store,
        process_env_store,
        true,
        clock,
    );
    let (_events, final_summary) = drive_generated_workload(&mut world, workload).await?;
    Ok(final_summary)
}

pub async fn run_generated_postgres_replay_for_seeds(
    artifact_root: impl AsRef<Path>,
    profile: &str,
    seed_values: &[u64],
    max_boundaries: usize,
    database_url: &str,
) -> Result<GeneratedPostgresReplayReport, FixedScriptRunnerError> {
    validate_workload_profile(profile)?;
    if seed_values.is_empty() {
        return Err(FixedScriptRunnerError::Assertion(
            "generated Postgres replay requires at least one seed".to_string(),
        ));
    }
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root)?;
    let boundary_limit = max_boundaries.max(1);
    let mut cases = Vec::new();
    let mut passed = 0;
    let mut failed = 0;
    let mut first_failure = None;
    for seed in seed_values.iter().copied() {
        let workload = generate_workload(seed, profile, boundary_limit)?;
        let reference = replay_workload_serialized_reference(&workload).await?;
        let case_dir = artifact_root.join(format!("seed-{seed:016x}"));
        std::fs::create_dir_all(&case_dir)?;
        let actual = replay_workload_on_postgres(&workload, database_url, &case_dir).await?;
        let verdict = replay_determinism(&reference, &actual);
        let report_path = case_dir.join("postgres-generated-rerun.json");
        let matches_reference = verdict.is_passed();
        let case_report = if matches_reference {
            json!({
                "schema": "lash.sim.postgres-generated-rerun.v1",
                "seed": seed,
                "profile": profile,
                "backend": "lash_postgres_store",
                "driver": "unified_generated_runtime_world",
                "matches_reference": true,
                "reference_digest": reference.digest.clone(),
                "actual_digest": actual.digest.clone(),
                "verdict": verdict.clone(),
                "final_summary": actual,
            })
        } else {
            json!({
                "schema": "lash.sim.postgres-generated-rerun.v1",
                "seed": seed,
                "profile": profile,
                "backend": "lash_postgres_store",
                "driver": "unified_generated_runtime_world",
                "matches_reference": false,
                "reference_digest": reference.digest.clone(),
                "actual_digest": actual.digest.clone(),
                "verdict": verdict.clone(),
                "reference_summary": reference,
                "actual_summary": actual,
            })
        };
        std::fs::write(&report_path, serde_json::to_vec_pretty(&case_report)?)?;
        let report_sha256 = file_sha256(&report_path)?;
        if matches_reference {
            passed += 1;
        } else {
            failed += 1;
        }
        cases.push(GeneratedPostgresReplayCase {
            seed,
            trace_alias: format!("seed-{seed:016x}"),
            status: if matches_reference {
                "passed"
            } else {
                "failed"
            },
            report_path: relative_path(artifact_root, &report_path),
            report_sha256,
            reference_digest: case_report["reference_digest"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            actual_digest: case_report["actual_digest"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            verdict: verdict.clone(),
        });
        if !matches_reference && first_failure.is_none() {
            first_failure = Some(format!(
                "generated Postgres re-run for seed {seed} ({profile}) diverged from the serialized in-memory reference: {}; wrote {}",
                verdict.message,
                report_path.display()
            ));
        }
    }
    let summary_path = artifact_root.join("summary.json");
    let report = GeneratedPostgresReplayReport {
        schema: "lash.sim.postgres-generated-rerun-summary.v1",
        status: if failed == 0 { "passed" } else { "failed" },
        profile: profile.to_string(),
        configured_max_boundaries: boundary_limit,
        database_url_redacted: crate::postgres_replay::redact_database_url(database_url),
        cases,
        counts: GeneratedPostgresReplayCounts {
            seeds: seed_values.len(),
            passed,
            failed,
        },
        summary_path: summary_path.clone(),
    };
    std::fs::write(&summary_path, serde_json::to_vec_pretty(&report)?)?;
    if let Some(message) = first_failure {
        return Err(FixedScriptRunnerError::Assertion(message));
    }
    Ok(report)
}

pub(super) async fn run_generated_workload(
    workload: GeneratedWorkload,
    script_bundle_hash: &str,
    shard_label: &str,
    trace_path: &Path,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    let mut world = GeneratedRuntimeWorld::new();
    let (events, final_summary) = drive_generated_workload(&mut world, &workload).await?;
    let durable_writes = world.durable_write_events();
    // Per-seed live provider FAILURE turns: real `session.turn().run()`s that
    // stream valid prose then a non-retryable malformed chunk, released through a
    // real BoundaryScheduler, across >1 provider kind and >1 fault position.
    let live_failure_facts = drive_live_provider_failure_turns(workload.seed).await?;
    let mut oracles = vec![
        live_provider_failure_coverage(&live_failure_facts),
        scheduler_controlled_delivery(&events),
        scheduler_owned_runtime_completions(&events),
        state_machine_semantic_invariants(&events, &final_summary),
        operational_coverage(&events, &final_summary),
        ingress_sessions_opened(&final_summary),
        queued_ingress_observed(&final_summary, &events),
        cancellation_observed(&final_summary, &events),
        trigger_delivery_observed(&final_summary, &events),
        observer_reconnect_observed(&final_summary, &events),
        backend_failure_observed(&final_summary, &events),
        provider_mutation_rejected(&final_summary, &events),
        provider_transport_mutation_classified(&events),
        generated_runtime_provider_matrix_oracle(&events),
        provider_turn_interleaving_depth(&events),
        process_wake_observed(&final_summary, &events),
        process_wake_at_most_once(&events),
        process_never_double_started(&events),
        abandoned_requires_evidence(&events),
        tool_boundary_observed(&final_summary, &events),
        exec_code_observed(&final_summary, &events),
        cross_session_isolation(&final_summary),
        observer_convergence(&final_summary),
        runtime_session_graph_contract(&final_summary),
        runtime_graph_acyclic(&events),
        runtime_single_active_agent_frame(&events),
        runtime_usage_monotonic(&events),
        durable_effect_exactly_once(&final_summary),
        worker_stale_completion_rejected(&final_summary),
        worker_failover_continues_work(&events),
        healthy_long_turn_liveness(&events),
        lease_time_monotonic(&events),
        generated_suspend_resume(&events),
        generated_final_value_semantic_channel(&events),
    ];
    oracles.extend(scenario_contract_mini_oracles(&events, &final_summary));
    oracles.extend(scenario_contract_oracles(&events, &final_summary));
    // The combined oracle rides the trace; callers decide whether a failing
    // oracle aborts the run (evidence/fixture paths) or becomes a persisted
    // failure package (search mode).
    let oracle = combine_oracles(&oracles);
    Ok(SimulationTrace::new(
        workload.seed,
        workload.generator_version,
        workload.profile,
        shard_label,
        workload.workload_family,
        workload.workload_id,
        script_bundle_hash,
        workload.aliases.into_map(),
        events,
        durable_writes,
        oracle,
        oracles,
        final_summary,
        trace_path,
    ))
}
