//! One law per effect family. Each records a durable operation, moves the
//! store on, and requires the recorded outcome back: through the engine's
//! replay of the recording handler (the replay leg), or through a fresh
//! delivery of the same durable identity (the duplicate leg).

use std::sync::Arc;

use lash_core::ProcessId;
use serde_json::{Value, json};

use super::harness::{
    BOUND, Operation, Receipt, SIGNAL, StorageKind, World, receipt, replay_leg, run_once,
};

/// How a law moves the store on after the first run recorded its outcome.
#[derive(Clone, Copy, Debug)]
pub enum Advance {
    /// The target ends and retention prunes it: reads answer
    /// `ProcessNoLongerRetained`.
    Prune,
    /// As `Prune`, then its tombstone is compacted: nothing names it.
    PruneAndCompact,
    /// The law's session is deleted.
    DeleteSession,
}

fn process_id_of(receipt: &Value) -> ProcessId {
    serde_json::from_value(receipt["process_id"].clone()).expect("a receipt names its process")
}

fn assert_replayed_as_recorded(family: &str, advance: Advance, original: Value, replayed: Receipt) {
    assert_eq!(
        replayed.unwrap_or_else(|error| panic!(
            "{family} after {advance:?}: the replay must answer its recorded outcome, got {error}"
        )),
        original,
        "{family} after {advance:?}: the replay answers exactly the recorded outcome"
    );
}

/// A host start, `Until` the law's session, answers its recorded receipt
/// after the started process is pruned and compacted, or after the session
/// it was granted is deleted (the start's session check is its recorded
/// admission's, ADR 0105 §1).
pub async fn process_start(kind: StorageKind, live: bool, advance: Advance) {
    let world = World::new(kind, live, "start").await;
    let scope = world
        .core
        .processes()
        .session_scope(&world.session_id)
        .await
        .expect("the law's session is live");
    let operation: Operation = {
        let core = world.core.clone();
        let session_id = world.session_id.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = lash_core::ProcessStartRequest::external(
                lash_core::ProcessOriginator::host(),
                json!({"law": "replay-after-advance"}),
                lash_core::Lifetime::Until(scope.clone()),
            )
            .with_observers([session_id.clone()]);
            Box::pin(async move { receipt(core.processes().start(request, scoped).await) })
        })
    };
    let (original, replayed) = replay_leg(
        &world,
        "raa-start",
        operation,
        async |recorded: &Value| match advance {
            Advance::Prune | Advance::PruneAndCompact => {
                world
                    .end_and_prune(
                        &process_id_of(recorded),
                        matches!(advance, Advance::PruneAndCompact),
                    )
                    .await;
            }
            Advance::DeleteSession => world.delete_session().await,
        },
    )
    .await;
    assert_replayed_as_recorded("process start", advance, original, replayed);
}

/// A trigger emit answers its recorded report after the child its delivery
/// started is pruned and compacted: the delivery's start is its recorded
/// process admission, and the replay reads it back.
pub async fn trigger_emit(kind: StorageKind, live: bool) {
    let world = World::new(kind, live, "trigger").await;
    let backend = world.engine.backend();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ),
    )
    .await
    .expect("publish the subscription's env");
    backend
        .trigger_store()
        .execute_command(
            "raa-trigger-subscription",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("replay-after-advance")
                    .expect("a host owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("replay-after-advance"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "raa/trigger-delivery",
                    env_ref,
                    "raa.trigger",
                    "raa-trigger-source",
                    lash_core::ProcessInput::Engine {
                        kind: "testing-fixture".to_string(),
                        payload: json!({"law": "trigger"}),
                    },
                    lash_core::ProcessIdentity::labelled(
                        "testing-fixture",
                        Some("raa-trigger-delivery"),
                    ),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("register the subscription")
        .expect("the subscription registers");
    let operation: Operation = {
        let core = world.core.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            Box::pin(async move {
                receipt(
                    core.triggers()
                        .emit(
                            lash_core::TriggerOccurrenceRequest::new(
                                "raa.trigger",
                                "raa-trigger-source",
                                json!({"law": "trigger"}),
                                "raa-trigger-occurrence",
                            ),
                            scoped,
                        )
                        .await,
                )
            })
        })
    };
    let (original, replayed) = replay_leg(
        &world,
        "raa-trigger",
        operation,
        async |recorded: &Value| {
            let delivered: ProcessId =
                serde_json::from_value(recorded["deliveries"][0]["process_id"].clone())
                    .expect("the emit started one delivery");
            world.end_and_prune(&delivered, true).await;
        },
    )
    .await;
    assert_replayed_as_recorded("trigger emit", Advance::PruneAndCompact, original, replayed);
}

/// The host's route restorer in the trigger-route laws: it answers what the
/// law last set, and counts how often it was asked.
#[derive(Default)]
struct RouteProbe {
    refusal: std::sync::Mutex<Option<lash_core::TriggerRouteRefusal>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RouteProbe {
    fn answer(&self, refusal: Option<lash_core::TriggerRouteRefusal>) {
        *self.refusal.lock().expect("probe lock") = refusal;
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl lash_core::TriggerRouteRestorer for RouteProbe {
    async fn restore(
        &self,
        _capture: &lash_core::TriggerSourceCapture,
    ) -> Result<(), lash_core::TriggerRouteRefusal> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.refusal.lock().expect("probe lock").clone() {
            None => Ok(()),
            Some(refusal) => Err(refusal),
        }
    }
}

/// A world with one subscription on a captured provider route, the router
/// that restores the route through `probe`, and the emission of one
/// occurrence through it.
async fn trigger_route_world(
    kind: StorageKind,
    live: bool,
    tag: &str,
    probe: &Arc<RouteProbe>,
) -> (World, lash_core::facade_support::TriggerRouter, Operation) {
    let world = World::with_route_restorer(
        kind,
        live,
        tag,
        Some(Arc::clone(probe) as Arc<dyn lash::triggers::TriggerRouteRestorer>),
    )
    .await;
    let backend = world.engine.backend();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ),
    )
    .await
    .expect("publish the subscription's env");
    backend
        .trigger_store()
        .execute_command(
            "raa-route-subscription",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("replay-after-advance")
                    .expect("a host owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("replay-after-advance"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "raa/route-delivery",
                    env_ref,
                    "raa.route",
                    "raa-route-source",
                    lash_core::ProcessInput::Engine {
                        kind: "testing-fixture".to_string(),
                        payload: json!({"law": "route"}),
                    },
                    lash_core::ProcessIdentity::labelled(
                        "testing-fixture",
                        Some("raa-route-delivery"),
                    ),
                )
                .with_payload_schema(lash_core::LashSchema::any())
                .with_source_capture(lash_core::TriggerSourceCapture::provider(
                    ["raa", "route"],
                    lash_core::LashSchema::any(),
                    "raa-provider",
                    json!({"grant": "opaque"}),
                )),
            },
        )
        .await
        .expect("register the subscription")
        .expect("the subscription registers");
    let router = lash_core::facade_support::TriggerRouter::new(
        backend.trigger_store(),
        backend.process_work(),
    )
    .with_process_artifacts(
        backend.process_env_store(),
        lash_core::testing::process_engine_fixture(),
    )
    .with_route_restorer(Arc::clone(probe) as Arc<dyn lash_core::TriggerRouteRestorer>);
    let operation: Operation = {
        let core = world.core.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            Box::pin(async move {
                receipt(
                    core.triggers()
                        .emit(
                            lash_core::TriggerOccurrenceRequest::new(
                                "raa.route",
                                "raa-route-source",
                                json!({"law": "route"}),
                                "raa-route-occurrence",
                            ),
                            scoped,
                        )
                        .await,
                )
            })
        })
    };
    (world, router, operation)
}

/// A delivery recorded `Started` replays as started after its provider
/// revoked the route (FIG-4554): the host's restorer was asked inside the
/// start's recorded admission, and the replay reads that record and never
/// asks it again.
pub async fn trigger_route_revoked_after_start(kind: StorageKind, live: bool) {
    let probe = Arc::new(RouteProbe::default());
    let (world, _router, operation) =
        trigger_route_world(kind, live, "route-revoked", &probe).await;
    let (original, replayed) = replay_leg(
        &world,
        "raa-route-revoked",
        operation,
        async |recorded: &Value| {
            assert_eq!(
                recorded["deliveries"][0]["outcome"],
                json!("started"),
                "the first emission started its delivery: {recorded}"
            );
            assert_eq!(probe.calls(), 1, "the fresh start restored its route once");
            probe.answer(Some(lash_core::TriggerRouteRefusal::Revoked {
                provider_id: "raa-provider".to_string(),
                message: "grant withdrawn".to_string(),
            }));
        },
    )
    .await;
    assert_replayed_as_recorded(
        "trigger emit over a route revoked since",
        Advance::Prune,
        original,
        replayed,
    );
    assert_eq!(
        probe.calls(),
        1,
        "the replay reads the recorded start and never asks the restorer"
    );
}

/// A fresh start against an unavailable route records that refusal, typed,
/// as the start's outcome (FIG-4554): its replay reproduces it after the
/// provider came back, without asking the restorer. The reservation stays
/// owed, and its recovery starts it under the same identity.
pub async fn trigger_route_unavailable_at_start(kind: StorageKind, live: bool) {
    let probe = Arc::new(RouteProbe::default());
    probe.answer(Some(lash_core::TriggerRouteRefusal::Unavailable {
        provider_id: "raa-provider".to_string(),
        message: "connect timeout".to_string(),
    }));
    let (world, router, operation) =
        trigger_route_world(kind, live, "route-unavailable", &probe).await;
    let (original, replayed) = replay_leg(
        &world,
        "raa-route-unavailable",
        operation,
        async |recorded: &Value| {
            assert_eq!(
                recorded["deliveries"][0]["outcome"]["failed"]["code"],
                json!("trigger_route_unavailable"),
                "the report carries the typed refusal code: {recorded}"
            );
            let report: lash::triggers::TriggerEmitReport =
                serde_json::from_value(recorded.clone()).expect("decode the facade report");
            assert!(matches!(
                report.deliveries[0].outcome,
                lash::triggers::TriggerDeliveryEmitOutcome::Failed {
                    code: lash::runtime::RuntimeErrorCode::TriggerRouteUnavailable,
                    ..
                }
            ));
            let reason = recorded["deliveries"][0]["outcome"]["failed"]["reason"]
                .as_str()
                .unwrap_or_else(|| panic!("the first emission's delivery failed: {recorded}"));
            assert!(
                reason.contains("connect timeout"),
                "the start records the typed refusal: {reason}"
            );
            assert_eq!(probe.calls(), 1, "the fresh start asked the restorer once");
            probe.answer(None);
        },
    )
    .await;
    assert_replayed_as_recorded(
        "trigger emit over a route restored since",
        Advance::Prune,
        original.clone(),
        replayed,
    );
    assert_eq!(
        probe.calls(),
        1,
        "the replay reads the recorded refusal and never asks the restorer"
    );
    let delivery = &original["deliveries"][0];
    let recovered = router
        .recover_delivery(
            delivery["occurrence_id"]
                .as_str()
                .expect("an occurrence id"),
            delivery["subscription_id"]
                .as_str()
                .expect("a subscription id"),
        )
        .await
        .expect("the owed delivery's recovery starts it over the restored route");
    assert_eq!(probe.calls(), 2, "the recovery is new work, and asks once");
    assert!(
        world
            .registry()
            .get_process(&recovered)
            .await
            .expect("read the recovered process")
            .is_some(),
        "the recovery registered the delivery's process"
    );
}

/// Which surface a signal or cancel law drives.
#[derive(Clone, Copy, Debug)]
pub enum Surface {
    /// The facade's global process commands (`LashCore::processes`).
    Facade,
    /// A session's process admin, which runs the session manager's process
    /// capability: the path a replayed cell's process controls take.
    Session,
}

/// A signal answers its recorded event after its target is pruned.
pub async fn signal(kind: StorageKind, live: bool, surface: Surface, advance: Advance) {
    let world = World::new(kind, live, "signal").await;
    let target = world.target().await;
    let session = world
        .core
        .session(world.session_id.clone())
        .open()
        .await
        .expect("open the law's session");
    let operation: Operation = {
        let core = world.core.clone();
        let target = target.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            let session = session.clone();
            let target = target.clone();
            Box::pin(async move {
                match surface {
                    Surface::Facade => receipt(
                        match lash_core::ProcessSignalIdentity::new(
                            target.clone(),
                            SIGNAL,
                            "raa-signal",
                        ) {
                            Ok(identity) => {
                                core.processes()
                                    .signal(
                                        lash_core::ProcessSignal::new(
                                            identity,
                                            json!({"law": "signal"}),
                                        ),
                                        scoped,
                                    )
                                    .await
                            }
                            Err(error) => Err(error.into()),
                        },
                    ),
                    Surface::Session => receipt(
                        session
                            .admin()
                            .processes()
                            .signal(
                                &target,
                                SIGNAL,
                                "raa-signal",
                                json!({"law": "signal"}),
                                scoped,
                            )
                            .await,
                    ),
                }
            })
        })
    };
    let (original, replayed) = replay_leg(&world, "raa-signal", operation, async |_: &Value| {
        world
            .end_and_prune(&target, matches!(advance, Advance::PruneAndCompact))
            .await;
    })
    .await;
    assert_replayed_as_recorded("signal", advance, original, replayed);
}

/// A cancel answers its recorded receipt after its target is pruned.
pub async fn cancel(kind: StorageKind, live: bool, surface: Surface, advance: Advance) {
    let world = World::new(kind, live, "cancel").await;
    let target = world.target().await;
    let session = world
        .core
        .session(world.session_id.clone())
        .open()
        .await
        .expect("open the law's session");
    let operation: Operation = {
        let core = world.core.clone();
        let target = target.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            let session = session.clone();
            let target = target.clone();
            Box::pin(async move {
                match surface {
                    Surface::Facade => receipt(core.processes().cancel(&target, scoped).await),
                    Surface::Session => {
                        receipt(session.admin().processes().cancel(&target, scoped).await)
                    }
                }
            })
        })
    };
    let (original, replayed) = replay_leg(&world, "raa-cancel", operation, async |_: &Value| {
        world
            .end_and_prune(&target, matches!(advance, Advance::PruneAndCompact))
            .await;
    })
    .await;
    assert_replayed_as_recorded("cancel", advance, original, replayed);
}

/// Cancel-all keeps the recorded selection after every selected row ends
/// and is compacted. A process admitted afterward is outside that selection.
pub async fn cancel_all(kind: StorageKind, live: bool) {
    let world = World::new(kind, live, "cancel-all").await;
    let first = world.target().await;
    let second = world.target().await;
    let operation: Operation = {
        let core = world.core.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            Box::pin(async move { receipt(core.processes().cancel_all(scoped).await) })
        })
    };
    let (original, replayed) = replay_leg(
        &world,
        "raa-cancel-all",
        operation,
        async |recorded: &Value| {
            assert_eq!(recorded.as_array().expect("cancel receipts").len(), 2);
            world.end_and_prune(&first, true).await;
            world.end_and_prune(&second, true).await;
            world.target().await;
        },
    )
    .await;
    assert_replayed_as_recorded("cancel-all", Advance::PruneAndCompact, original, replayed);
}

/// The runtime's external completion returns the original admission after
/// retention removes the row or a transfer removes the observer edge.
pub async fn external_completion(kind: StorageKind, live: bool, transfer: bool) {
    let world = World::new(kind, live, "external-completion").await;
    let target = world.target().await;
    let runtime = lash_core::testing::runtime_helpers::TestRuntime::new(
        &world.engine.backend(),
        lash_core::testing::TestProvider::builder().build(),
    )
    .with_session_id(world.session_id.clone())
    .build()
    .await;
    let processes = runtime
        .process_service()
        .expect("the runtime's process service");
    let operation: Operation = {
        let session_id = world.session_id.clone();
        let target = target.clone();
        Arc::new(move |scoped| {
            let processes = Arc::clone(&processes);
            let session_id = session_id.clone();
            let target = target.clone();
            Box::pin(async move {
                receipt(
                    processes
                        .complete_external(
                            &session_id,
                            &target,
                            lash_core::ProcessAwaitOutput::from_tool_output(
                                lash_core::ToolCallOutput::success(json!({"completed": true})),
                            ),
                            lash_core::ProcessOpScope::new(scoped),
                        )
                        .await,
                )
            })
        })
    };
    let (original, replayed) = replay_leg(
        &world,
        "raa-external-completion",
        operation,
        async |_: &Value| {
            if transfer {
                world
                    .registry()
                    .transfer_observers(
                        &world.session_id,
                        &lash_core::SessionId::from("raa-new-observer"),
                        std::slice::from_ref(&target),
                        lash_core::ProcessObserverBy::host("replay-law-transfer"),
                    )
                    .await
                    .expect("transfer the observer edge");
                assert!(
                    !world
                        .registry()
                        .is_observer(&world.session_id, &target)
                        .await
                        .expect("the former observer")
                );
            } else {
                world.end_and_prune(&target, true).await;
            }
        },
    )
    .await;
    assert_replayed_as_recorded(
        "external completion",
        Advance::PruneAndCompact,
        original,
        replayed,
    );
}

/// A process await answers its recorded terminal output after the awaited
/// process is pruned and compacted: the await's existence guard and its
/// terminal are recorded, and the replay reads them back.
pub async fn attach_await(kind: StorageKind, live: bool) {
    let world = World::new(kind, live, "await").await;
    let target = world.target().await;
    world
        .registry()
        .complete_process(
            &target,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!({"awaited": true}),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("end the awaited target");
    let operation: Operation = {
        let registry = world.registry();
        let process_work = Arc::clone(world.engine.backend().process_work().port());
        let target = target.clone();
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let process_work = Arc::clone(&process_work);
            let target = target.clone();
            Box::pin(async move {
                let invocation = lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(scoped.execution_scope().clone(), "raa-await")
                        .map_err(|error| format!("{error:?}"))?,
                    lash_core::RuntimeAttribution::none(),
                    "raa-await",
                );
                let outcome = scoped
                    .execute_effect(
                        lash_core::RuntimeEffectEnvelope::new(
                            invocation,
                            lash_core::RuntimeEffectCommand::process(
                                lash_core::ProcessCommand::Await { process_id: target },
                            ),
                        ),
                        lash_core::RuntimeEffectLocalExecutor::processes(registry, process_work),
                    )
                    .await
                    .map_err(|error| format!("{error:?}"))?;
                match outcome {
                    lash_core::RuntimeEffectOutcome::Process {
                        result: lash_core::ProcessEffectOutcome::Await { output },
                    } => receipt(Ok::<_, String>(output)),
                    other => Err(format!("an await answers its terminal, got {other:?}")),
                }
            })
        })
    };
    let (original, replayed) = replay_leg(&world, "raa-await", operation, async |_: &Value| {
        world.end_and_prune(&target, true).await;
    })
    .await;
    assert_replayed_as_recorded(
        "process await",
        Advance::PruneAndCompact,
        original,
        replayed,
    );
}

/// A durable wait answers its recorded resolution after the wait was
/// resolved again with a different value: the replay reads its journal, not
/// the wait's current state.
pub async fn durable_wait(kind: StorageKind, live: bool) {
    let world = World::new(kind, live, "wait").await;
    let (keys, mut key_receiver) = tokio::sync::mpsc::unbounded_channel();
    let operation: Operation = {
        let effect_host = world.engine.backend().effect_host();
        Arc::new(move |scoped| {
            let effect_host = Arc::clone(&effect_host);
            let keys = keys.clone();
            Box::pin(async move {
                let key = effect_host
                    .await_event_key(
                        scoped.execution_scope(),
                        lash_core::AwaitEventWaitIdentity::Custom {
                            key: "raa-wait".to_string(),
                        },
                    )
                    .await
                    .map_err(|error| format!("{error:?}"))?;
                // The first attempt's key is the one the law resolves; a replay's
                // send finds the listener gone.
                let _ = keys.send(key.clone());
                let invocation = lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(scoped.execution_scope().clone(), "raa-wait")
                        .map_err(|error| format!("{error:?}"))?,
                    lash_core::RuntimeAttribution::none(),
                    "raa-wait",
                );
                let outcome = scoped
                    .execute_effect(
                        lash_core::RuntimeEffectEnvelope::new(
                            invocation,
                            lash_core::RuntimeEffectCommand::AwaitEvent { key },
                        ),
                        lash_core::RuntimeEffectLocalExecutor::await_event(
                            lash_core::CancellationToken::new(),
                            None,
                        ),
                    )
                    .await
                    .map_err(|error| format!("{error:?}"))?;
                match outcome {
                    lash_core::RuntimeEffectOutcome::AwaitEvent { resolution } => {
                        receipt(Ok::<_, String>(resolution))
                    }
                    other => Err(format!("an await answers its resolution, got {other:?}")),
                }
            })
        })
    };
    let completions = world.core.completions();
    let resolver = tokio::spawn(async move {
        let key = tokio::time::timeout(BOUND, key_receiver.recv())
            .await
            .expect("the handler mints its wait key")
            .expect("a wait key");
        completions
            .resolve(
                key.clone(),
                lash_core::Resolution::Ok(json!({"resolved": "first"})),
            )
            .await
            .expect("resolve the wait");
        (key, key_receiver)
    });
    let world_ref = &world;
    let (original, replayed) = replay_leg(world_ref, "raa-wait", operation, async |_: &Value| {
        let (key, _receiver) = resolver.await.expect("the resolver joins");
        let again = world_ref
            .core
            .completions()
            .resolve(
                key,
                lash_core::Resolution::Ok(json!({"resolved": "second"})),
            )
            .await
            .expect("a second resolution answers");
        assert!(
            matches!(again, lash_core::ResolveOutcome::AlreadyResolved { .. }),
            "the wait resolves once: {again:?}"
        );
    })
    .await;
    assert_replayed_as_recorded("durable wait", Advance::Prune, original, replayed);
}

/// A session command resubmitted under its idempotency key answers its
/// first receipt after the session moved on through later commands.
pub async fn session_command(kind: StorageKind) {
    let world = World::new(kind, false, "command").await;
    let session = world
        .core
        .session(world.session_id.clone())
        .open()
        .await
        .expect("open the law's session");
    let first = receipt(
        session
            .admin()
            .commands()
            .refresh_tool_catalog("raa-first", "raa-command")
            .await,
    )
    .expect("the first submission is admitted");
    for index in 0..3 {
        receipt(
            session
                .admin()
                .commands()
                .refresh_tool_catalog(format!("raa-later-{index}"), format!("raa-later-{index}"))
                .await,
        )
        .expect("a later command is admitted");
    }
    let resubmitted = receipt(
        session
            .admin()
            .commands()
            .refresh_tool_catalog("raa-first", "raa-command")
            .await,
    )
    .expect("the resubmission is answered");
    assert_eq!(
        resubmitted, first,
        "a resubmitted command answers its first receipt"
    );
}

/// A fresh command against a pruned target still refuses: the recorded
/// admission is what refuses, so moving it off the facade keeps the refusal.
pub async fn fresh_command_refuses_a_pruned_target(kind: StorageKind, surface: Surface) {
    let world = World::new(kind, false, "fresh").await;
    let target = world.target().await;
    world.end_and_prune(&target, false).await;
    let session = world
        .core
        .session(world.session_id.clone())
        .open()
        .await
        .expect("open the law's session");
    let operation: Operation = {
        let core = world.core.clone();
        let target = target.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            let session = session.clone();
            let target = target.clone();
            Box::pin(async move {
                match surface {
                    Surface::Facade => receipt(core.processes().cancel(&target, scoped).await),
                    Surface::Session => {
                        receipt(session.admin().processes().cancel(&target, scoped).await)
                    }
                }
            })
        })
    };
    let refused = run_once(&world, "raa-fresh", operation).await;
    let error = refused.expect_err("a fresh cancel of a pruned target refuses");
    assert!(
        error.contains("ProcessNoLongerRetained") || error.contains("process_no_longer_retained"),
        "the refusal names the retention: {error}"
    );
}
