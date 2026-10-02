//! FIG-4651: a live fault at a checkpoint's store admission, or while a
//! recorded tool surface is installed, is the attempt's fault. It is never
//! the turn's outcome.
//!
//! Two places sit between a root's work and its next model call:
//!
//! - **Installing a recorded tool surface.** An execution-environment sync
//!   records the surface it built, and the drive installs what the sync
//!   recorded. The install pins the live registry and runs the catalog
//!   contributors again, outside any step, on the live pass and on every
//!   replay that serves the sync from the journal.
//! - **A checkpoint's store admission.** The checkpoint step admits the
//!   root's pending work at the store before it runs its hooks.
//!
//! A contributor that reports a store that did not answer, and a store that
//! did not answer the admission, are facts about this attempt. Each aborts
//! the attempt under its own code: nothing is recorded in the fault's place,
//! the engine retries the attempt, and it rests the turn once its retries run
//! out. When the fault clears, the retry completes the turn from its journal.
//! A hook's own failure, and a deterministic catalog defect, are still the
//! turn's outcome.
//!
//! The laws drive a real root through the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).

use crate::admit;
use lash_core::runtime::effect::{EffectLayer, LayeredEffectHost};
use lash_core::testing::TestTurnDrive as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};

const CONTRIBUTOR_FAULT: &str = "fig4651: the contributor's environment store did not answer";
const ADMISSION_FAULT: &str = "fig4651: pool timed out while waiting for an open connection";

type TurnReports =
    tokio::sync::mpsc::UnboundedReceiver<Result<crate::AssembledTurn, crate::RuntimeError>>;

/// Arms the contributor's fault once the turn's execution-environment sync
/// has answered, live or from the journal: the next contributor run is the
/// install of the surface that sync recorded.
struct FaultAfterSync {
    armed: Arc<AtomicBool>,
    faulting: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl EffectLayer for FaultAfterSync {
    async fn execute_effect(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        envelope: crate::RuntimeEffectEnvelope,
        executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let sync = matches!(
            envelope.command,
            crate::RuntimeEffectCommand::SyncExecutionEnvironment
        );
        let outcome = inner.execute_effect(envelope, executor).await?;
        if sync && self.armed.load(Ordering::SeqCst) {
            self.faulting.store(true, Ordering::SeqCst);
        }
        Ok(outcome)
    }
}

/// A store whose next `faults` checkpoint admissions do not answer.
struct FaultingAdmission {
    inner: Arc<dyn crate::RuntimeStore>,
    faults: Arc<AtomicUsize>,
    admissions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for FaultingAdmission {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_at_checkpoint(
        &self,
        request: &crate::store::CheckpointAdmissionRequest,
    ) -> Result<crate::store::CheckpointAdmission, crate::StoreError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        let faulted = self
            .faults
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            });
        if faulted.is_ok() {
            return Err(crate::StoreError::Backend(ADMISSION_FAULT.to_string()));
        }
        self.inner.admit_at_checkpoint(request).await
    }
}

/// One root of a law: a session, the faults it can raise and what it saw.
#[derive(Clone)]
struct World {
    session_id: SessionId,
    turn_id: TurnId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    /// Whether the sync's answer arms the contributor's fault.
    contributor_armed: Arc<AtomicBool>,
    contributor_faulting: Arc<AtomicBool>,
    /// How many checkpoint admissions the store still leaves unanswered.
    admission_faults: Arc<AtomicUsize>,
    admissions: Arc<AtomicUsize>,
    model_calls: Arc<AtomicUsize>,
}

impl World {
    async fn new(
        name: &str,
        effect_host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
    ) -> Self {
        let model_calls = Arc::new(AtomicUsize::new(0));
        let model = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let model_calls = Arc::clone(&model_calls);
                move |_| {
                    model_calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Ok(crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::Text {
                                text: "done".to_string(),
                                response_meta: None,
                            }],
                            ..crate::LlmResponse::default()
                        })
                    }
                }
            })
            .build();
        let mut host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
            .host_config(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            );
        host.providers.models = crate::testing::standard_test_models(model.into_handle());
        let session_id = SessionId::from(format!("{name}-session"));
        let admission_faults = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn crate::RuntimeStore> = Arc::new(FaultingAdmission {
            inner: crate::conformance::law_session_store(stores.as_ref(), &session_id).await,
            faults: Arc::clone(&admission_faults),
            admissions: Arc::clone(&admissions),
        });
        Self {
            session_id,
            turn_id: TurnId::from(format!("{name}-turn")),
            host,
            store,
            contributor_armed: Arc::new(AtomicBool::new(false)),
            contributor_faulting: Arc::new(AtomicBool::new(false)),
            admission_faults,
            admissions,
            model_calls,
        }
    }

    fn admitted(&self) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(&self.session_id, &self.turn_id))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn runtime(&self) -> crate::LashRuntime {
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        let state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy: policy.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        let faulting = Arc::clone(&self.contributor_faulting);
        let contributor: crate::plugin::ToolCatalogContributor = Arc::new(move |_| {
            if faulting.load(Ordering::SeqCst) {
                return Err(crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    CONTRIBUTOR_FAULT,
                )));
            }
            Ok(crate::plugin::ToolCatalogContribution::default())
        });
        let factories = crate::testing::test_standard_protocol_factories()
            .into_iter()
            .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
                "conformance-live-fault-park",
                crate::facade_support::PluginSpec::new().with_tool_catalog_contributor(contributor),
            ))
                as Arc<dyn crate::facade_support::PluginFactory>])
            .collect();
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(factories)
                .with_store(crate::conformance::helpers::session_view(
                    &self.store,
                    self.session_id.clone(),
                ))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the live-fault conformance runtime")
    }

    /// One execution of the root: a fresh runtime, driven on the tier's
    /// controller, reporting what its drive returned.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    fn attempt(
        &self,
        report: tokio::sync::mpsc::UnboundedSender<
            Result<crate::AssembledTurn, crate::RuntimeError>,
        >,
    ) -> crate::ConformanceTurnAttempt {
        let world = self.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let report = report.clone();
            Box::pin(async move {
                let scope = LayeredEffectHost::layer_scoped(
                    scope,
                    Arc::new(FaultAfterSync {
                        armed: Arc::clone(&world.contributor_armed),
                        faulting: Arc::clone(&world.contributor_faulting),
                    }),
                )
                .expect("layer the tier's controller");
                let mut runtime = world.runtime().await;
                let mut input = crate::TurnInput::text("answer");
                input.trace_turn_id = Some(world.turn_id.clone());
                let turn = runtime
                    .drive_turn(
                        input,
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = report.send(turn);
                end
            })
        })
    }
}

/// Every execution that reported so far aborted on `fault` under its own
/// code: a live store fault, never a recorded outcome. Answers how many
/// reported; an engine that fails the attempt inside a step ends the
/// execution before it can report.
fn assert_aborted_on_live_fault(reports: &mut TurnReports, fault: &str, phase: &str) -> usize {
    let mut aborted = 0;
    while let Ok(report) = reports.try_recv() {
        let error = match report {
            Ok(turn) => panic!("{phase}: the fault is never the turn's outcome: {turn:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitFailed,
            "{phase}: the attempt aborts under the fault's own code: {error:?}"
        );
        assert_eq!(
            error.turn_failure_cause(),
            crate::TurnFailureCause::LiveFault,
            "{phase}: a store that did not answer is the attempt's fault: {error:?}"
        );
        assert!(
            error.message.contains(fault),
            "{phase}: the abort carries the fault it met: {error:?}"
        );
        aborted += 1;
    }
    aborted
}

/// Law: a contributor's live fault while the drive installs the tool surface
/// its sync recorded aborts the attempt as `store_commit_failed`. The engine
/// retries the turn and rests it while the fault lasts, with the model never
/// asked; once the fault clears, the retry serves the recorded sync and
/// completes the turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_live_fault_installing_a_recorded_tool_surface_parks_the_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    // While the fault lasts, every execution aborts and the engine rests the turn.
    let parked = World::new(
        &format!("{prefix}-install-fault-park"),
        &effect_host,
        &stores,
    )
    .await;
    parked.contributor_armed.store(true, Ordering::SeqCst);
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_parking_turn_until_rested(parked.admitted(), parked.attempt(report))
        .await;
    let aborted = assert_aborted_on_live_fault(&mut reports, CONTRIBUTOR_FAULT, "install park");
    assert!(
        aborted > 0,
        "the install runs in the drive, which reports its abort"
    );
    assert_eq!(
        parked.model_calls.load(Ordering::SeqCst),
        0,
        "the surface installs before the model is asked"
    );
    runner.scenario_finished().await;

    // A fault that clears: the retry completes the turn from its journal.
    let repaired = World::new(
        &format!("{prefix}-install-fault-repair"),
        &effect_host,
        &stores,
    )
    .await;
    repaired.contributor_armed.store(true, Ordering::SeqCst);
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(repaired.admitted(), repaired.attempt(report.clone()))
        .await;
    assert_eq!(
        assert_aborted_on_live_fault(&mut reports, CONTRIBUTOR_FAULT, "install fault"),
        1
    );
    repaired.contributor_armed.store(false, Ordering::SeqCst);
    repaired.contributor_faulting.store(false, Ordering::SeqCst);
    runner
        .run_turn(repaired.admitted(), repaired.attempt(report))
        .await;
    let turn = reports
        .try_recv()
        .expect("the retry reported")
        .expect("the retry completes the turn once the contributor answers");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the fault is never the turn's outcome: {:?}",
        turn.outcome
    );
    assert_eq!(
        repaired.model_calls.load(Ordering::SeqCst),
        1,
        "the completed turn asked the model once"
    );
    runner.scenario_finished().await;
}

/// Law: a store that does not answer a checkpoint's admission ends the
/// attempt as `store_commit_failed`; the fault is never recorded as the
/// checkpoint's outcome. The engine retries the turn and rests it while the
/// fault lasts, running the unrecorded checkpoint again and asking the model
/// nothing again; once the store answers, the retry runs the checkpoint and
/// completes the turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_store_fault_at_checkpoint_admission_parks_the_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    // While the fault lasts, no execution settles and the engine rests the turn.
    let parked = World::new(
        &format!("{prefix}-checkpoint-fault-park"),
        &effect_host,
        &stores,
    )
    .await;
    parked.admission_faults.store(usize::MAX, Ordering::SeqCst);
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_parking_turn_until_rested(parked.admitted(), parked.attempt(report))
        .await;
    assert_aborted_on_live_fault(&mut reports, ADMISSION_FAULT, "checkpoint park");
    assert!(
        parked.admissions.load(Ordering::SeqCst) >= 2,
        "the faulted admission was never recorded, so every retry ran it again ({} runs)",
        parked.admissions.load(Ordering::SeqCst)
    );
    assert_eq!(
        parked.model_calls.load(Ordering::SeqCst),
        1,
        "the recorded model call is served on every retry"
    );
    runner.scenario_finished().await;

    // A fault that clears: the engine's retry runs the checkpoint and completes.
    let repaired = World::new(
        &format!("{prefix}-checkpoint-fault-repair"),
        &effect_host,
        &stores,
    )
    .await;
    repaired.admission_faults.store(1, Ordering::SeqCst);
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let mut completed = None;
    // An engine that fails the attempt inside the step retries it itself; one
    // that hands the abort back leaves the retry to the next run of the scope.
    for _ in 0..2 {
        runner
            .run_turn(repaired.admitted(), repaired.attempt(report.clone()))
            .await;
        while let Ok(turn) = reports.try_recv() {
            match turn {
                Ok(turn) => completed = Some(turn),
                Err(error) => {
                    assert_eq!(
                        error.code,
                        crate::RuntimeErrorCode::StoreCommitFailed,
                        "the attempt aborts under the fault's own code: {error:?}"
                    );
                    assert_eq!(
                        error.turn_failure_cause(),
                        crate::TurnFailureCause::LiveFault,
                        "a store that did not answer is the attempt's fault: {error:?}"
                    );
                }
            }
        }
        if completed.is_some() {
            break;
        }
    }
    let turn = completed.expect("the retry completes the turn once the store answers");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the fault is never the turn's outcome: {:?}",
        turn.outcome
    );
    assert_eq!(
        repaired.model_calls.load(Ordering::SeqCst),
        1,
        "the retry served the recorded model call"
    );
    assert!(
        repaired.admissions.load(Ordering::SeqCst) >= 2,
        "the faulted admission was never recorded, so the retry ran it again"
    );
    runner.scenario_finished().await;
}
