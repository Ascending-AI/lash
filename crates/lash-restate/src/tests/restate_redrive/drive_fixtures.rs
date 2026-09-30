use super::*;

pub(super) fn journaled_drive_inputs(live_generation: u64) -> lash_core::AdmittedTurnInputs {
    let session_id = SessionId::from("session");
    lash_core::AdmittedTurnInputs {
        session_id: session_id.clone(),
        mode: lash_core::TurnInputAdmissionMode::NextTurn,
        inputs: vec![lash_core::PendingTurnInput {
            input_id: lash_core::InputId::from("in_7"),
            session_id,
            enqueue_seq: 7,
            source_key: None,
            state: lash_core::TurnInputState::DeferredNextTurn,
            enqueued_at_ms: live_generation,
            input: lash_core::TurnInput::text("deploy staging"),
            run_spec: None,
        }],
        applications: Vec::new(),
    }
}

pub(super) fn drive_envelope() -> RuntimeEffectEnvelope {
    let acceptance = lash_core::runtime::causal::turn_acceptance_effect_invocation(
        &durable_turn_scope("session", "turn"),
        &SessionId::from("session"),
        &lash_core::TurnId::from("turn"),
    );
    RuntimeEffectEnvelope::new(
        lash_core::runtime::causal::turn_input_drive_effect_invocation(&acceptance),
        RuntimeEffectCommand::AdmitRoot {
            head: lash_core::store::AdmittedHead::Input(lash_core::InputId::from("in_7")),
        },
    )
}

pub(super) async fn execute_drive(
    context: &Arc<ReplayableRecordingContext>,
    live_generation: u64,
    local_runs: &Arc<AtomicUsize>,
) -> lash_core::store::RootAdmission {
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(context));
    match controller
        .execute_effect(
            drive_envelope(),
            RuntimeEffectLocalExecutor::testing({
                let local_runs = Arc::clone(local_runs);
                move |_envelope| async move {
                    local_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::AdmitRoot {
                        answer: lash_core::store::RootAdmissionAnswer::Admitted {
                            admission: Box::new(lash_core::store::RootAdmission {
                                head: lash_core::store::AdmittedHead::Input(
                                    lash_core::InputId::from("in_7"),
                                ),
                                inputs: Some(Box::new(journaled_drive_inputs(live_generation))),
                                queued: None,
                                // What this execution would read from the live
                                // head: a replay must not see it (FIG-3682).
                                base: lash_core::store::SessionHeadRef {
                                    generation: 1,
                                    revision: live_generation,
                                    leaf: None,
                                    checkpoint: None,
                                },
                                turn_index: live_generation + 1,
                                // Likewise the generation: a replay keeps the one
                                // the first execution admitted under (FIG-3571).
                                generation: Some(lash_core::ExecutableGeneration::new(format!(
                                    "blake3:live-{live_generation}"
                                ))),
                            }),
                        },
                    })
                }
            }),
        )
        .await
        .expect("the drive effect runs as a journaled Restate run")
        .into_root_admission()
        .expect("the drive effect returns an admission")
    {
        lash_core::store::RootAdmissionAnswer::Admitted { admission } => *admission,
        refused => panic!("the drive effect admits its head: {refused:?}"),
    }
}

/// Registers `registration` and answers the id its registrar minted.
pub(super) async fn registered(
    registry: &dyn ProcessRegistry,
    registration: &ProcessRegistration,
) -> ProcessId {
    registry
        .register_process(registration.clone())
        .await
        .expect("register the process")
        .id
}
