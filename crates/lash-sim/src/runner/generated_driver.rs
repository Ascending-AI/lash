use super::*;

pub(crate) async fn run_generated_workload_for_fixture(
    workload: GeneratedWorkload,
    script_bundle_hash: &str,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    let trace =
        run_generated_workload(workload, script_bundle_hash, &SimShard::FULL.label()).await?;
    if !trace.oracle.is_passed() {
        return Err(FixedScriptRunnerError::Assertion(
            trace.oracle.message.clone(),
        ));
    }
    Ok(trace)
}

/// Drive a generated workload through the scheduler-driven, concurrency-faithful
/// runtime world and return the delivered boundary log plus the abstract world
/// summary.
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
    loop {
        // A due provider completion must enter the scheduler before a later
        // boundary can overtake it. Task polling speed must not decide whether
        // that completion admits the next turn before queued-input cancellation.
        // This still permits concurrent turns: every turn's wire releases precede
        // its completion time and continue through the shared scheduler.
        if let Some(barrier) = world.min_active_final_ready_at()
            && scheduler
                .min_pending_at()
                .is_none_or(|next_at| next_at >= barrier)
        {
            world
                .schedule_finished_provider_turns(&mut scheduler)
                .await?;
            world
                .schedule_parked_suspend_resolutions(&mut scheduler)
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
            world
                .schedule_parked_suspend_resolutions(&mut scheduler)
                .await?;
            if world.active_provider_turn_count() > 0 || world.pending_suspend_turn_count() > 0 {
                continue;
            }
            if scheduler.is_empty() {
                debug_assert!(
                    world.staged_admissions_is_empty(),
                    "the run ended with discovered boundaries still unadmitted"
                );
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
        let admissions = register_ready_runtime_completions(
            &mut completion_queue,
            &mut completion_state,
            &mut scheduler,
            &delivered,
            world,
            &store,
        )
        .await?;
        store.apply_provider_admissions(&admissions);
        if !admissions.is_empty() {
            delivered.payload["provider_admissions"] = json!(admissions);
        }
        world
            .schedule_finished_provider_turns(&mut scheduler)
            .await?;
        world
            .schedule_parked_suspend_resolutions(&mut scheduler)
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
    append_contract_execution_boundaries(
        &mut events,
        &mut store,
        workload.seed,
        world.checkpoint_write_collector(),
    )
    .await?;
    // The generated lane owns both runtime-turn and attributed contract-proof
    // checkpoint observations; summarize them through the single projection
    // owner so no lane can forget the evidence.
    let final_summary = store
        .summarize_with_trace_checkpoint_writes(&events, &world.checkpoint_write_events())
        .map_err(FixedScriptRunnerError::Assertion)?;
    Ok((events, final_summary))
}

/// One run of a workload in the SERIAL lane: one live provider turn at a
/// time, on a server double that runs one attempt at a time.
#[derive(Debug)]
pub(super) struct SerialLaneRun {
    pub(super) delivered: Vec<(String, BoundaryKind)>,
    pub(super) summary: AbstractWorldSummary,
    /// Every grant of the server's turn, in order.
    pub(super) schedule_trace: Vec<(String, u32)>,
    /// Turns the server moved on a stall rather than a sequenced hand-off.
    pub(super) stall_preemptions: u64,
}

/// Run `workload` in the serial lane on its own current-thread runtime, the
/// one runtime shape on which one seed grants the server's turn in one order.
pub(super) fn run_serial_lane(
    workload: GeneratedWorkload,
) -> Result<SerialLaneRun, FixedScriptRunnerError> {
    let label = format!("serial-lane-seed-{:016x}", workload.seed);
    run_on_sim_harness_stack(label, SIM_HARNESS_STACK_LIMIT_BYTES, move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(FixedScriptRunnerError::Io)?;
        runtime.block_on(async move {
            let mut world = GeneratedRuntimeWorld::serial(workload.seed).await?;
            let (events, summary) = drive_generated_workload(&mut world, &workload).await?;
            let server = world.engine().restate().server();
            Ok(SerialLaneRun {
                delivered: events
                    .iter()
                    .map(|event| (event.boundary_id.clone(), event.kind))
                    .collect(),
                summary,
                schedule_trace: server.schedule_trace(),
                stall_preemptions: server.stats().stall_preemptions,
            })
        })
    })
}

pub const SERIAL_ENGINE_DETERMINISM_ORACLE: &str = "sim.oracle.serial-engine-determinism.v1";

/// The serial lane is deterministic across runs of one seed: the same
/// delivered boundaries and the same final abstract outcome. The server's
/// grant order and stall count are recorded as evidence: a turn waiting on a
/// scripted provider gate is not blocked on the server, so an outside request
/// from a task lash runs beside its handlers can still land on a stall. The
/// lane tightens to grant-order equality and zero stalls once those tasks are
/// server-visible (FIG-3751).
pub(super) fn serial_engine_determinism(
    seed: u64,
    first: &SerialLaneRun,
    second: &SerialLaneRun,
) -> OracleVerdict {
    let failure = if first.delivered != second.delivered {
        Some("the two runs delivered different boundary sequences".to_string())
    } else if !replay_determinism(&first.summary, &second.summary).is_passed() {
        Some(format!(
            "the two runs reached different outcomes: {} then {}",
            first.summary.digest, second.summary.digest
        ))
    } else {
        None
    };
    match failure {
        Some(reason) => OracleVerdict::failed(
            SERIAL_ENGINE_DETERMINISM_ORACLE,
            format!("seed {seed:#018x}: {reason}"),
        ),
        None => OracleVerdict::passed(
            SERIAL_ENGINE_DETERMINISM_ORACLE,
            format!(
                "seed {seed:#018x}: two serial runs delivered {} boundaries and reached one outcome; the server granted the turn {} then {} times, with {} then {} stall preemption(s), and the grant orders {}",
                first.delivered.len(),
                first.schedule_trace.len(),
                second.schedule_trace.len(),
                first.stall_preemptions,
                second.stall_preemptions,
                if first.schedule_trace == second.schedule_trace {
                    "matched"
                } else {
                    "differed"
                }
            ),
        ),
    }
}

pub(super) async fn run_generated_workload(
    workload: GeneratedWorkload,
    script_bundle_hash: &str,
    shard_label: &str,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    let mut world = GeneratedRuntimeWorld::new(workload.seed).await?;
    // Declared before the run so oracles can prove an observation class is
    // absent rather than passing vacuously over an empty set.
    let expectations = workload.expectations();
    let (events, final_summary) = drive_generated_workload(&mut world, &workload).await?;
    let durable_writes = world.checkpoint_write_events();
    // Per-seed live provider FAILURE turns: real `session.turn().run()`s that
    // stream valid prose then a non-retryable malformed chunk, released through a
    // real BoundaryScheduler, across >1 provider kind and >1 fault position.
    let live_failure_facts = drive_live_provider_failure_turns(workload.seed).await?;
    // Content evidence is read back through fresh store handles after the run
    // and carries provider-emitted wire content, so it is evaluated here, not
    // from the serialized trace (see `RUN_ONLY_ORACLES`).
    let mut content = world
        .content_evidence(world.reopen_factory().as_ref())
        .await?;
    content.extend(drive_attempt_usage_probe(workload.seed).await?);
    let mut oracles = vec![
        live_provider_failure_coverage(&live_failure_facts),
        crate::content_oracle::durable_content(&content),
        crate::content_oracle::failed_attempt_usage_ledgered(&content),
    ];
    oracles.extend(crate::oracles::generated_trace_oracles(
        &events,
        &final_summary,
        &durable_writes,
        &expectations,
    ));
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
        expectations,
        workload.aliases.into_map(),
        events,
        durable_writes,
        oracle,
        oracles,
        final_summary,
    ))
}
