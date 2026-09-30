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
//! FIG-4369 closes the window that admission leaves open. An emission whose
//! ingest answered the delivery unbound records its admission to start, and a
//! barrier then holds it before its start registers. Meanwhile another invocation binds the
//! delivery to P1, P1 completes, and retention prunes it. The held emission's
//! start key then finds nothing. Its registration reads the delivery's binding
//! in the same transaction as the start-key check, finds P1 and registers
//! nothing. The emission records that refusal, then an admission of the bound
//! process, and answers P1. Its replay serves both from the journal.
//!
//! Both laws run every emission inside a handler of the tier's engine, and P1
//! runs on the tier's process workflow, served by a worker whose engine
//! records each process that ran.

use std::sync::atomic::{AtomicBool, Ordering};
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

/// Everything both laws stand on: one subscription whose target is the
/// increment engine, the tier's process workflow serving it, and the
/// occurrence request each emission sends.
struct LawRig {
    session_id: SessionId,
    stores: Arc<dyn crate::StoreSet>,
    triggers: Arc<dyn crate::TriggerStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    increments: Arc<Increments>,
    process_work: crate::ProcessWorkWiring,
    process_engines: crate::ProcessEngineRegistry,
    request: crate::TriggerOccurrenceRequest,
}

impl LawRig {
    /// Register `{prefix}-{law}`'s subscription, capturing `source_capture`,
    /// and serve its processes on the tier's process workflow.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each setup step is the law's precondition"
    )]
    async fn set_up(
        prefix: &str,
        law: &str,
        source_capture: crate::TriggerSourceCapture,
        effect_host: Arc<dyn crate::EffectHost>,
        stores: Arc<dyn crate::StoreSet>,
        runner: &dyn ConformanceTurnRunner,
    ) -> Self {
        let session_id = SessionId::from(format!("{prefix}-{law}"));
        let source_key = format!("{prefix}-{law}-source");
        let idempotency_key = format!("{prefix}-{law}-occurrence");
        let triggers = stores.trigger_store();
        let registry = stores.process_registry();

        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
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
        // No wake target: a wake still owed would keep the completed child
        // from being pruned, and the laws are about the prune.
        triggers
            .execute_command(
                &format!("{prefix}-{law}-register"),
                crate::TriggerCommand::Register {
                    owner_scope: crate::TriggerOwnerScope::session(&session_id),
                    actor: crate::ProcessOriginator::session(crate::SessionScope::new(&session_id)),
                    draft: increment_subscription(&source_key, env_ref, source_capture),
                },
            )
            .await
            .expect("register trigger call")
            .expect("register trigger succeeds");

        // The tier's process workflow serves every process the deliveries
        // start with a worker whose one engine records the process it ran.
        let increments = Arc::new(Increments::default());
        let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), effect_host)
            .host_config(
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
                lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ),
        )
        .expect("build the bound-trigger process worker");
        let process_work = runner.process_work(watched, worker);
        Self {
            request: crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key,
                serde_json::json!({ "button": "Blue" }),
                idempotency_key,
            ),
            session_id,
            stores,
            triggers,
            registry,
            increments,
            process_work,
            process_engines: host.process_engines,
        }
    }

    /// The router a host emits through: the tier's process work, the
    /// environments and engines its starts stage, and the `ProcessStart`
    /// ledger its starts arm.
    fn router(&self) -> lash_core::facade_support::TriggerRouter {
        lash_core::facade_support::TriggerRouter::new(
            Arc::clone(&self.triggers),
            self.process_work.clone(),
        )
        .with_process_artifacts(
            self.stores.process_env_store(),
            self.process_engines.clone(),
        )
        .with_process_starts(
            self.stores
                .obligation_ledger(crate::store::ObligationKind::ProcessStart),
            self.stores.clock(),
            crate::drive::relay::RelayPolicy::default(),
        )
    }

    /// The journaled step an emission's invocation records after it.
    fn marker(&self) -> Arc<JournalMarker> {
        Arc::new(JournalMarker {
            triggers: Arc::clone(&self.triggers),
            owner_scope: crate::TriggerOwnerScope::session(&self.session_id),
        })
    }

    /// Wait for `process_id` to complete, which its one increment precedes.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the process is retained until the law prunes it"
    )]
    async fn await_terminal(&self, process_id: &ProcessId) {
        loop {
            let record = self
                .registry
                .get_process(process_id)
                .await
                .expect("read the process")
                .expect("the process is retained until it is pruned");
            if record.status.is_terminal() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Prune `process_id`, the one terminal process, and check that its
    /// delivery outlives it.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the prune is established by the setup above"
    )]
    async fn prune_bound(&self, process_id: &ProcessId, occurrence_id: &str) {
        let pruned = self
            .registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune terminal processes");
        assert_eq!(
            pruned.pruned_processes, 1,
            "the bound, released process is pruned"
        );
        assert!(
            matches!(
                self.registry.get_process(process_id).await,
                Err(crate::PluginError::ProcessNoLongerRetained { .. })
            ),
            "the bound process was pruned"
        );
        assert_eq!(
            bound_process(self.triggers.as_ref(), occurrence_id).await,
            vec![Some(process_id.clone())],
            "the delivery outlives its pruned process"
        );
    }

    /// The delivery of `occurrence_id` answered by its first emission: bound
    /// to `process_id`, with its pin released.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the reads are established by the setup above"
    )]
    async fn bound_delivery(
        &self,
        occurrence_id: &str,
        process_id: &ProcessId,
    ) -> crate::TriggerDeliveryReservation {
        let reservation = self
            .triggers
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
            .expect("list the delivery")
            .remove(0);
        assert_eq!(
            reservation.process_id,
            Some(process_id.clone()),
            "the delivery is bound"
        );
        assert_eq!(
            self.registry
                .list_trigger_delivery_pins()
                .await
                .expect("list trigger delivery pins"),
            Vec::new(),
            "the bind released the delivery's pin"
        );
        reservation
    }

    /// Nothing but `process_id` was ever registered for the delivery: its
    /// start key holds no process, nothing is pinned, and the delivery is
    /// still bound to `process_id`.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the reads are established by the setup above"
    )]
    async fn assert_nothing_else_registered(
        &self,
        start_key: &crate::StartKey,
        occurrence_id: &str,
        process_id: &ProcessId,
        context: &str,
    ) {
        assert_eq!(
            self.registry
                .get_process_by_start_key(start_key)
                .await
                .expect("read the delivery's start key"),
            None,
            "{context} registered no second process"
        );
        assert_eq!(
            self.registry
                .list_trigger_delivery_pins()
                .await
                .expect("list trigger delivery pins"),
            Vec::new(),
            "{context} pinned nothing"
        );
        assert_eq!(
            bound_process(self.triggers.as_ref(), occurrence_id).await,
            vec![Some(process_id.clone())],
            "{context} left the delivery bound to its one process"
        );
    }

    /// The end state: no process start is owed and `process_id` performed
    /// the only increment.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the ledger read is established by the setup above"
    )]
    async fn assert_one_increment(&self, process_id: ProcessId) {
        let owed = self
            .stores
            .obligation_ledger(crate::store::ObligationKind::ProcessStart)
            .claim_due(
                crate::ClockWallTime::timestamp_ms(self.stores.clock().as_ref()) + 86_400_000,
                1,
                std::num::NonZeroUsize::new(16).expect("a nonzero page"),
            )
            .await
            .expect("claim owed process starts");
        assert!(owed.is_empty(), "no process start is owed: {owed:?}");
        assert_eq!(
            self.increments.ran(),
            vec![process_id],
            "one occurrence, one increment in total"
        );
    }
}

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
    let rig = LawRig::set_up(
        prefix,
        "bound-trigger",
        crate::TriggerSourceCapture::resident(["ui", "button"], crate::LashSchema::any()),
        effect_host,
        stores,
        runner.as_ref(),
    )
    .await;
    let router = Arc::new(rig.router());

    // 1. The first emission's invocation starts P1 and binds the delivery to
    //    it. The invocation then dies, and is replayed at step 5.
    let first_emission = Arc::new(Reports::default());
    let emitted = CancellationToken::new();
    let crash = CancellationToken::new();
    let marker = rig.marker();
    let replayed = Arc::new(Reports::default());
    let first_invocation = runner.run_crashed_then_redriven_turn(
        crate::admit(crate::ExecutionScope::runtime_operation(format!(
            "{prefix}-bound-trigger-first"
        ))),
        crashing_emit_attempt(
            Arc::clone(&router),
            rig.request.clone(),
            Arc::clone(&first_emission),
            Arc::clone(&marker),
            emitted.clone(),
            crash.clone(),
        ),
        emit_attempt(
            Arc::clone(&router),
            rig.request.clone(),
            Arc::clone(&replayed),
            Some(Arc::clone(&marker)),
        ),
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
        let reservation = rig.bound_delivery(&occurrence_id, &p1).await;
        let subscription_id = reservation.subscription.subscription_id.clone();
        let start_key = lash_core::facade_support::trigger_delivery_start_key(&reservation);

        // 2. P1 performs its increment and completes.
        rig.await_terminal(&p1).await;
        assert_eq!(
            rig.increments.ran(),
            vec![p1.clone()],
            "P1 incremented once"
        );

        // 3. A retention pass prunes P1; the delivery stays retained.
        rig.prune_bound(&p1, &occurrence_id).await;

        // 4. The identical occurrence, through a fresh owning invocation,
        //    answers P1 and starts nothing.
        runner
            .run_turn(
                crate::admit(crate::ExecutionScope::runtime_operation(format!(
                    "{prefix}-bound-trigger-duplicate"
                ))),
                emit_attempt(
                    Arc::clone(&router),
                    rig.request.clone(),
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
        rig.assert_nothing_else_registered(&start_key, &occurrence_id, &p1, "the duplicate")
            .await;

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
    rig.assert_nothing_else_registered(&start_key, &occurrence_id, &p1, "the replay")
        .await;
    rig.assert_one_increment(p1).await;
    runner.scenario_finished().await;
}

/// An emission held between its ingest, which answered the delivery unbound,
/// and its start's registration, while another invocation binds the delivery
/// and retention prunes the bound process, answers that process and starts
/// nothing; its replay answers it again (FIG-4369).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn trigger_emission_held_across_a_bind_and_prune_returns_the_bound_process(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    // A provider route, so the router consults its restorer between the
    // delivery's recorded admission and its start's registration. The held
    // emission's router holds there; the other router has no restorer and
    // leaves the route as captured.
    let rig = LawRig::set_up(
        prefix,
        "raced-trigger",
        crate::TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::LashSchema::any(),
            "raced-trigger-provider",
            serde_json::json!({ "route": "button" }),
        ),
        effect_host,
        stores,
        runner.as_ref(),
    )
    .await;
    let barrier = Arc::new(RouteBarrier::new());
    let held_router = Arc::new(
        rig.router()
            .with_route_restorer(Arc::clone(&barrier) as Arc<dyn crate::TriggerRouteRestorer>),
    );
    let router = Arc::new(rig.router());

    // 1. The held emission's ingest answers the delivery unbound, and its
    //    admission to start is recorded. The barrier then holds it before its
    //    start registers. Its invocation dies after the emission and is
    //    replayed at step 5.
    let held_emission = Arc::new(Reports::default());
    let emitted = CancellationToken::new();
    let crash = CancellationToken::new();
    let marker = rig.marker();
    let replayed = Arc::new(Reports::default());
    let held_invocation = runner.run_crashed_then_redriven_turn(
        crate::admit(crate::ExecutionScope::runtime_operation(format!(
            "{prefix}-raced-trigger-held"
        ))),
        crashing_emit_attempt(
            Arc::clone(&held_router),
            rig.request.clone(),
            Arc::clone(&held_emission),
            Arc::clone(&marker),
            emitted.clone(),
            crash.clone(),
        ),
        emit_attempt(
            Arc::clone(&held_router),
            rig.request.clone(),
            Arc::clone(&replayed),
            Some(Arc::clone(&marker)),
        ),
    );

    let binding = Arc::new(Reports::default());
    let law = async {
        barrier.reached.cancelled().await;

        // 2. Another invocation emits the same occurrence: it starts P1 and
        //    binds the delivery to it.
        runner
            .run_turn(
                crate::admit(crate::ExecutionScope::runtime_operation(format!(
                    "{prefix}-raced-trigger-binding"
                ))),
                emit_attempt(
                    Arc::clone(&router),
                    rig.request.clone(),
                    Arc::clone(&binding),
                    None,
                ),
            )
            .await;
        let report = binding.only();
        assert_eq!(report.deliveries.len(), 1, "one subscription matches");
        assert_eq!(
            report.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::Started,
            "the binding emission started its delivery: {report:?}"
        );
        let p1 = report.deliveries[0]
            .process_id
            .clone()
            .expect("the binding emission names its process");
        let occurrence_id = report.occurrence_id.clone();
        let reservation = rig.bound_delivery(&occurrence_id, &p1).await;
        let subscription_id = reservation.subscription.subscription_id.clone();
        let start_key = lash_core::facade_support::trigger_delivery_start_key(&reservation);

        // 3. P1 performs its increment and completes, and a retention pass
        //    prunes it; the delivery stays retained.
        rig.await_terminal(&p1).await;
        assert_eq!(
            rig.increments.ran(),
            vec![p1.clone()],
            "P1 incremented once"
        );
        rig.prune_bound(&p1, &occurrence_id).await;

        // 4. The held emission goes on from the unbound delivery its ingest
        //    answered. Its start's key no longer finds P1, and it answers P1
        //    without registering, scheduling or pinning anything.
        barrier.release.cancel();
        emitted.cancelled().await;
        let report = held_emission.only();
        assert_eq!(
            report.deliveries,
            vec![crate::TriggerDeliveryEmitReceipt {
                occurrence_id: occurrence_id.clone(),
                subscription_id: subscription_id.clone(),
                process_id: Some(p1.clone()),
                outcome: TriggerDeliveryEmitOutcome::Started,
            }],
            "the held emission returns the process the delivery was bound to while it was held"
        );
        rig.assert_nothing_else_registered(&start_key, &occurrence_id, &p1, "the held emission")
            .await;

        // 5. The held emission's invocation is replayed.
        crash.cancel();
        (p1, occurrence_id, subscription_id, start_key)
    };
    let ((), (p1, occurrence_id, subscription_id, start_key)) = tokio::join!(held_invocation, law);

    let report = replayed.only();
    assert_eq!(
        report.deliveries,
        vec![crate::TriggerDeliveryEmitReceipt {
            occurrence_id: occurrence_id.clone(),
            subscription_id,
            process_id: Some(p1.clone()),
            outcome: TriggerDeliveryEmitOutcome::Started,
        }],
        "the replayed held emission serves its recorded steps and answers P1"
    );
    rig.assert_nothing_else_registered(&start_key, &occurrence_id, &p1, "the replay")
        .await;
    rig.assert_one_increment(p1).await;
    runner.scenario_finished().await;
}

/// The first run of an emission whose invocation dies after it: it emits
/// `request`, records `marker`'s step and its report, signals `emitted`, and
/// panics once `crash` is cancelled.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: an emission that errors is the law's failure"
)]
fn crashing_emit_attempt(
    router: Arc<lash_core::facade_support::TriggerRouter>,
    request: crate::TriggerOccurrenceRequest,
    reports: Arc<Reports>,
    marker: Arc<JournalMarker>,
    emitted: CancellationToken,
    crash: CancellationToken,
) -> ConformanceTurnAttempt {
    Arc::new(move |scoped: ScopedEffectController<'_>| {
        let router = Arc::clone(&router);
        let request = request.clone();
        let reports = Arc::clone(&reports);
        let marker = Arc::clone(&marker);
        let emitted = emitted.clone();
        let crash = crash.clone();
        Box::pin(async move {
            let report = router.emit(request, &scoped).await.expect("the emission");
            marker.record(&scoped).await;
            reports.record(report);
            emitted.cancel();
            crash.cancelled().await;
            panic!("the emission's invocation dies after its emission")
        })
    })
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

/// The route restorer the held emission's router consults once its admission
/// is recorded and just before its start registers. Its first call holds the
/// emission there until the law releases it. Every later call, the replay's
/// included, answers at once.
struct RouteBarrier {
    armed: AtomicBool,
    /// Cancelled once the held emission has reached the barrier.
    reached: CancellationToken,
    /// Cancelled by the law to let the held emission go on.
    release: CancellationToken,
}

impl RouteBarrier {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(true),
            reached: CancellationToken::new(),
            release: CancellationToken::new(),
        }
    }
}

#[async_trait::async_trait]
impl crate::TriggerRouteRestorer for RouteBarrier {
    async fn restore(
        &self,
        _capture: &crate::TriggerSourceCapture,
    ) -> Result<(), crate::TriggerRouteRefusal> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.cancel();
            self.release.cancelled().await;
        }
        Ok(())
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
    source_capture: crate::TriggerSourceCapture,
) -> crate::TriggerSubscriptionDraft {
    let mut input_template = std::collections::BTreeMap::new();
    input_template.insert("event".to_string(), crate::TriggerInputBinding::Event);
    crate::TriggerSubscriptionDraft {
        source_capture,
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

/// Register the bound-trigger duplicate law (FIG-4297) and the held-emission
/// law (FIG-4369). The fixture, run once for each law, hands back a guard, a
/// prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must crash
/// and replay an invocation and serve process segments.
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

        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn trigger_emission_held_across_a_bind_and_prune_returns_the_bound_process() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            // The deadlock watchdog, and no part of the law: its waits have
            // no deadline, so a run a loaded pool slows is waited for.
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                $crate::registration_macro_support::trigger_emission_held_across_a_bind_and_prune_returns_the_bound_process(
                    &prefix, host, stores, runner,
                ),
            )
            .await
            .expect(
                "deadlock watchdog: trigger_emission_held_across_a_bind_and_prune_returns_the_bound_process hung on a wait that never ended",
            );
        }
    };
}
