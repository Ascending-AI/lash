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
        let bodies = Arc::clone(&self.bodies);
        ctx.run(move || async move {
            bodies.fetch_add(1, Ordering::SeqCst);
            Ok(Json(entry))
        })
        .name(format!("lash:{STEP}"))
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

#[derive(Default)]
struct TransitionShifts {
    bodies: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SessionShifts for TransitionShifts {
    async fn admit(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _request: &ShiftRequest,
        _generation: &BuildGeneration,
        _ordinal: u32,
        _draining: Option<&BuildGeneration>,
    ) -> Result<lash_core::engine::AdmitVerdict, ShiftAbort> {
        unreachable!("the witness sends the recorded run directly")
    }

    async fn execute_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        _admitted: Admitted,
    ) -> RunEnd {
        let bodies = Arc::clone(&self.bodies);
        let result = controller
            .execute_effect(
                transition(),
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
    let recorded = build_generation_of(
        durable_formats(),
        Some(predecessor_epoch),
        &SessionAdmissionWindow::of_this_build(),
        &bare(),
    );
    let executing = bare_generation();
    let mut old_envelope: serde_json::Value =
        serde_json::from_str(transition().canonical_form().unwrap().json()).unwrap();
    let base = &mut old_envelope["command"]["request"]["base"];
    *base = base["head"].take();
    let decode = serde_json::from_value::<lash_core::plugin::PluginTransitionRequest>(
        old_envelope["command"]["request"].clone(),
    )
    .expect_err("the Part 1 head has no Part 2 kind tag");
    assert!(decode.to_string().contains("kind"), "{decode}");
    let old_json = serde_json::to_string(&old_envelope).unwrap();
    let mut hasher = Blake3DomainHasher::new("lash-runtime-effect-envelope/v3");
    hasher.update(old_json.as_bytes());
    let old_hash = hasher.finalize_hex();
    let predecessor = Predecessor {
        entry: serde_json::json!({
            "build_generation": recorded,
            "effect_journal_version": crate::restate::EFFECT_JOURNAL_VERSION,
            "envelope": { "json": old_json, "hash": old_hash },
            "outcome": { "Ok": {
                "type": "transition_plugins",
                "record": {
                    "request": old_envelope["command"]["request"],
                    "source": "blake3:predecessor-state",
                    "namespaces": {},
                    "config": { "Ok": {} },
                    "publication": null,
                },
            } },
        }),
        bodies: Arc::default(),
        passes: Arc::default(),
    };
    let server =
        RestateTestServer::new(ServerConfig::default().with_seed(0x4914)).expect("server double");
    let deployment = server
        .register(predecessor.endpoint(&recorded))
        .await
        .unwrap();
    let connection =
        lash_restate::RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = lash_restate::RestateIngressClient::new(connection.clone());
    let request = lash_restate::RestateRunRequest {
        sender_generation: Some(recorded.clone()),
        admitted: admission_body::admitted(
            SESSION.into(),
            RUN.into(),
            ShiftRequestId::new("shift"),
            AdmissionId::new("shift:0"),
            0,
            recorded.clone(),
            AdmittedWork::Queued {
                head: "commands".into(),
            },
        ),
    };
    let key = lash_restate::turn_workflow_key(&SESSION.into(), &RUN.into());
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
    let shifts = Arc::new(TransitionShifts::default());
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
    let _: bool = ingress
        .call_workflow_json(&lane, "drain", "run", &lash_restate::Call::new(&request))
        .await
        .unwrap();
    assert_eq!(
        predecessor.bodies.load(Ordering::SeqCst),
        2,
        "the drain lane still reaches the old build"
    );
    assert_eq!(shifts.bodies.load(Ordering::SeqCst), 0);
}
