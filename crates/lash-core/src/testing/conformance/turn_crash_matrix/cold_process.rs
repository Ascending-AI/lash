//! Level-2 backend helper-process driver for the turn-crash matrix.
//!
//! The helper actions, their oracle projection out of `turn_crash_outcomes.json`,
//! and the in-process driver the backend helper binaries call live here. The
//! level-1 matrix in the parent module owns the seam scaffolding they reuse.

use super::*;

/// Level-2 crash sites driven by the backend helper processes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ColdProcessTurnAction {
    ProviderInitialMidStream,
    ProviderAfterToolMidStream,
    EffectAfterExternalBeforeOutcome,
    FinalCommitBoundary,
    FinalCommitInsideCall,
    CheckpointAfterExecuteBeforeOutcome,
    RecoverFinalCommitBoundary,
    PeerReclaim,
    Recover,
}

impl ColdProcessTurnAction {
    pub(super) const CRASH_ACTIONS: [Self; 5] = [
        Self::ProviderInitialMidStream,
        Self::ProviderAfterToolMidStream,
        Self::EffectAfterExternalBeforeOutcome,
        Self::FinalCommitBoundary,
        Self::FinalCommitInsideCall,
    ];

    fn command(self) -> &'static str {
        match self {
            Self::ProviderInitialMidStream => "turn_provider_mid_stream",
            Self::ProviderAfterToolMidStream => "turn_provider_after_tool_mid_stream",
            Self::EffectAfterExternalBeforeOutcome => "turn_effect_after_external",
            Self::FinalCommitBoundary => "turn_final_commit_boundary",
            Self::FinalCommitInsideCall => "turn_final_commit_inside",
            Self::CheckpointAfterExecuteBeforeOutcome => {
                "turn_checkpoint_after_execute_before_outcome"
            }
            Self::RecoverFinalCommitBoundary => "turn_recover_final_commit_boundary",
            Self::PeerReclaim => "turn_peer_reclaim",
            Self::Recover => "turn_recover",
        }
    }

    pub(super) fn point(self) -> Option<TurnCrashPoint> {
        match self {
            Self::ProviderInitialMidStream => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Provider(ProviderOperation::InitialMidStream),
                placement: CrashPlacement::ProviderMidStream,
            }),
            Self::ProviderAfterToolMidStream => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Provider(ProviderOperation::AfterToolMidStream),
                placement: CrashPlacement::ProviderMidStream,
            }),
            Self::EffectAfterExternalBeforeOutcome => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
                    name: "trace_effect".to_string(),
                }),
                placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
            }),
            Self::FinalCommitBoundary | Self::RecoverFinalCommitBoundary => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                    settles_queue: true,
                    settles_turn_input: true,
                    releases_lease: true,
                }),
                placement: CrashPlacement::Boundary,
            }),
            Self::FinalCommitInsideCall => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                    settles_queue: true,
                    settles_turn_input: true,
                    releases_lease: true,
                }),
                placement: CrashPlacement::InsideCall,
            }),
            Self::CheckpointAfterExecuteBeforeOutcome | Self::PeerReclaim | Self::Recover => None,
        }
    }
}

/// Return each helper action's exact effect-count and durable-state oracle.
///
/// The values are derived from `turn_crash_outcomes.json`, keeping the backend
/// helper-process assertions on the same oracle as the level-1 matrix.
pub fn cold_process_turn_expectations() -> Vec<(&'static str, usize, usize, String, Option<String>)>
{
    let generated = generated_points(&golden_trace());
    let table = turn_crash_matrix_outcomes();
    validate_outcome_table(&generated, &table).expect("committed turn crash outcomes are valid");
    validate_durable_recovery_rulings(&durable_recovery_rulings())
        .expect("committed durable recovery rulings are valid");
    ColdProcessTurnAction::CRASH_ACTIONS
        .into_iter()
        .map(|action| {
            let point = action.point().expect("crash action has a point");
            let expectation = table
                .iter()
                .find(|entry| entry.point == point)
                .and_then(|entry| entry.level_2.as_ref())
                .expect("level-2 action has a committed expectation");
            let (end_state, known_defect) = match (&expectation.exact, &expectation.known_defect) {
                (Some(exact), None) => (
                    exact.exact().expect("validated exact end-state expectation"),
                    None,
                ),
                (None, Some(defect)) => {
                    let expected = defect
                        .expected_defective
                        .exact()
                        .expect("validated exact known-defect expectation");
                    let notice = format!(
                        "KNOWN-DEFECT {} reproduced exactly for {}: observed {}; fixing {} must produce the correct durable end state {}",
                        defect.ticket,
                        action.command(),
                        expected.summary(),
                        defect.ticket,
                        DurableEndState::CORRECT.summary()
                    );
                    (expected, Some(notice))
                }
                _ => unreachable!("validated level-2 end-state expectation"),
            };
            (
                action.command(),
                expectation.effect_executions.at_crash,
                expectation.effect_executions.after_recovery,
                end_state.summary(),
                known_defect,
            )
        })
        .collect()
}

/// Return the reviewed exact durable end state for a composed level-2 recovery
/// trajectory in `turn_crash_outcomes.json`.
pub fn cold_process_durable_recovery_expectation(scenario: &str) -> String {
    let rulings = durable_recovery_rulings();
    validate_durable_recovery_rulings(&rulings)
        .expect("committed durable recovery rulings are valid");
    rulings
        .into_iter()
        .find(|ruling| ruling.scenario == scenario)
        .unwrap_or_else(|| panic!("unknown durable recovery scenario `{scenario}`"))
        .exact
        .exact()
        .expect("validated exact durable recovery ruling")
        .summary()
}

/// Return the stable execution scope used by a level-2 helper scenario.
pub fn cold_process_turn_scope(scenario: &str) -> crate::ExecutionScope {
    let identity = ReferenceIdentity::for_scenario(scenario);
    crate::ExecutionScope::turn(identity.session_id, identity.turn_id)
}

/// Drive or recover one full scripted turn inside a backend helper process.
///
/// `action` accepts `turn_provider_mid_stream`,
/// `turn_provider_after_tool_mid_stream`, `turn_effect_after_external`,
/// `turn_final_commit_boundary`, `turn_final_commit_inside`, or
/// `turn_recover`.
///
/// Crash actions print `crash_ready` only after the configured semantic point
/// is reached and then park until the parent sends `SIGKILL`. `Recover` polls
/// the session lease, drives a fresh runtime/controller, and reports exact
/// committed terminal-output and ingress counts from the reopened store. The
/// parent process compares that `turn_complete` summary with the outcome table.
pub async fn cold_process_real_turn_driver(
    store: Arc<dyn RuntimePersistence>,
    effect_controller: Arc<dyn RuntimeEffectController>,
    scenario: &str,
    action: &str,
    external_effect_marker: Option<std::path::PathBuf>,
) {
    let action = match action {
        "turn_provider_mid_stream" => ColdProcessTurnAction::ProviderInitialMidStream,
        "turn_provider_after_tool_mid_stream" => ColdProcessTurnAction::ProviderAfterToolMidStream,
        "turn_effect_after_external" => ColdProcessTurnAction::EffectAfterExternalBeforeOutcome,
        "turn_final_commit_boundary" => ColdProcessTurnAction::FinalCommitBoundary,
        "turn_final_commit_inside" => ColdProcessTurnAction::FinalCommitInsideCall,
        "turn_checkpoint_after_execute_before_outcome" => {
            ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome
        }
        "turn_recover_final_commit_boundary" => ColdProcessTurnAction::RecoverFinalCommitBoundary,
        "turn_peer_reclaim" => ColdProcessTurnAction::PeerReclaim,
        "turn_recover" => ColdProcessTurnAction::Recover,
        other => panic!("unknown cold-process real-turn action `{other}`"),
    };
    let identity = ReferenceIdentity::for_scenario(scenario);
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recovers_existing_turn = matches!(
        action,
        ColdProcessTurnAction::Recover
            | ColdProcessTurnAction::RecoverFinalCommitBoundary
            | ColdProcessTurnAction::PeerReclaim
    );
    if !recovers_existing_turn {
        seed_reference_ingress(&store, &identity, scenario).await;
    } else if action == ColdProcessTurnAction::PeerReclaim {
        let owner =
            LeaseOwnerIdentity::opaque("cold-process-peer", format!("{scenario}:peer-reclaim"));
        let lease = tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                super::super::bind_conformance_session(&store, &identity.session_id).await;
                let outcome = store
                    .try_claim_session_execution_lease(
                        &identity.session_id,
                        &owner,
                        "cold-process-real-turn-driver-executor",
                        recovery_timings().ttl_ms(),
                    )
                    .await
                    .expect("poll peer-reclaim lease");
                if let Some(acquisition) = outcome.acquisition() {
                    let displaced = acquisition
                        .displaced
                        .as_ref()
                        .expect("peer reclaim displaces the crashed executor");
                    assert_eq!(displaced.owner.owner_id, "lash-core-test-worker");
                    break acquisition.lease;
                }
                tokio::time::sleep(recovery_timings().renew_interval()).await;
            }
        })
        .await
        .expect("peer can acquire crashed turn lease");
        let claim = store
            .claim_ready_queued_work(
                &identity.session_id,
                &lease.fence(),
                &owner,
                crate::QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("peer reclaims queued-work row")
            .expect("crashed turn left one queued-work row");
        assert_eq!(claim.batches.len(), 1, "peer reclaims exactly one row");
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release peer lease without settling peer row");
        println!(
            "peer_claim row={} claim={} generation={}",
            claim.batches[0].batch_id, claim.claim_id, claim.session_lease_generation
        );
        return;
    } else {
        let owner = LeaseOwnerIdentity::opaque(
            "cold-process-recovery-probe",
            format!("{scenario}:recovery-probe"),
        );
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                super::super::bind_conformance_session(&store, &identity.session_id).await;
                let outcome = store
                    .try_claim_session_execution_lease(
                        &identity.session_id,
                        &owner,
                        "cold-process-real-turn-driver-executor-2",
                        recovery_timings().ttl_ms(),
                    )
                    .await
                    .expect("poll cold-process recovery lease");
                if let Some(acquisition) = outcome.acquisition() {
                    if let Some(displaced) = acquisition.displaced.as_ref() {
                        assert_eq!(displaced.owner.owner_id, "lash-core-test-worker");
                    } else {
                        let terminal_count = crate::load_persisted_session_state(store.as_ref())
                            .await
                            .expect("read already-committed cold-process state")
                            .map(|state| {
                                state
                                    .session_graph
                                    .read_model()
                                    .messages
                                    .iter()
                                    .flat_map(|message| message.parts.iter())
                                    .filter(|part| part.content == "trace turn complete")
                                    .count()
                            })
                            .unwrap_or(0);
                        let pending_count = store
                            .list_pending_turn_inputs(&identity.session_id)
                            .await
                            .expect("list recovery turn inputs")
                            .len();
                        let queued_count = store
                            .list_queued_work(&identity.session_id)
                            .await
                            .expect("list recovery queued work")
                            .len();
                        // FIG-1573: the pinned-active-input scenario seeds no
                        // next-turn row, so its one pending row is the
                        // active-turn row pinned to the turn recovery is about
                        // to resume - the same count, reached from the other
                        // side, and the state in which the drain evaluates the
                        // orphan backstop.
                        if scenario.starts_with("peer-reclaim-") {
                            assert_eq!(
                                (terminal_count, pending_count, queued_count),
                                (0, 1, 1),
                                "the asserted peer handoff leaves both ingress rows for recovery"
                            );
                        } else {
                            assert_eq!(
                                (terminal_count, pending_count, queued_count),
                                (1, 0, 0),
                                "only an already-landed final commit may leave ordinary recovery unheld"
                            );
                        }
                    }
                    let lease = acquisition.lease;
                    store
                        .release_session_execution_lease(&lease.completion())
                        .await
                        .expect("release cold-process recovery probe");
                    break;
                }
                tokio::time::sleep(recovery_timings().renew_interval()).await;
            }
        })
        .await
        .expect("cold-process turn lease becomes reclaimable");
    }

    let trace_tool = TraceTool {
        marker: external_effect_marker,
        ..TraceTool::default()
    };
    let reader = Arc::clone(&store);
    let decorated = SeamStore::wrap(store, control.clone());
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        executions,
        &identity,
        trace_tool,
    ))
    .await;

    let point = action.point();
    if let Some(point) = point {
        control.arm(point);
    } else {
        control.clear();
    }
    let effect_controller: Arc<dyn RuntimeEffectController> =
        if action == ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome {
            Arc::new(CrashAfterCheckpointExecutionController {
                inner: effect_controller,
            })
        } else {
            effect_controller
        };
    let task_identity = identity.clone();
    let task =
        crate::task::spawn(
            async move { drive_turn(runtime, effect_controller, &task_identity).await },
        );
    if action == ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome {
        let result = task.await;
        panic!(
            "checkpoint crash controller returned instead of terminating the process: {result:?}"
        );
    }
    if action != ColdProcessTurnAction::Recover {
        control.wait_for_hit().await;
        println!("crash_ready");
        std::io::Write::flush(&mut std::io::stdout()).expect("flush level-2 crash signal");
        std::future::pending::<()>().await;
    }
    let _recovered_turn = task
        .await
        .expect("join cold-process recovered turn")
        .expect("drive cold-process recovered turn");
    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read cold-process recovered state")
        .expect("cold-process recovery committed a session head");
    let terminal_count = state
        .session_graph
        .read_model()
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.content == "trace turn complete")
        .count();
    let pending_input_count = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list cold-process recovered turn inputs")
        .len();
    let queued_work_count = reader
        .list_queued_work(&identity.session_id)
        .await
        .expect("list cold-process recovered queued work")
        .len();
    println!(
        "turn_complete terminal={terminal_count} pending_inputs={pending_input_count} queued_work={queued_work_count}"
    );
}
