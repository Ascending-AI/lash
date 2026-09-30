//! FIG-4297: a duplicate of a trigger occurrence whose delivery is already
//! bound returns the process it was bound to, and starts nothing.
//!
//! An emission reserves occurrence O's delivery to subscription S, starts its
//! process P1 and binds O/S to P1, which releases the delivery's pin
//! (FIG-4203). P1 performs one external increment and completes, and a
//! retention pass prunes it while O/S stays retained. The identical occurrence
//! then arrives through a fresh owning invocation: the store coalesces it onto
//! O and answers the bound delivery. Its start key no longer finds P1 (ADR
//! 0107's retained-only start keys), so a start would mint P2 and schedule it
//! before its bind refused. The emission's journaled admission decision is what
//! tells the two apart: a fresh invocation that meets a bound delivery records
//! the bound process and returns it without registering or scheduling
//! anything.
//!
//! The first emission's own invocation is then replayed after the prune. Its
//! journal recorded an admission to start, taken while the delivery was
//! unbound, and the start that followed it, so the replay consumes both and
//! answers P1 again: the decision is the journal's, never today's reservation.
//! The invocation journals one more step after its emission, so a replay that
//! left any of the emission's entries unconsumed meets that step as a journal
//! mismatch instead of ending quietly.
//!
//! The law runs every emission inside a handler of the tier's engine, and P1
//! runs on the tier's process workflow, served by a worker whose engine
//! records each process that ran.

use std::sync::{Arc, Mutex, PoisonError};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use crate::{
    ConformanceTurnAttempt, ConformanceTurnEnd, ConformanceTurnRunner, ProcessAwaitOutput,
    ProcessId, ScopedEffectController, TriggerDeliveryEmitOutcome, TriggerEmitReport,
};

/// The engine kind the law's subscription targets.
const INCREMENT_KIND: &str = "bound-trigger-increment";

/// A duplicate of a bound trigger delivery's occurrence, emitted by a fresh
/// invocation after the bound process was pruned, returns that process and
/// starts nothing; the original emission's replay still answers it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn bound_trigger_duplicate_after_child_prune_returns_original_process(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-bound-trigger"));
    let source_key = format!("{prefix}-bound-trigger-source");
    let idempotency_key = format!("{prefix}-bound-trigger-occurrence");
    let triggers = stores.trigger_store();
    let registry = stores.process_registry();

    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    stores
        .process_env_store()
        .publish_process_execution_env(
            &crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
                crate::HostArtifactPin::mint(),
            ))
            .expect("host pin claim"),
            &env_ref,
            &spec.to_store_bytes().expect("encode env"),
        )
        .await
        .expect("publish the subscription's environment");
    // No wake target: a wake still owed would keep the completed child from
    // being pruned, and the law is about the prune.
    triggers
        .execute_command(
            &format!("{prefix}-bound-trigger-register"),
            crate::TriggerCommand::Register {
                owner_scope: crate::TriggerOwnerScope::session(&session_id),
                actor: crate::ProcessOriginator::session(crate::SessionScope::new(&session_id)),
                draft: increment_subscription(&source_key, env_ref),
            },
        )
        .await
        .expect("register trigger call")
        .expect("register trigger succeeds");

    // The tier's process workflow serves every process the deliveries start
    // with a worker whose one engine records the process it ran.
    let increments = Arc::new(Increments::default());
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), effect_host).host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.process_engines = host.process_engines.clone().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(IncrementEngine {
            increments: Arc::clone(&increments),
        })),
    );
    let watched = crate::facade_support::watch_process_registry(Arc::clone(&registry));
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            // A process's runtime needs a protocol session; the engine
            // needs nothing else.
            Arc::new(crate::testing::test_plugin_host(Vec::new())),
            host.clone(),
            crate::ProcessWorkWiring::new(
                watched.clone(),
                Arc::new(crate::NoProcessWork::new(&watched)),
            ),
            Arc::new(crate::NoSessionWork::new()),
            crate::testing::runtime_lease_owner(),
        ),
    )
    .expect("build the bound-trigger process worker");
    let process_work = runner.process_work(watched, worker);
    // The router a host emits through: the tier's process work, the
    // environments and engines its starts stage, and the `ProcessStart`
    // ledger its starts arm.
    let router = Arc::new(
        lash_core::facade_support::TriggerRouter::new(Arc::clone(&triggers), process_work)
            .with_process_artifacts(stores.process_env_store(), host.process_engines.clone())
            .with_process_starts(
                stores.obligation_ledger(crate::store::ObligationKind::ProcessStart),
                stores.clock(),
                crate::drive::relay::RelayPolicy::default(),
            ),
    );
    let request = crate::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        source_key.clone(),
        serde_json::json!({ "button": "Blue" }),
        idempotency_key,
    );

    // 1. The first emission's invocation starts P1 and binds the delivery to
    //    it. The invocation then dies, and is replayed at step 5.
    let first_emission = Arc::new(Reports::default());
    let emitted = CancellationToken::new();
    let crash = CancellationToken::new();
    let marker = Arc::new(JournalMarker {
        triggers: Arc::clone(&triggers),
        owner_scope: crate::TriggerOwnerScope::session(&session_id),
    });
    let crashing: ConformanceTurnAttempt = {
        let router = Arc::clone(&router);
        let request = request.clone();
        let first_emission = Arc::clone(&first_emission);
        let marker = Arc::clone(&marker);
        let emitted = emitted.clone();
        let crash = crash.clone();
        Arc::new(move |scoped: ScopedEffectController<'_>| {
            let router = Arc::clone(&router);
            let request = request.clone();
            let first_emission = Arc::clone(&first_emission);
            let marker = Arc::clone(&marker);
            let emitted = emitted.clone();
            let crash = crash.clone();
            Box::pin(async move {
                let report = router
                    .emit(request, &scoped)
                    .await
                    .expect("the first emission");
                marker.record(&scoped).await;
                first_emission.record(report);
                emitted.cancel();
                crash.cancelled().await;
                panic!("the first emission's invocation dies after its emission")
            })
        })
    };
    let replayed = Arc::new(Reports::default());
    let redrive = emit_attempt(
        Arc::clone(&router),
        request.clone(),
        Arc::clone(&replayed),
        Some(Arc::clone(&marker)),
    );
    let first_invocation = runner.run_crashed_then_redriven_turn(
        crate::admit(crate::ExecutionScope::runtime_operation(format!(
            "{prefix}-bound-trigger-first"
        ))),
        crashing,
        redrive,
    );

    let duplicate = Arc::new(Reports::default());
    let law = async {
        emitted.cancelled().await;
        let report = first_emission.only();
        assert_eq!(report.deliveries.len(), 1, "one subscription matches");
        assert_eq!(
            report.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::Started,
            "the first emission started its delivery: {report:?}"
        );
        let p1 = report.deliveries[0]
            .process_id
            .clone()
            .expect("the first emission names its process");
        let occurrence_id = report.occurrence_id.clone();
        let reservation = triggers
            .list_deliveries_by_occurrence_id(&occurrence_id)
            .await
            .expect("list the delivery")
            .remove(0);
        let subscription_id = reservation.subscription.subscription_id.clone();
        let start_key = lash_core::facade_support::trigger_delivery_start_key(&reservation);
        assert_eq!(
            reservation.process_id,
            Some(p1.clone()),
            "the delivery is bound"
        );
        assert_eq!(
            registry
                .list_trigger_delivery_pins()
                .await
                .expect("list trigger delivery pins"),
            Vec::new(),
            "the bind released the delivery's pin"
        );

        // 2. P1 performs its increment and completes.
        loop {
            let record = registry
                .get_process(&p1)
                .await
                .expect("read P1")
                .expect("P1 is retained until it is pruned");
            if record.status.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(increments.ran(), vec![p1.clone()], "P1 incremented once");

        // 3. A retention pass prunes P1; the delivery stays retained.
        let pruned = registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune terminal processes");
        assert_eq!(
            pruned.pruned_processes, 1,
            "the bound, released P1 is pruned"
        );
        assert!(
            matches!(
                registry.get_process(&p1).await,
                Err(crate::PluginError::ProcessNoLongerRetained { .. })
            ),
            "P1 was pruned"
        );
        assert_eq!(
            bound_process(triggers.as_ref(), &occurrence_id).await,
            vec![Some(p1.clone())],
            "the delivery outlives its pruned process"
        );

        // 4. The identical occurrence, through a fresh owning invocation,
        //    answers P1 and starts nothing.
        runner
            .run_turn(
                crate::admit(crate::ExecutionScope::runtime_operation(format!(
                    "{prefix}-bound-trigger-duplicate"
                ))),
                emit_attempt(
                    Arc::clone(&router),
                    request.clone(),
                    Arc::clone(&duplicate),
                    None,
                ),
            )
            .await;
        let report = duplicate.only();
        assert_eq!(
            report.occurrence_id, occurrence_id,
            "the duplicate coalesces"
        );
        assert_eq!(
            report.deliveries,
            vec![crate::TriggerDeliveryEmitReceipt {
                occurrence_id: occurrence_id.clone(),
                subscription_id: subscription_id.clone(),
                process_id: Some(p1.clone()),
                outcome: TriggerDeliveryEmitOutcome::Started,
            }],
            "the duplicate returns the process its delivery is bound to"
        );
        assert_eq!(
            registry
                .get_process_by_start_key(&start_key)
                .await
                .expect("read the delivery's start key"),
            None,
            "the duplicate registered no second process"
        );
        assert_eq!(
            registry
                .list_trigger_delivery_pins()
                .await
                .expect("list trigger delivery pins"),
            Vec::new(),
            "the duplicate pinned nothing"
        );
        assert_eq!(
            bound_process(triggers.as_ref(), &occurrence_id).await,
            vec![Some(p1.clone())],
            "the delivery's binding is unchanged"
        );

        // 5. The first emission's invocation is replayed after the prune.
        crash.cancel();
        (p1, occurrence_id, subscription_id, start_key)
    };
    let ((), (p1, occurrence_id, subscription_id, start_key)) = tokio::join!(first_invocation, law);

    let report = replayed.only();
    assert_eq!(
        report.deliveries,
        vec![crate::TriggerDeliveryEmitReceipt {
            occurrence_id: occurrence_id.clone(),
            subscription_id,
            process_id: Some(p1.clone()),
            outcome: TriggerDeliveryEmitOutcome::Started,
        }],
        "the replayed emission consumed its recorded start and answers P1"
    );
    assert_eq!(
        registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the delivery's start key"),
        None,
        "the replay registered no second process"
    );
    assert_eq!(
        bound_process(triggers.as_ref(), &occurrence_id).await,
        vec![Some(p1.clone())],
        "the delivery is still bound to P1"
    );
    let owed = stores
        .obligation_ledger(crate::store::ObligationKind::ProcessStart)
        .claim_due(
            crate::ClockWallTime::timestamp_ms(stores.clock().as_ref()) + 86_400_000,
            1,
            std::num::NonZeroUsize::new(16).expect("a nonzero page"),
        )
        .await
        .expect("claim owed process starts");
    assert!(owed.is_empty(), "no process start is owed: {owed:?}");
    assert_eq!(
        increments.ran(),
        vec![p1],
        "one occurrence, one increment in total"
    );
    runner.scenario_finished().await;
}

/// One run of an emission of `request`, recording the report it answered,
/// then `marker`'s step when there is one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: an emission that errors is the law's failure"
)]
fn emit_attempt(
    router: Arc<lash_core::facade_support::TriggerRouter>,
    request: crate::TriggerOccurrenceRequest,
    reports: Arc<Reports>,
    marker: Option<Arc<JournalMarker>>,
) -> ConformanceTurnAttempt {
    Arc::new(move |scoped: ScopedEffectController<'_>| {
        let router = Arc::clone(&router);
        let request = request.clone();
        let reports = Arc::clone(&reports);
        let marker = marker.clone();
        Box::pin(async move {
            let report = router.emit(request, &scoped).await.expect("the emission");
            if let Some(marker) = marker {
                marker.record(&scoped).await;
            }
            reports.record(report);
            ConformanceTurnEnd::Settled
        })
    })
}

/// The journaled step the first emission's invocation records after its
/// emission, on every run: a read of the law's subscriptions. A replay whose
/// emission issued fewer or other journaled steps than the first run did
/// meets this step at another step's position and fails with a journal
/// mismatch.
struct JournalMarker {
    triggers: Arc<dyn crate::TriggerStore>,
    owner_scope: crate::TriggerOwnerScope,
}

impl JournalMarker {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a replay that cannot serve the step is the law's failure"
    )]
    async fn record(&self, scoped: &ScopedEffectController<'_>) {
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                scoped.execution_scope().clone(),
                "bound-trigger-journal-marker",
            )
            .expect("the marker's address"),
            crate::RuntimeAttribution::none(),
            "bound-trigger-journal-marker",
        );
        scoped
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::Trigger {
                        command: Box::new(crate::TriggerCommand::List {
                            owner_scope: self.owner_scope.clone(),
                            filter: crate::TriggerSubscriptionFilter::default(),
                        }),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::triggers(Arc::clone(&self.triggers)),
            )
            .await
            .expect("the step after the emission runs, or replays in its place")
            .into_trigger()
            .expect("the marker is a trigger read")
            .expect("the marker's read succeeds");
    }
}

/// The process each of `occurrence_id`'s retained deliveries is bound to.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the read is established by the setup above"
)]
async fn bound_process(
    triggers: &dyn crate::TriggerStore,
    occurrence_id: &str,
) -> Vec<Option<ProcessId>> {
    triggers
        .list_deliveries_by_occurrence_id(occurrence_id)
        .await
        .expect("list the delivery")
        .into_iter()
        .map(|delivery| delivery.process_id)
        .collect()
}

fn increment_subscription(
    source_key: &str,
    env_ref: crate::ProcessExecutionEnvRef,
) -> crate::TriggerSubscriptionDraft {
    let mut input_template = std::collections::BTreeMap::new();
    input_template.insert("event".to_string(), crate::TriggerInputBinding::Event);
    crate::TriggerSubscriptionDraft {
        source_capture: crate::TriggerSourceCapture::resident(
            ["ui", "button"],
            crate::LashSchema::any(),
        ),
        subscription_key: "bound-trigger-increment".to_string(),
        env_ref,
        wake_target: None,
        name: Some("increment".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: source_key.to_string(),
        source: serde_json::json!({ "button": "Blue" }),
        payload_schema: crate::LashSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "button": { "type": "string" } },
            "required": ["button"],
            "additionalProperties": false
        })),
        target: crate::ProcessInput::Engine {
            kind: INCREMENT_KIND.to_string(),
            payload: serde_json::json!({ "process": "increment" }),
        },
        target_identity: crate::ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(
                INCREMENT_KIND,
                serde_json::json!({ "process_name": "increment" }),
            ),
            Some("increment".to_string()),
        ),
        event_types: Vec::new(),
        input_template,
        target_label: Some("increment".to_string()),
    }
}

/// The emission reports one attempt recorded, in run order: Restate may run
/// an attempt more than once.
#[derive(Default)]
struct Reports(Mutex<Vec<TriggerEmitReport>>);

impl Reports {
    fn record(&self, report: TriggerEmitReport) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(report);
    }

    /// The report every run of the attempt answered: a delivery reports its
    /// settled outcome, so a replayed run answers what the first run did.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the attempt ran before its report is read"
    )]
    fn only(&self) -> TriggerEmitReport {
        let reports = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let first = reports.first().cloned().expect("the attempt ran");
        assert!(
            reports.iter().all(|report| *report == first),
            "every run of one emission answers the same: {reports:?}"
        );
        first
    }
}

/// The processes whose run performed the external increment, in run order.
/// A process's run that the engine delivers again coalesces, so every entry
/// is a distinct process's increment.
#[derive(Default)]
struct Increments(Mutex<Vec<ProcessId>>);

impl Increments {
    fn ran(&self) -> Vec<ProcessId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The subscription's target: each process it runs performs one external
/// increment and completes.
struct IncrementEngine {
    increments: Arc<Increments>,
}

#[async_trait::async_trait]
impl crate::ProcessEngine for IncrementEngine {
    fn kind(&self) -> &'static str {
        INCREMENT_KIND
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        {
            let mut ran = self
                .increments
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !ran.contains(context.process_id()) {
                ran.push(context.process_id().clone());
            }
        }
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({ "incremented": 1 }),
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }
}

/// Register the bound-trigger duplicate law (FIG-4297). The fixture hands
/// back a guard, a prefix, the tier's effect host, the store set under test
/// and its [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must
/// crash and replay an invocation and serve process segments.
#[macro_export]
macro_rules! bound_trigger_duplicate_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bound_trigger_duplicate_after_child_prune_returns_original_process() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            // The deadlock watchdog, and no part of the law: its waits have
            // no deadline, so a run a loaded pool slows is waited for.
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                $crate::registration_macro_support::bound_trigger_duplicate_after_child_prune_returns_original_process(
                    &prefix, host, stores, runner,
                ),
            )
            .await
            .expect(
                "deadlock watchdog: bound_trigger_duplicate_after_child_prune_returns_original_process hung on a wait that never ended",
            );
        }
    };
}
