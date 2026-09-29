use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

const QUEUED_WORK_JOIN_BATCHES_PER_TURN: usize = 32;
const QUEUED_WORK_SEED_OTHER_SESSION_BATCHES: usize = 64;
const TURN_INPUT_INGRESS_ACTIVE_PER_TURN: usize = 32;
const TURN_INPUT_INGRESS_ACCEPTED_PER_TURN: usize = 16;
const TURN_INPUT_INGRESS_NEXT_PER_TURN: usize = 8;

pub(super) async fn run_once_queued_work_claim_stress(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::QueuedWorkClaimStress;
    let session_id = SessionId::from(format!("runtime-perf-{}", scenario.name()));
    let other_session_id = "runtime-perf-queued-work-other";
    let mut run = RunRecorder::start(scenario, chat_turns);

    let (store, _runtime, mut commit_state) = run
        .build(async {
            let runtime = build_runtime(scenario, None).await?;
            let store = runtime.store();
            let commit_state = runtime_perf_commit_state(store.as_ref(), &session_id).await?;
            Ok((store, runtime, commit_state))
        })
        .await?;

    run.seed(async {
        for index in 0..QUEUED_WORK_SEED_OTHER_SESSION_BATCHES {
            store
                .enqueue_queued_work(
                    QueuedWorkBatchDraft::new(
                        other_session_id,
                        DeliveryPolicy::EarliestSafeBoundary,
                        SessionCommand::RefreshToolCatalog {
                            reason: format!("other queued work {index}"),
                        },
                    )
                    .with_source_key(format!("other:{index}")),
                )
                .await?;
        }
        Ok(())
    })
    .await?;

    let mut enqueued_batches = QUEUED_WORK_SEED_OTHER_SESSION_BATCHES;
    let mut command_runs = 0usize;
    let mut join_admissions = 0usize;
    let mut join_batches_admitted = 0usize;
    let mut exclusive_admissions = 0usize;
    let mut completed_batches = 0usize;

    for turn_index in 0..chat_turns {
        run.turn(
            turn_index,
            async {
                let mut phase_profile = BTreeMap::new();

                let (fence, phase) =
                    measure_runtime_perf_async_phase("queued_work.seal_drive_epoch", async {
                        seal_perf_drive(store.as_ref(), &session_id).await
                    })
                    .await?;
                phase_profile.insert(phase.0, phase.1);

                let (heads, phase) =
                    measure_runtime_perf_async_phase("queued_work.enqueue_mixed_batch", async {
                        enqueue_queued_work_stress_turn(store.as_ref(), &session_id, turn_index)
                            .await
                    })
                    .await?;
                phase_profile.insert(phase.0, phase.1);
                enqueued_batches += QUEUED_WORK_JOIN_BATCHES_PER_TURN + 2;

                let (commands, phase) = measure_runtime_perf_async_phase(
                    "queued_work.open_session_command_run",
                    async {
                        store
                            .open_session_command_run(&fence)
                            .await
                            .map_err(anyhow::Error::from)
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                command_runs += 1;
                let run_work = lash_core::runtime::AdmittedQueuedWork {
                    session_id: session_id.clone(),
                    batches: commands,
                };
                if run_work.exclusive_session_command().is_none() {
                    anyhow::bail!(
                        "queued-work stress command run did not contain one session command"
                    );
                }

                let (_, phase) = measure_runtime_perf_async_phase(
                    "queued_work.complete_session_command",
                    async {
                        let mut commit = queued_work_stress_commit(&commit_state, &fence);
                        commit.applied_commands = Some(run_work.completion());
                        let result = store.commit_runtime_state(commit).await?;
                        commit_state.apply_persisted_commit_result(result);
                        Ok::<(), anyhow::Error>(())
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                completed_batches += 1;

                let join_root = lash_core::TurnId::from(format!("queued-work-join-{turn_index}"));
                let (join, phase) =
                    measure_runtime_perf_async_phase("queued_work.admit_join_turn_work", async {
                        admit_perf_root(
                            store.as_ref(),
                            &fence,
                            &join_root,
                            AdmittedHead::Batch(heads.join.clone()),
                            QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                        )
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("queued-work stress expected the join admission")
                        })
                    })
                    .await?;
                phase_profile.insert(phase.0, phase.1);
                let join_batch_ids = join.batch_ids();
                if join_batch_ids.len() != QUEUED_WORK_JOIN_BATCHES_PER_TURN {
                    anyhow::bail!(
                        "queued-work stress expected {} joined batches, got {}",
                        QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                        join_batch_ids.len()
                    );
                }
                join_admissions += 1;
                join_batches_admitted += join_batch_ids.len();

                // An interrupted drive: a successor seals, then resumes the
                // unfinished root, reading back exactly its recorded admission.
                let (fence, phase) =
                    measure_runtime_perf_async_phase("queued_work.supersede_join_drive", async {
                        seal_perf_drive(store.as_ref(), &session_id).await
                    })
                    .await?;
                phase_profile.insert(phase.0, phase.1);

                let (join, phase) =
                    measure_runtime_perf_async_phase("queued_work.resume_join_admission", async {
                        admit_perf_root(
                            store.as_ref(),
                            &fence,
                            &join_root,
                            AdmittedHead::Batch(heads.join.clone()),
                            64,
                        )
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("queued-work stress expected the join resume")
                        })
                    })
                    .await?;
                phase_profile.insert(phase.0, phase.1);
                if join.batch_ids() != join_batch_ids {
                    anyhow::bail!("queued-work stress resume returned different batches");
                }

                let (_, phase) = measure_runtime_perf_async_phase(
                    "queued_work.complete_join_turn_work",
                    async {
                        let result = store
                            .commit_runtime_state(finishing_perf_root(
                                queued_work_stress_commit(&commit_state, &fence),
                                &join_root,
                                &join,
                            ))
                            .await?;
                        commit_state.apply_persisted_commit_result(result);
                        Ok::<(), anyhow::Error>(())
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                completed_batches += join_batch_ids.len();

                let exclusive_root =
                    lash_core::TurnId::from(format!("queued-work-exclusive-{turn_index}"));
                let (exclusive, phase) = measure_runtime_perf_async_phase(
                    "queued_work.admit_exclusive_turn_work",
                    async {
                        admit_perf_root(
                            store.as_ref(),
                            &fence,
                            &exclusive_root,
                            AdmittedHead::Batch(heads.exclusive.clone()),
                            QUEUED_WORK_JOIN_BATCHES_PER_TURN,
                        )
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("queued-work stress expected the exclusive admission")
                        })
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                if exclusive.batch_ids().len() != 1 {
                    anyhow::bail!(
                        "queued-work stress expected one exclusive batch, got {}",
                        exclusive.batch_ids().len()
                    );
                }
                exclusive_admissions += 1;

                let (_, phase) = measure_runtime_perf_async_phase(
                    "queued_work.complete_exclusive_turn_work",
                    async {
                        let result = store
                            .commit_runtime_state(finishing_perf_root(
                                queued_work_stress_commit(&commit_state, &fence),
                                &exclusive_root,
                                &exclusive,
                            ))
                            .await?;
                        commit_state.apply_persisted_commit_result(result);
                        Ok::<(), anyhow::Error>(())
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                completed_batches += 1;

                let (pending, phase) =
                    measure_runtime_perf_async_phase("queued_work.list_pending", async {
                        store
                            .list_open_queued_work(&session_id)
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

                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
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

    let (remaining_measured, remaining_other) = run
        .export(async {
            let remaining_measured = store.list_queued_work(&session_id).await?.len();
            let remaining_other = store
                .list_queued_work(&SessionId::from(other_session_id))
                .await?
                .len();
            let _export_shape = serde_json::json!({
                "enqueued_batches": enqueued_batches,
                "completed_batches": completed_batches,
                "remaining_measured_batches": remaining_measured,
                "remaining_other_batches": remaining_other,
            })
            .to_string();
            Ok((remaining_measured, remaining_other))
        })
        .await?;
    Ok(run.finish(RunTail {
        session_nodes: enqueued_batches,
        active_path_messages: completed_batches,
        extra_counters: BTreeMap::from([
            ("enqueued_batches".to_string(), enqueued_batches as u64),
            ("command_runs".to_string(), command_runs as u64),
            ("join_admissions".to_string(), join_admissions as u64),
            (
                "join_batches_admitted".to_string(),
                join_batches_admitted as u64,
            ),
            (
                "exclusive_admissions".to_string(),
                exclusive_admissions as u64,
            ),
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
        ..RunTail::default()
    }))
}

async fn enqueue_queued_work_stress_turn(
    store: &RuntimePerfStore,
    session_id: &SessionId,
    turn_index: usize,
) -> anyhow::Result<StressTurnHeads> {
    store
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                SessionCommand::RefreshToolCatalog {
                    reason: format!("queued-work-stress-{turn_index}"),
                },
            )
            .with_source_key(format!("command:{turn_index}")),
        )
        .await?;

    let mut join = None;
    for batch_index in 0..QUEUED_WORK_JOIN_BATCHES_PER_TURN {
        let wake = queued_work_stress_wake(
            session_id,
            &format!("queued work stress turn {turn_index} batch {batch_index}"),
            (turn_index * QUEUED_WORK_JOIN_BATCHES_PER_TURN + batch_index + 1) as u64,
            lash_core::store::FleetFormatStore::fleet_format(store),
        );
        let draft = lash_core::runtime::process_wake_batch_draft(wake)
            .with_merge_key("runtime-perf-queued-work-stress");
        let batch = store.enqueue_queued_work(draft).await?;
        join.get_or_insert(batch.batch_id);
    }

    let wake = queued_work_stress_wake(
        session_id,
        &format!("queued work stress exclusive {turn_index}"),
        ((turn_index + 1) * 10_000) as u64,
        lash_core::store::FleetFormatStore::fleet_format(store),
    );
    let exclusive = store
        .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(wake))
        .await?
        .batch_id;
    Ok(StressTurnHeads {
        join: join.ok_or_else(|| anyhow::anyhow!("queued-work stress enqueued no join batch"))?,
        exclusive,
    })
}

/// The turn-lane heads one stress turn enqueued: the first of its joinable
/// wakes and its exclusive wake.
struct StressTurnHeads {
    join: lash_core::BatchId,
    exclusive: lash_core::BatchId,
}

pub(super) fn queued_work_stress_wake(
    session_id: &SessionId,
    input: &str,
    sequence: u64,
    fleet_format: lash_core::FleetFormat,
) -> lash_core::ProcessWakeDelivery {
    let process_id = ProcessId::fixture(&format!("runtime-perf-process-{sequence}"));
    lash_core::ProcessWakeDelivery {
        version: fleet_format.writer_version(lash_core::surface_format!(
            lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("wake:{session_id}:{sequence}"),
        target_session_id: SessionId::from(session_id.to_string()),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::RuntimeInvocation {
            attribution: RuntimeAttribution::for_session(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::QueuedWorkAuthority::default(),
        input: input.to_string(),
        created_at_ms: sequence,
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "a literal valid media type string is the constant attachment payload of this scenario; it parses by construction"
)]
pub(super) async fn run_once_turn_input_ingress_interrupt(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::TurnInputIngressInterrupt;
    let session_id = SessionId::from(format!("runtime-perf-{}", scenario.name()));
    let other_session_id = "runtime-perf-turn-input-other";
    let mut run = RunRecorder::start(scenario, chat_turns);

    let (store, mut commit_state, _restate, turn_control) = run
        .build(async {
            let store = memory_perf_store(&session_id).await?;
            let commit_state = runtime_perf_commit_state(store.as_ref(), &session_id).await?;
            // The effect host that owns the turn-control promises the
            // deferrals settle: the in-process lane's Restate host.
            let restate = restate_backend().await?;
            let host = restate.lash_backend().effect_host();
            let turn_control = lash_core::TurnCancellationAuthority::new(
                host.turn_control_binding_id(),
                host as Arc<dyn lash_core::AwaitEventResolver>,
            );
            Ok((store, commit_state, restate, turn_control))
        })
        .await?;

    run.seed(async {
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
        Ok(())
    })
    .await?;

    let mut active_enqueued = 0usize;
    let mut next_enqueued = 0usize;
    let mut active_admissions = 0usize;
    let mut next_admissions = 0usize;
    let mut resumed_admissions = 0usize;
    let mut completed_inputs = 0usize;
    let mut deferred_inputs = 0usize;

    for turn_index in 0..chat_turns {
        run.turn(
            turn_index,
            async {
                let mut phase_profile = BTreeMap::new();
                let turn_id = lash_core::TurnId::from(format!("turn-input-ingress-{turn_index}"));

                let (fence, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.seal_drive_epoch",
                    async { seal_perf_drive(store.as_ref(), &session_id).await },
                )
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
                                            turn_id.clone(),
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
                            let mut input = TurnInput::text(format!(
                                "queued next turn {turn_index} input {input_index}"
                            ));
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

                let active_step = format!("turn-input-ingress-{turn_index}:after-work");
                let (active, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.admit_active_inputs",
                    async {
                        admit_perf_checkpoint(store.as_ref(), &fence, &turn_id, &active_step)
                            .await?
                            .inputs
                            .ok_or_else(|| anyhow::anyhow!("expected an active input admission"))
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                active_admissions += 1;

                // An interrupted drive: a successor seals, then re-runs the
                // checkpoint step, reading back exactly the rows it admitted.
                let (fence, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.supersede_active_drive",
                    async { seal_perf_drive(store.as_ref(), &session_id).await },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);

                let (active_claim, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.resume_active_admission",
                    async {
                        admit_perf_checkpoint(store.as_ref(), &fence, &turn_id, &active_step)
                            .await?
                            .inputs
                            .ok_or_else(|| anyhow::anyhow!("expected the resumed active admission"))
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                resumed_admissions += 1;
                if active_claim.input_ids() != active.input_ids() {
                    anyhow::bail!("turn-input ingress resume returned different active inputs");
                }
                if active_claim.inputs.len() != TURN_INPUT_INGRESS_ACCEPTED_PER_TURN {
                    anyhow::bail!(
                        "turn-input ingress expected {} active inputs, got {}",
                        TURN_INPUT_INGRESS_ACCEPTED_PER_TURN,
                        active_claim.inputs.len()
                    );
                }
                let active_turn_input = active_claim.materialize_turn_input();
                // Position-independent on purpose: an admission aggregates many inputs and
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
                    anyhow::bail!("turn-input ingress active admission lost attachment bytes");
                }

                // The deferral's completion gate is settled through the
                // effect host that owns the turn-control promises; the phase
                // measures the store's complete-and-defer commit.
                let mut completing = RuntimeCommit::persisted_state_for_test(&commit_state, &[])
                    .deferring_interrupted_turn_inputs(turn_id.clone(), None);
                let mut settlement = lash_core::store::IngressSettlement::new(turn_id.clone());
                settlement.completed_inputs.push(active_claim.completion());
                completing.ingress = Some(settlement);
                let deferral =
                    lash_core::testing::store_fixtures::authorize_completion_deferral_for_test(
                        store.as_ref(),
                        &turn_control,
                        &fence,
                        completing,
                    )
                    .await?;
                let (_, phase) = Box::pin(measure_runtime_perf_async_phase(
                    "turn_input_ingress.complete_active_and_defer",
                    async {
                        let result = store.commit_runtime_state(deferral).await?;
                        commit_state.apply_persisted_commit_result(result);
                        Ok::<(), anyhow::Error>(())
                    },
                ))
                .await?;
                phase_profile.insert(phase.0, phase.1);
                completed_inputs += TURN_INPUT_INGRESS_ACCEPTED_PER_TURN;
                deferred_inputs +=
                    TURN_INPUT_INGRESS_ACTIVE_PER_TURN - TURN_INPUT_INGRESS_ACCEPTED_PER_TURN;

                let next_head = store
                    .list_pending_turn_inputs(&session_id)
                    .await?
                    .into_iter()
                    .find(|read| {
                        matches!(read.status, lash_core::PendingTurnInputReadStatus::Open)
                            && matches!(read.input.ingress(), lash_core::TurnInputIngress::NextTurn)
                    })
                    .map(|read| read.input.input_id)
                    .ok_or_else(|| anyhow::anyhow!("expected an open next-turn input"))?;
                let next_root =
                    lash_core::TurnId::from(format!("turn-input-ingress-next-{turn_index}"));
                let (next, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.admit_next_turn_inputs",
                    async {
                        admit_perf_root(
                            store.as_ref(),
                            &fence,
                            &next_root,
                            AdmittedHead::Input(next_head.clone()),
                            1,
                        )
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("expected a next-turn input admission"))
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                next_admissions += 1;
                let expected_next = TURN_INPUT_INGRESS_ACTIVE_PER_TURN
                    - TURN_INPUT_INGRESS_ACCEPTED_PER_TURN
                    + TURN_INPUT_INGRESS_NEXT_PER_TURN;
                if next.input_ids().len() != expected_next {
                    anyhow::bail!(
                        "turn-input ingress expected {expected_next} next-turn inputs, got {}",
                        next.input_ids().len()
                    );
                }

                let (fence, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.supersede_next_drive",
                    async { seal_perf_drive(store.as_ref(), &session_id).await },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);

                let (next_admission, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.resume_next_admission",
                    async {
                        admit_perf_root(
                            store.as_ref(),
                            &fence,
                            &next_root,
                            AdmittedHead::Input(next_head.clone()),
                            1,
                        )
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("expected the resumed next-turn admission"))
                    },
                )
                .await?;
                phase_profile.insert(phase.0, phase.1);
                resumed_admissions += 1;
                if next_admission.input_ids() != next.input_ids() {
                    anyhow::bail!("turn-input ingress resume returned different next-turn inputs");
                }
                let next_claim = next_admission
                    .inputs
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("expected admitted next-turn inputs"))?;
                let next_turn_input = next_claim.materialize_turn_input();
                // Position-independent: see the active-admission assertion above.
                if !next_turn_input.items.iter().any(|item| {
                    matches!(
                        item,
                        lash_core::InputItem::Attachment {
                            source: lash_core::AttachmentSource::Inline { bytes, .. }
                        } if bytes == &vec![4, 5, 6, turn_index as u8]
                    )
                }) {
                    anyhow::bail!("turn-input ingress next admission lost attachment bytes");
                }

                let (_, phase) = measure_runtime_perf_async_phase(
                    "turn_input_ingress.complete_next_turn_inputs",
                    async {
                        let mut commit =
                            RuntimeCommit::persisted_state_for_test(&commit_state, &[]);
                        commit.drive_fence = Some(Box::new(fence.clone()));
                        let result = store
                            .commit_runtime_state(finishing_perf_root(
                                commit,
                                &next_root,
                                &next_admission,
                            ))
                            .await?;
                        commit_state.apply_persisted_commit_result(result);
                        Ok::<(), anyhow::Error>(())
                    },
                )
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

                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
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

    let (remaining_measured, remaining_other) = run
        .export(async {
            let remaining_measured = store.list_pending_turn_inputs(&session_id).await?.len();
            let remaining_other = store
                .list_pending_turn_inputs(&SessionId::from(other_session_id))
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
            Ok((remaining_measured, remaining_other))
        })
        .await?;

    Ok(run.finish(RunTail {
        session_nodes: active_enqueued + next_enqueued,
        active_path_messages: completed_inputs,
        extra_counters: BTreeMap::from([
            ("active_enqueued".to_string(), active_enqueued as u64),
            ("next_enqueued".to_string(), next_enqueued as u64),
            ("active_admissions".to_string(), active_admissions as u64),
            ("next_admissions".to_string(), next_admissions as u64),
            ("resumed_admissions".to_string(), resumed_admissions as u64),
            ("completed_inputs".to_string(), completed_inputs as u64),
            ("deferred_inputs".to_string(), deferred_inputs as u64),
            (
                "remaining_measured_inputs".to_string(),
                remaining_measured as u64,
            ),
            ("remaining_other_inputs".to_string(), remaining_other as u64),
        ]),
        ..RunTail::default()
    }))
}

/// A head-preserving commit over `state` under `fence`.
fn queued_work_stress_commit(
    state: &RuntimeSessionState,
    fence: &lash_core::store::DriveFence,
) -> RuntimeCommit {
    RuntimeCommit {
        graph: GraphAppend::PreserveHead,
        drive_fence: Some(Box::new(fence.clone())),
        ..RuntimeCommit::persisted_state_for_test(state, &[])
    }
}

/// Admit `root` headed by `head` under `fence`, composing at most
/// `max_batches` joined wakes and every input its head's run takes.
async fn admit_perf_root(
    store: &RuntimePerfStore,
    fence: &lash_core::store::DriveFence,
    root: &lash_core::TurnId,
    head: AdmittedHead,
    max_batches: usize,
) -> anyhow::Result<Option<lash_core::store::RootAdmission>> {
    let mut request =
        lash_core::testing::store_fixtures::admit_root_request_for_test(fence, root, head);
    request.policy = lash_core::testing::queued_work_admission_policy(max_batches);
    request.max_inputs = TURN_INPUT_INGRESS_ACTIVE_PER_TURN + TURN_INPUT_INGRESS_NEXT_PER_TURN;
    Ok(store.admit_root(&request).await?)
}

/// `commit` as `root`'s final commit: it completes every row `admission`
/// bound and writes the root's terminal.
pub(super) fn finishing_perf_root(
    mut commit: RuntimeCommit,
    root: &lash_core::TurnId,
    admission: &lash_core::store::RootAdmission,
) -> RuntimeCommit {
    let mut settlement = lash_core::store::IngressSettlement::new(root.clone());
    settlement
        .completed_batches
        .extend(admission.queued.as_ref().map(|queued| queued.completion()));
    settlement
        .completed_inputs
        .extend(admission.inputs.as_ref().map(|inputs| inputs.completion()));
    commit.ingress = Some(settlement);
    commit.root_terminal = Some(Box::new(lash_core::store::RootTerminalWrite {
        commit: lash_core::store::TurnCommitId::new(root.clone(), 0),
        turn: lash_core::store::PhysicalTurn::derive_turn_id(root, 0),
        root: root.clone(),
        stop: None,
    }));
    commit
}

/// Admit what root `turn_id`'s `AfterWork` checkpoint step `step` takes
/// under `fence`.
async fn admit_perf_checkpoint(
    store: &RuntimePerfStore,
    fence: &lash_core::store::DriveFence,
    turn_id: &lash_core::TurnId,
    step: &str,
) -> anyhow::Result<lash_core::store::CheckpointAdmission> {
    Ok(store
        .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
            fence: fence.clone(),
            root: turn_id.clone(),
            turn_id: turn_id.clone(),
            checkpoint: lash_core::CheckpointKind::AfterWork,
            step: step.to_string(),
            max_inputs: TURN_INPUT_INGRESS_ACCEPTED_PER_TURN,
            policy: lash_core::testing::queued_work_admission_policy(
                QUEUED_WORK_JOIN_BATCHES_PER_TURN,
            ),
        })
        .await?)
}

async fn runtime_perf_commit_state(
    store: &RuntimePerfStore,
    session_id: &SessionId,
) -> anyhow::Result<RuntimeSessionState> {
    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(session_id))
        .await?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_work_admission_stress_advances_its_commit_cursor() {
        Box::pin(run_once_queued_work_claim_stress(1))
            .await
            .expect("queued-work stress scenario");
    }

    #[tokio::test]
    async fn turn_input_ingress_stress_advances_its_commit_cursor() {
        run_once_turn_input_ingress_interrupt(1)
            .await
            .expect("turn-input ingress stress scenario");
    }
}
