//! FIG-4914: the Part 1 journal must park before the Part 2 transition decodes.

use super::*;
use lash_core::engine::{
    AdmissionId, Admitted, AdmittedWork, RunEnd, ShiftAbort, ShiftRequest, ShiftRequestId,
    admission_body,
};
use lash_core::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectLocalExecutor, SessionShifts,
    SessionWorkEngine,
};
use lash_restate::restate_sdk;
use lash_restate_test::{InvocationView, RestateTestServer, ServerConfig};
use restate_sdk::context::{ContextSideEffects, RunFuture, WorkflowContext};
use restate_sdk::errors::HandlerResult;
use restate_sdk::prelude::Endpoint;
use restate_sdk::serde::Json;
use restate_sdk::service::{Discoverable, Service};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SESSION: &str = "predecessor-transition";
const RUN: &str = "run";
const STEP: &str = "plugin-transition";

fn transition() -> RuntimeEffectEnvelope {
    let address = EffectAddress::new(lash_core::ExecutionScope::turn(SESSION, RUN), STEP)
        .expect("transition address");
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            address.clone(),
            RuntimeAttribution::for_session(SESSION),
            "transition",
        ),
        RuntimeEffectCommand::TransitionPlugins {
            request: Box::new(lash_core::plugin::PluginTransitionRequest {
                id: lash_core::plugin::PluginTransitionId(address),
                owner: lash_core::RuntimeOwner::Session(SESSION.into()),
                base: lash_core::plugin::PluginTransitionBase::Session {
                    head: lash_core::store::SessionHeadRef {
                        generation: 0,
                        revision: 0,
                        leaf: None,
                        checkpoint: None,
                    },
                },
                target: Default::default(),
            }),
        },
    )
}

#[derive(Clone)]
struct Predecessor {
    step: String,
    entry: serde_json::Value,
    bodies: Arc<AtomicUsize>,
    passes: Arc<AtomicUsize>,
}

#[restate_sdk::workflow(name = "LashTurn")]
impl Predecessor {
    #[restate_sdk::handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        _request: Json<serde_json::Value>,
    ) -> HandlerResult<Json<bool>> {
        let entry = self.entry.clone();
        let step = self.step.clone();
        let bodies = Arc::clone(&self.bodies);
        ctx.run(move || async move {
            bodies.fetch_add(1, Ordering::SeqCst);
            Ok(Json(entry))
        })
        .name(format!("lash:{step}"))
        .await?;
        if self.passes.fetch_add(1, Ordering::SeqCst) == 0 {
            std::future::pending::<()>().await;
        }
        Ok(Json(true))
    }
}

// The SDK's generated dispatcher has no rename operation. Dispatch the
// fixture's sole handler explicitly so both retained names use the same body.
impl Service for Predecessor {
    type Future = restate_sdk::service::macro_support::ServiceBoxFuture;

    fn handle(&self, ctx: restate_sdk::endpoint::ContextInternal) -> Self::Future {
        let predecessor = self.clone();
        Box::pin(async move {
            let (input, metadata) = ctx.input().await;
            let result = predecessor.run((&ctx, metadata).into(), input).await;
            ctx.handle_handler_result(result);
            ctx.end();
            Ok(())
        })
    }
}

impl Predecessor {
    fn endpoint(&self, generation: &BuildGeneration) -> Endpoint {
        let mut lane = Self::discover();
        lane.name = format!("LashTurn_g{generation}")
            .try_into()
            .expect("generation service name");
        Endpoint::builder()
            .bind(self.clone())
            .bind(restate_sdk::service::macro_support::service_definition(
                self.clone(),
                lane,
            ))
            .build()
    }
}

struct TransitionShifts {
    envelope: RuntimeEffectEnvelope,
    bodies: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SessionShifts for TransitionShifts {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        generation: &BuildGeneration,
        ordinal: u32,
        _draining: Option<&BuildGeneration>,
    ) -> Result<lash_core::engine::AdmitVerdict, ShiftAbort> {
        let bodies = Arc::clone(&self.bodies);
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(
                    controller.execution_scope().clone(),
                    lash_core::engine::shift_admission_replay_key(&request.request, ordinal),
                )
                .expect("current admission address"),
                RuntimeAttribution::for_session(request.session.clone()),
                format!("shift-admission-{ordinal}"),
            ),
            RuntimeEffectCommand::AdmitShift {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: request.session.clone(),
                    request: request.request.clone(),
                    run_start: lash_core::engine::RunStartNonce::new("predecessor-fixture"),
                    build_generation: generation.clone(),
                }),
            },
        );
        let result = controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    bodies.fetch_add(1, Ordering::SeqCst);
                    Err(RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::PluginSessionManager,
                        "predecessor work must never execute",
                    ))
                }),
            )
            .await;
        Err(ShiftAbort::Retry(
            result
                .expect_err("the old journal must be refused")
                .into_runtime_error(),
        ))
    }

    async fn execute_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        _admitted: Admitted,
    ) -> RunEnd {
        let bodies = Arc::clone(&self.bodies);
        let result = controller
            .execute_effect(
                self.envelope.clone(),
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    bodies.fetch_add(1, Ordering::SeqCst);
                    Err(RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::PluginSessionManager,
                        "transition work must never execute",
                    ))
                }),
            )
            .await;
        RunEnd::owing_nothing(Err(ShiftAbort::Retry(
            result
                .expect_err("the old journal must be refused")
                .into_runtime_error(),
        )))
    }

    async fn close_run(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _run: &lash_core::TurnId,
    ) -> Result<(), ShiftAbort> {
        unreachable!("the refused run owes no close")
    }
}

async fn wait_for(server: &RestateTestServer, key: &str, status: &str) -> InvocationView {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(view) = server.find_invocation("LashTurn", key, "run", status) {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:#?}",
            server.invocations()
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_untagged_transition_parks_before_decoding_or_invoking_work() {
    // Keep the feature set, format manifest, admission and plugins identical
    // to the predecessor. Only the handler logic epoch may separate its lane.
    let predecessor_epoch = if cfg!(feature = "synthetic-next") {
        3
    } else {
        2
    };
    predecessor_journal_keeps_its_lane(predecessor_epoch, PredecessorShape::UntaggedTransition)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_empty_queue_stop_rule_keeps_predecessor_journals_on_their_drain_lane() {
    let predecessor_epoch = if cfg!(feature = "synthetic-next") {
        7
    } else {
        6
    };
    predecessor_journal_keeps_its_lane(predecessor_epoch, PredecessorShape::TaggedTransition).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_prelude_refuses_a_predecessor_before_decoding_and_keeps_its_drain_lane() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::EnvironmentPrelude,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_only_result_checks_refuse_predecessor_journals_and_keep_their_drain_lane() {
    let predecessor_epoch = if cfg!(feature = "synthetic-next") {
        17
    } else {
        16
    };
    predecessor_journal_keeps_its_lane(predecessor_epoch, PredecessorShape::ResultCheckCommands)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callback_session_contributions_refuse_predecessor_journals_and_keep_their_drain_lane() {
    let epoch = if cfg!(feature = "synthetic-next") {
        20
    } else {
        19
    };
    predecessor_journal_keeps_its_lane(epoch, PredecessorShape::TurnCallbacks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_root_admission_refuses_a_predecessor_before_decoding_and_keeps_its_drain_lane() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::RootAdmission,
    )
    .await;
}

/// L21 / FIG-4944: native turn activation now precedes Run config resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_activation_refuses_predecessor_before_decode_and_retains_drain() {
    // The malformed transition makes decoding visible: generation refusal
    // must happen first, and restoring the old build must retain its result.
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::UntaggedTransition,
    )
    .await;
}

/// L21: binding a Run's process launch to the live executor changes its
/// journaled command stream. The epoch alone must refuse a retained journal
/// even when its effect entry still decodes under the current stored format.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_process_launch_keeps_predecessor_journals_on_their_drain_lane() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::TaggedTransition,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_run_environment_refuses_predecessor_journals_and_keeps_their_drain_lane() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::RootAdmission,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn borrowed_run_schedules_refuse_predecessor_journals_and_keep_their_drain_lane() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::RootAdmission,
    )
    .await;
}

enum PredecessorShape {
    UntaggedTransition,
    TaggedTransition,
    RootAdmission,
    EnvironmentPrelude,
    ResultCheckCommands,
    TurnCallbacks,
}

async fn predecessor_journal_keeps_its_lane(predecessor_epoch: u32, shape: PredecessorShape) {
    let recorded = build_generation_of(
        durable_formats(),
        Some(predecessor_epoch),
        &SessionAdmissionWindow::of_this_build(),
        &bare(),
    );
    let executing = bare_generation();
    assert_ne!(
        recorded, executing,
        "changed handler logic needs a new lane"
    );
    let envelope = match shape {
        PredecessorShape::TurnCallbacks => RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(
                    lash_core::ExecutionScope::turn(SESSION, RUN),
                    "plugin-callbacks:after-turn",
                )
                .unwrap(),
                RuntimeAttribution::for_session(SESSION),
                "callbacks",
            ),
            RuntimeEffectCommand::PluginCallbacks {
                phase: lash_core::plugin::RecordedCallbackPhase::AfterTurn,
            },
        ),
        PredecessorShape::RootAdmission => RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(
                    lash_core::ExecutionScope::turn(SESSION, RUN),
                    "shift-admission:predecessor#0",
                )
                .unwrap(),
                RuntimeAttribution::for_session(SESSION),
                "root-admission",
            ),
            RuntimeEffectCommand::AdmitShift {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: SESSION.into(),
                    request: ShiftRequestId::new("predecessor"),
                    build_generation: executing.clone(),
                    run_start: lash_core::engine::RunStartNonce::new("fixture"),
                }),
            },
        ),
        PredecessorShape::EnvironmentPrelude => RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(lash_core::ExecutionScope::turn(SESSION, RUN), "prelude")
                    .unwrap(),
                RuntimeAttribution::for_session(SESSION),
                "prelude",
            ),
            RuntimeEffectCommand::SyncExecutionEnvironment,
        ),
        PredecessorShape::ResultCheckCommands => RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(
                    lash_core::ExecutionScope::turn(SESSION, RUN),
                    "plugin-state:c1:cached",
                )
                .unwrap(),
                RuntimeAttribution::for_session(SESSION),
                "result checks",
            ),
            RuntimeEffectCommand::PluginCallbacks {
                phase: lash_core::plugin::RecordedCallbackPhase::BeforeTurn,
            },
        ),
        _ => transition(),
    };
    let mut old_envelope: serde_json::Value =
        serde_json::from_str(envelope.canonical_form().unwrap().json()).unwrap();
    if matches!(shape, PredecessorShape::UntaggedTransition) {
        let base = &mut old_envelope["command"]["request"]["base"];
        *base = base["head"].take();
        let decode = serde_json::from_value::<lash_core::plugin::PluginTransitionRequest>(
            old_envelope["command"]["request"].clone(),
        )
        .expect_err("the Part 1 head has no Part 2 kind tag");
        assert!(decode.to_string().contains("kind"), "{decode}");
    }
    if matches!(shape, PredecessorShape::ResultCheckCommands) {
        old_envelope["command"]["phase"] = serde_json::json!({
            "phase": "tool_result_checks",
            "call_id": lash_core::ToolCallId::fixture("c1"),
            "occurrence": lash_core::plugin::ToolHookOccurrence::Cached,
        });
        let refusal =
            serde_json::from_value::<lash_core::RuntimeEffectEnvelope>(old_envelope.clone())
                .expect_err("the result-check state-only phase is removed");
        assert!(
            refusal.to_string().contains("tool_result_checks"),
            "{refusal}"
        );
    }
    let old_json = serde_json::to_string(&old_envelope).unwrap();
    let mut hasher = Blake3DomainHasher::new("lash-runtime-effect-envelope/v3");
    hasher.update(old_json.as_bytes());
    let old_hash = hasher.finalize_hex();
    let outcome = match shape {
        PredecessorShape::ResultCheckCommands => serde_json::json!({
            "type": "plugin_callbacks",
            "result": {"Ok": []},
        }),
        PredecessorShape::TurnCallbacks => serde_json::json!({
            "type": "plugin_callbacks", "result": {"Ok": [{"plugin_id": "predecessor"}]}
        }),
        PredecessorShape::RootAdmission => {
            let outcome = serde_json::json!({"type":"admit_shift", "verdict":{"verdict":"admit", "session":SESSION, "run":RUN}});
            assert!(
                serde_json::from_value::<lash_core::RuntimeEffectOutcome>(outcome.clone()).is_err(),
                "the predecessor's unsealed selection is not a root receipt"
            );
            outcome
        }

        PredecessorShape::EnvironmentPrelude => {
            let outcome = serde_json::json!({
                "type": "sync_execution_environment",
                "result": {"Ok": lash_core::sansio::ExecutionEnvironmentSync::default()},
                "tool_surface": [],
            });
            let refusal =
                serde_json::from_value::<lash_core::RuntimeEffectOutcome>(outcome.clone())
                    .expect_err("the predecessor sync has no prelude");
            assert!(refusal.to_string().contains("prelude"), "{refusal}");
            outcome
        }
        _ => serde_json::json!({
            "type": "transition_plugins",
            "record": {
                "request": old_envelope["command"]["request"],
                "source": "blake3:predecessor-state",
                "namespaces": {},
                "config": { "Ok": {} },
                "publication": null,
            },
        }),
    };
    let predecessor = Predecessor {
        step: envelope.invocation.effect_replay_key().to_string(),
        entry: serde_json::json!({
            "build_generation": recorded,
            "effect_journal_version": crate::restate::EFFECT_JOURNAL_VERSION,
            "envelope": { "json": old_json, "hash": old_hash },
            "outcome": { "Ok": outcome },
        }),
        bodies: Arc::default(),
        passes: Arc::default(),
    };
    if matches!(shape, PredecessorShape::TaggedTransition) {
        serde_json::from_value::<lash_core::RuntimeEffectOutcome>(
            predecessor.entry["outcome"]["Ok"].clone(),
        )
        .expect("the immediately preceding generation wrote a tagged transition record");
    }
    let server =
        RestateTestServer::new(ServerConfig::default().with_seed(0x4914)).expect("server double");
    let deployment = server
        .register(predecessor.endpoint(&recorded))
        .await
        .unwrap();
    let connection =
        lash_restate::RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = lash_restate::RestateIngressClient::new(connection.clone());
    let request = serde_json::json!({
        "sender_generation": recorded,
        "admitted": {"run": RUN},
    });
    let key = "22:predecessor-transitionrun".to_owned();
    ingress
        .send_workflow_json("LashTurn", &key, "run", &lash_restate::Call::new(&request))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while predecessor.passes.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the predecessor did not record"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let invocation = wait_for(&server, &key, "running").await;
    let journal_len = server.journal(&invocation.id).unwrap().len();
    let engine = lash_restate::RestateEngine::new(
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
        lash_restate::RestateConfig::new(
            connection.clone(),
            connection,
            lash_restate::RestateAuthorityId::new("transition-generation").unwrap(),
        )
        .stamped(executing.clone()),
    );
    let shifts = Arc::new(TransitionShifts {
        envelope,
        bodies: Arc::default(),
    });
    let _installation = engine
        .session_work_engine()
        .install_session_shifts(shifts.clone());
    let endpoint = || {
        engine
            .endpoint_builder(lash_restate::RestateProcessWorkerSlot::new())
            .unwrap()
            .build()
    };
    server
        .restart_deployment(&deployment, endpoint())
        .await
        .unwrap();
    let refused = wait_for(&server, &key, "paused").await;
    let (_, message) = refused.last_failure.expect("generation refusal");
    assert!(
        message.contains("RetiredGeneration"),
        "expected generation refusal before decoding: {message}"
    );
    assert!(
        message.contains(recorded.as_str()) && message.contains(executing.as_str()),
        "{message}"
    );
    assert_eq!(
        shifts.bodies.load(Ordering::SeqCst),
        0,
        "no new transition work"
    );
    assert_eq!(
        server.journal(&invocation.id).unwrap().len(),
        journal_len,
        "no new command"
    );

    // Restore the compatible build, then register the new build separately.
    // Its registration leaves the predecessor's drain lane and journal owned
    // by the predecessor rather than moving them to the latest stable name.
    server
        .restart_deployment(&deployment, predecessor.endpoint(&recorded))
        .await
        .unwrap();
    server.register(endpoint()).await.unwrap();
    assert_eq!(server.resume(&invocation.id), Some(true));
    let restored = wait_for(&server, &key, "completed").await;
    assert_eq!(restored.pinned_deployment_id, deployment.as_str());
    assert_eq!(
        predecessor.bodies.load(Ordering::SeqCst),
        1,
        "durable result replays"
    );
    let lane = format!("LashTurn_g{recorded}");
    let admitted = {
        let receipt_session: lash_core::SessionId = SESSION.into();
        let receipt_admission = AdmissionId::new("shift#0");
        admission_body::admitted(
            receipt_session.clone(),
            ShiftRequestId::new("shift"),
            receipt_admission.clone(),
            recorded.clone(),
            lash_core::store::ShiftAdmissionReceipt {
                selection: lash_core::store::ShiftAdmissionSelection {
                    run: RUN.into(),
                    work: AdmittedWork::Queued {
                        head: "head".into(),
                    },
                    observed_epoch: 0,
                },
                run_start: lash_core::store::RunStartNonce::new(receipt_admission.as_str()),
                seal: lash_core::store::ShiftEpochSeal::Sealed(
                    lash_core::store_backend_support::sealed_shift_fence(
                        receipt_session.clone(),
                        1,
                        receipt_admission.clone(),
                    ),
                ),
                cancel_intent: lash_core::TurnCancelIntentSnapshot::Absent,
                run_admission: None,
            },
        )
    };
    let _: bool = ingress
        .call_workflow_json(
            &lane,
            "22:predecessor-transitionrun",
            "run",
            &lash_restate::Call::new(serde_json::json!({
                "sender_generation": recorded,
                "admitted": admitted,
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        predecessor.bodies.load(Ordering::SeqCst),
        2,
        "the drain lane still reaches the old build"
    );
    assert_eq!(shifts.bodies.load(Ordering::SeqCst), 0);
}

/// L21 / FIG-4944: Run-owned terminal subscriptions change the handler commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_terminal_attachment_refuses_predecessor_before_decode_and_retains_drain() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::UntaggedTransition,
    )
    .await;
}

/// L21 / S04 F1: replacing the admission state changes the turn's recorded commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stamped_turn_state_refuses_predecessor_before_decode_and_retains_drain() {
    predecessor_journal_keeps_its_lane(
        crate::restate::JOURNAL_LOGIC_EPOCH - 1,
        PredecessorShape::RootAdmission,
    )
    .await;
}

/// FIG-5020: offline deployment decisions and boot bind the same generation.
#[cfg(all(feature = "restate", feature = "sqlite"))]
#[tokio::test]
async fn offline_composed_generation_equals_the_booted_core() -> crate::Result<()> {
    use std::sync::Arc;
    let protocol: Arc<dyn PluginFactory> =
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new());
    let plugin: Arc<dyn PluginFactory> = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("host-generation"),
        PluginSpec::new(),
    ));
    let composition = crate::plugins::PluginHost::new(vec![protocol.clone(), plugin.clone()])
        .with_protocol_plugin(protocol.clone())
        .composition()?;
    let offline = composed_generation(&composition);
    let connection = crate::restate::RestateConnection::new("https://restate.invalid");
    let engine = crate::restate::RestateEngine::new(
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
        crate::restate::RestateConfig::new(
            connection.clone(),
            connection,
            crate::restate::RestateAuthorityId::new("offline-generation").unwrap(),
        ),
    );
    let core = crate::tests::explicit_ephemeral_facets(crate::LashCore::builder(
        lash_core::Backend::new(Arc::new(engine)),
    ))
    .protocol_plugin(protocol)
    .plugin(plugin)
    .build(crate::testing::runtime_lease_owner())?;
    assert_eq!(&offline, core.build_generation());
    assert_eq!(&offline, core.backend().build_generation()?);
    Ok(())
}
