//! RLM frame evidence for ADR 0113. The Restate double runs the real engine.

#![cfg(all(
    feature = "rlm",
    feature = "restate",
    feature = "sqlite",
    feature = "testing"
))]
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::{LashCore, TurnInput, TurnOutput};
use lash_sansio::sync::MutexExt;

#[path = "artifact_referrers_evidence/fixture.rs"]
mod fixture;
use fixture::Fixture;

#[tokio::test]
async fn stored_module_refusals_preserve_causes_and_terminal_semantics() {
    use lash_core::ProcessEngine as _;
    use lash_lashlang_runtime::{LashlangProcessEngine, LashlangProcessInput, LashlangSurface};
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    use lashlang::testing::ast_builders as b;

    let artifact = lashlang::ModuleArtifact::from_program(b::module(
        vec![b::process_returning(
            "refused",
            Vec::new(),
            lashlang::TypeExpr::Null,
            b::finish(b::null()),
        )],
        Vec::new(),
    ))
    .expect("valid module");
    let value: serde_json::Value =
        serde_json::from_slice(&artifact.to_store_bytes().unwrap()).unwrap();
    let mut generation = value.clone();
    generation["family"] = serde_json::json!("unsupported-family");
    let mut corrupt_hash = value.clone();
    corrupt_hash["artifact"]["module_ref"] = serde_json::json!("forged-module-ref");
    let mut unlifted = value;
    unlifted["artifact"]["ir"]["main"] = serde_json::to_value(b::list(vec![b::process_literal(
        Vec::new(),
        b::finish(b::null()),
    )]))
    .unwrap();
    for (index, (bytes, generation)) in [
        (b"invalid JSON".to_vec(), false),
        (serde_json::to_vec(&generation).unwrap(), true),
        (serde_json::to_vec(&corrupt_hash).unwrap(), false),
        (serde_json::to_vec(&unlifted).unwrap(), false),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(0x4654_0000 + index as u64).await;
        let backend = fixture.double.lash_backend();
        let store = lashlang::LashlangArtifacts::new(backend.module_artifacts());
        let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
            lash_core::HostArtifactPin::mint(),
        ))
        .unwrap();
        store
            .store()
            .publish_module_artifact(&claim, artifact.module_ref().as_str(), &bytes)
            .await
            .expect("persist immutable refused bytes");
        let expected: lashlang::ModuleArtifactRefusal =
            lashlang::ModuleArtifact::from_store_bytes(&bytes)
                .unwrap_err()
                .into();
        assert_eq!(
            matches!(expected, lashlang::ModuleArtifactRefusal::Generation(_)),
            generation
        );
        let verification = lash_vm_client::service::Service::default()
            .request_accounted(lash_vm_client::service::Request::VerifyArtifact {
                bytes: bytes.clone(),
            })
            .await
            .unwrap();
        let wire = rmp_serde::to_vec_named(&verification).unwrap();
        let replay: lash_vm_client::service::Response = rmp_serde::from_slice(&wire).unwrap();
        let lash_vm_client::service::Response::ArtifactVerification(
            lash_vm_client::service::ArtifactVerification::Refused(refusal),
        ) = replay
        else {
            panic!("worker must refuse the stored bytes: {replay:?}");
        };
        assert_eq!(refusal, expected, "worker preserves the complete cause");
        let refusal = store
            .get_module_artifact(artifact.module_ref())
            .await
            .unwrap_err();
        let plugin: lash_core::PluginError = refusal.into();
        assert!(plugin.is_terminal(), "permanent typed refusal: {plugin:?}");
        assert!(!plugin.is_retryable());
        let serialized = serde_json::to_value(&plugin).unwrap();
        assert_eq!(
            serialized["message"]["cause"]["refusal"],
            serde_json::to_value(&expected).unwrap(),
            "store and plugin preserve the complete cause"
        );
        let input = LashlangProcessInput {
            module_ref: artifact.module_ref().clone(),
            process_ref: artifact.process_ref("refused").unwrap().clone(),
            host_requirements_ref: artifact.host_requirements_ref().clone(),
            process_name: "refused".into(),
            args: serde_json::Map::new(),
        };
        let engine = LashlangProcessEngine::new(
            store,
            LashlangSurface::default(),
            backend.worker_recovery(),
        );
        let registration = lash_core::ProcessRegistration::new(
            input.to_process_input().unwrap(),
            lash_core::ProcessProvenance::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
            input.process_identity(),
        ))
        .with_execution_env_ref(Some(
            lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref())
                .await,
        ));
        let registry = backend.process_registry();
        let record = registry
            .register_process(registration.clone())
            .await
            .expect("register the admitted process");
        let context = lash_core::testing::process_engine_run_context_for_validation(
            &backend,
            registration,
            Arc::new(lash_core::ToolCatalog::default()),
            false,
        );
        let before = fixture.double.server().stats();
        let outcome = engine
            .run(context, serde_json::to_value(input).unwrap())
            .await
            .unwrap();
        let terminal = outcome.terminal_output().unwrap();
        let encoded = serde_json::to_vec(terminal).unwrap();
        let decoded: lash_core::ProcessAwaitOutput = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(&decoded, terminal, "durable terminal retains all evidence");
        let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = &decoded else {
            panic!("artifact refusal must abandon before effects: {decoded:?}");
        };
        let lash_core::AbandonWriter::ResumeRefused { reason } = &evidence.writer else {
            panic!("engine must own the refusal: {evidence:?}");
        };
        match expected {
            lashlang::ModuleArtifactRefusal::Generation(_) => assert_eq!(
                reason,
                &lash_core::ProcessResumeRefusal::RetiredGeneration {
                    found: artifact.module_ref().to_string(),
                }
            ),
            lashlang::ModuleArtifactRefusal::Corrupt(source) => assert_eq!(
                reason,
                &lash_core::ProcessResumeRefusal::StoredArtifactCorrupt {
                    artifact_ref: artifact.module_ref().to_string(),
                    source,
                }
            ),
        }
        let remote = lash_remote_protocol::RemoteProcessAwaitOutput::try_from(decoded.clone())
            .expect("remote terminal");
        let remote: lash_remote_protocol::RemoteProcessAwaitOutput =
            serde_json::from_value(serde_json::to_value(remote).unwrap()).unwrap();
        assert_eq!(
            lash_core::ProcessAwaitOutput::try_from(remote).unwrap(),
            decoded,
            "remote peer retains all terminal evidence"
        );
        assert_eq!(
            fixture.double.server().stats(),
            before,
            "refuse before any effect"
        );
        for _ in 0..2 {
            registry
                .complete_process(
                    &record.id,
                    decoded.clone(),
                    lash_core::ProcessCompletionAuthority::workflow_key(record.id.to_string()),
                )
                .await
                .expect("store or replay the refused terminal");
        }
        let retained = registry
            .get_process(&record.id)
            .await
            .expect("read durable terminal")
            .expect("retained process");
        assert_eq!(retained.outcome.as_ref(), Some(&decoded));
        let events = registry
            .recent_events(&record.id, 4)
            .await
            .expect("read durable terminal event");
        let terminals: Vec<_> = events
            .iter()
            .filter_map(|event| event.semantics.terminal.as_ref())
            .collect();
        assert_eq!(terminals.len(), 1, "replay writes one terminal event");
        assert_eq!(terminals[0].outcome, decoded, "event retains the cause");
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    artifact_ref: String,
    kind: String,
    id: String,
}

async fn wait_edges(fixture: &Fixture, predicate: impl Fn(&[Edge]) -> bool) -> Vec<Edge> {
    loop {
        let edges = fixture.edges().await;
        if predicate(&edges) {
            return edges;
        }
        fixture.reconcile().await;
    }
}

fn frame_artifacts(edges: &[Edge]) -> BTreeSet<&str> {
    edges
        .iter()
        .filter(|edge| edge.kind == "frame_environment")
        .map(|edge| edge.artifact_ref.as_str())
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "the evidence fixture must fail on a store read error"
)]
async fn wait_definition_reclaimed(
    fixture: &Fixture,
    id: &lash_core::ProcessDefinitionId,
    module_ref: &str,
) {
    let backend = fixture.double.lash_backend();
    loop {
        let descriptor = backend
            .definition_store()
            .get_process_definition(id)
            .await
            .expect("read definition reclamation");
        let module = backend
            .module_artifacts()
            .get_module_artifact(module_ref)
            .await
            .expect("read module reclamation");
        if descriptor.is_none() && module.is_none() {
            return;
        }
        fixture.reconcile().await;
    }
}

/// Move past the former deadline after a real store read. Manual polling
/// checks the pending wait without letting a paused clock auto-advance
/// PostgreSQL's connection timeouts.
#[tokio::test]
async fn wait_edges_waits_for_condition_past_former_deadline() {
    use futures_util::FutureExt as _;

    let fixture = Fixture::new(0x4232_0001).await;
    let ready = std::sync::atomic::AtomicBool::new(false);
    let observed = tokio::sync::Notify::new();
    let waiting = wait_edges(&fixture, |edges| {
        observed.notify_one();
        ready.load(std::sync::atomic::Ordering::Relaxed) && edges.is_empty()
    });
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("the state wait returned before the condition held"),
        () = observed.notified() => {}
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(11)).await;
    assert!(
        waiting.as_mut().now_or_never().is_none(),
        "the state wait remains pending past the former deadline"
    );
    tokio::time::resume();
    ready.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(waiting.await.is_empty());
}

fn last_cell_finish(output: &TurnOutput) -> Option<serde_json::Value> {
    output
        .result
        .state
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } if event.plugin_id == "rlm_protocol" => event
                .payload
                .get("RlmTrajectoryEntry")?
                .get("final_output")
                .cloned(),
            _ => None,
        })
        .next_back()
}

fn response(code: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<typescript>\n{code}\n</typescript>"),
            response_meta: None,
        }],
        ..Default::default()
    }
}

fn rlm_core(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    responses: Vec<LlmResponse>,
) -> LashCore {
    rlm_core_with_queue(double, Arc::new(Mutex::new(VecDeque::from(responses))))
}

fn rlm_core_with_queue(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
) -> LashCore {
    rlm_core_with_plugins(double, queue, Vec::new())
}

#[expect(clippy::expect_used, reason = "test fixture validates its setup")]
fn rlm_core_with_plugins(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> LashCore {
    let provider = lash::testing::TestProvider::builder()
        .kind("artifact-referrers")
        .complete(move |_request| {
            let queue = Arc::clone(&queue);
            async move {
                Ok(queue
                    .lock_recover()
                    .pop_front()
                    .expect("scripted response queue is exhausted"))
            }
        })
        .build()
        .into_handle();
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(double.lash_backend())
        .with_session_work(double.explicit_reconcile_session_work())
        .into_backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let builder = LashCore::rlm_builder(backend, factory);
    let builder = plugins
        .into_iter()
        .fold(builder, |builder, plugin| builder.plugin(plugin));
    builder
        .serve_test_model(
            provider,
            lash::ModelMetadata::builder("artifact-referrers")
                .context_window_tokens(16_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "artifact-referrers-worker",
            "artifact-referrers-boot",
        ))
        .expect("RLM core")
}

#[expect(
    clippy::expect_used,
    reason = "test fixture installs its process worker"
)]
fn serve_processes(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    core: &LashCore,
) {
    let worker = lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("process-worker config"),
    )
    .expect("process worker");
    double.install_process_worker(worker);
}

#[tokio::test]
async fn cold_reopen_globals_across_turns() {
    let fixture = Fixture::new(0x4031_0001).await;
    let double = &fixture.double;
    // A reopened core may use either provider handle for the same route.
    // Both handles therefore draw from the two-turn script in call order.
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response("const saved = async () => 7; finish('bound');"),
        response("const run = await processes.start({ definition: saved }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(double, Arc::clone(&responses));
    let first_session = created_session(&first_core, "artifact-referrers-cold-reopen")
        .await
        .open()
        .await
        .expect("open first session");
    let first = first_session
        .send(TurnInput::text("bind definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first.is_success());
    let first_edges = wait_edges(&fixture, |edges| {
        edges.len() == 2
            && edges.iter().any(|edge| edge.kind == "frame_environment")
            && edges.iter().any(|edge| edge.kind == "execution")
    })
    .await;
    let frame_id = first_edges
        .iter()
        .find(|edge| edge.kind == "frame_environment")
        .expect("frame edge")
        .id
        .clone();
    let settled = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment" && edges[0].id == frame_id
    })
    .await;
    assert_eq!(settled.len(), 1, "the settled first turn holds no edge");
    drop(first_session);
    drop(first_core);

    let second_core = rlm_core_with_queue(double, Arc::clone(&responses));
    serve_processes(double, &second_core);
    let second_session = created_session(&second_core, "artifact-referrers-cold-reopen")
        .await
        .open()
        .await
        .expect("cold reopen");
    let second = second_session
        .send(TurnInput::text("use definition"))
        .output()
        .await
        .expect("second turn");
    assert!(
        second.is_success(),
        "a global from the first turn survives cold reopen: {second:?}"
    );
    assert!(
        responses.lock_recover().is_empty(),
        "each of the two turns consumes one scripted response"
    );
    assert_eq!(last_cell_finish(&second), Some(serde_json::json!(7)));
    let after_start = wait_edges(&fixture, |edges| {
        edges
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == frame_id)
    })
    .await;
    assert_eq!(
        frame_artifacts(&after_start).len(),
        1,
        "both turns use one frame"
    );
}

#[tokio::test]
async fn overwrite_retains_old_module_until_frame_end() {
    let fixture = Fixture::new(0x4031_0002).await;
    let double = &fixture.double;
    let core = rlm_core(
        double,
        vec![
            response(
                "let holder = new Map(); { const old = async () => 11; holder.set('saved', old); } finish('first');",
            ),
            response(
                "{ const newer = async () => 22; holder.set('saved', newer); } finish('second');",
            ),
            response("await control.continue_as({ task: 'new frame' });"),
            response("finish('new frame');"),
        ],
    );
    let session = created_session(&core, "artifact-referrers-overwrite")
        .await
        .open()
        .await
        .expect("session");
    let first_turn = session
        .send(TurnInput::text("bind first"))
        .output()
        .await
        .expect("first turn");
    assert!(first_turn.is_success(), "first binding: {first_turn:?}");
    let first = wait_edges(&fixture, |edges| frame_artifacts(edges).len() == 1).await;
    let old_ref = frame_artifacts(&first)
        .into_iter()
        .next()
        .expect("old module")
        .to_owned();
    assert!(
        session
            .send(TurnInput::text("overwrite"))
            .output()
            .await
            .expect("second turn")
            .is_success()
    );
    let rebound = wait_edges(&fixture, |edges| frame_artifacts(edges).len() == 2).await;
    assert!(
        frame_artifacts(&rebound).contains(old_ref.as_str()),
        "overwrite retains the old frame edge"
    );
    assert!(
        session
            .send(TurnInput::text("switch"))
            .output()
            .await
            .expect("switch turn")
            .is_success()
    );
    let after = wait_edges(&fixture, |edges| {
        !edges.iter().any(|edge| edge.artifact_ref == old_ref)
    })
    .await;
    assert!(
        !after.iter().any(|edge| edge.artifact_ref == old_ref),
        "old bytes are reclaimed after frame end"
    );
}

#[tokio::test]
async fn continue_as_carries_only_seeded_definition() {
    let fixture = Fixture::new(0x4031_0003).await;
    let double = &fixture.double;
    let core = rlm_core(
        double,
        vec![
            response("const carried = async () => 11; finish('carried bound');"),
            response("const dropped = async () => 22; finish('dropped bound');"),
            response("await control.continue_as({ task: 'use seed', seed: { carried } });"),
            response(
                "const run = await processes.start({ definition: carried }); finish(await run);",
            ),
            response(
                "const run = await processes.start({ definition: carried }); finish(await run);",
            ),
        ],
    );
    serve_processes(double, &core);
    let session = created_session(&core, "artifact-referrers-carry")
        .await
        .open()
        .await
        .expect("session");
    let first_turn = session
        .send(TurnInput::text("bind carried definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first_turn.is_success(), "first binding: {first_turn:?}");
    let carried_before = wait_edges(&fixture, |edges| frame_artifacts(edges).len() == 1).await;
    let carried_ref = frame_artifacts(&carried_before)
        .into_iter()
        .next()
        .expect("carried module")
        .to_owned();
    assert!(
        session
            .send(TurnInput::text("bind dropped definition"))
            .output()
            .await
            .expect("second turn")
            .is_success()
    );
    let before = wait_edges(&fixture, |edges| frame_artifacts(edges).len() == 2).await;
    let dropped_ref = frame_artifacts(&before)
        .into_iter()
        .find(|artifact_ref| *artifact_ref != carried_ref)
        .expect("second definition has its own module")
        .to_owned();
    let old_frame = before
        .iter()
        .find(|edge| edge.kind == "frame_environment")
        .expect("frame edge")
        .id
        .clone();
    let switched = session
        .send(TurnInput::text("carry one"))
        .output()
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "carry switch: {switched:?}");
    let after = wait_edges(&fixture, |edges| {
        let frame: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == "frame_environment")
            .collect();
        frame.len() == 1
            && frame[0].id != old_frame
            && frame[0].artifact_ref == carried_ref
            && !edges.iter().any(|edge| edge.artifact_ref == dropped_ref)
    })
    .await;
    assert_eq!(frame_artifacts(&after).len(), 1);
    assert!(
        !after.iter().any(|edge| edge.id == old_frame),
        "the ended frame holds no edge"
    );
    let result = session
        .send(TurnInput::text("run carried definition"))
        .output()
        .await
        .expect("new-frame turn");
    assert!(result.is_success());
    assert_eq!(last_cell_finish(&result), Some(serde_json::json!(11)));
}

/// A context-pressure hook that opens a frame seeded with an RLM seed once
/// the test hands it the seed's value, and continues otherwise.
struct SeedingPressureHook {
    seed: Arc<Mutex<Option<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl lash_core::plugin::ContextPressureHook for SeedingPressureHook {
    fn id(&self) -> &'static str {
        "artifact-referrers.seeding_pressure"
    }

    async fn decide(
        &self,
        _ctx: &lash_core::plugin::ContextPressureContext<'_>,
    ) -> Result<lash_core::plugin::ContextPressureDecision, lash_core::plugin::ContextError> {
        let Some(seed) = self.seed.lock_recover().take() else {
            return Ok(lash_core::plugin::ContextPressureDecision::Continue);
        };
        let seed = lash_protocol_rlm::RlmSeed::from_seed_value(&seed)
            .map_err(lash_core::plugin::ContextError::Pipeline)?;
        Ok(lash_core::plugin::ContextPressureDecision::OpenFrame {
            records: Vec::new(),
            task: "pressure frame seeded with a definition".to_string(),
            seed: lash_protocol_rlm::rlm_seed_initial_nodes(seed),
        })
    }
}

/// FIG-4134: a context-pressure hook's RLM seed carries its module-backed
/// values into the frame it opens, exactly as a `continue_as` seed does
/// (ADR 0113 §3.1). The hook seeds a definition the first frame bound; once
/// the old frame's cleanup settles, the new frame alone holds the module's
/// edge, and after a cold reopen the seeded definition still starts.
#[tokio::test]
async fn a_pressure_seed_carries_its_module_into_the_new_frame() {
    let fixture = Fixture::new(0x4134_0001).await;
    let double = &fixture.double;
    let session_id = "artifact-referrers-pressure-seed";
    let run_carried =
        "const run = await processes.start({ definition: carried }); finish(await run);";
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response("const carried = async () => 13; finish(carried);"),
        response(run_carried),
        response(run_carried),
    ])));
    let seed = Arc::new(Mutex::new(None));
    let hook: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "artifact-referrers-seeding-pressure",
            lash_core::facade_support::PluginSpec::new().with_context_pressure_hook(
                100,
                Arc::new(SeedingPressureHook {
                    seed: Arc::clone(&seed),
                }),
            ),
        ));
    let core = rlm_core_with_plugins(double, Arc::clone(&responses), vec![Arc::clone(&hook)]);
    serve_processes(double, &core);
    let session = created_session(&core, session_id)
        .await
        .open()
        .await
        .expect("session");
    let bound = session
        .send(TurnInput::text("bind the definition"))
        .output()
        .await
        .expect("binding turn");
    assert!(bound.is_success(), "binding: {bound:?}");
    let definition = last_cell_finish(&bound).expect("the cell finished with the definition");
    let before = wait_edges(&fixture, |edges| frame_artifacts(edges).len() == 1).await;
    let module_ref = frame_artifacts(&before)
        .into_iter()
        .next()
        .expect("the definition's module")
        .to_owned();
    let old_frame = before
        .iter()
        .find(|edge| edge.kind == "frame_environment")
        .expect("frame edge")
        .id
        .clone();

    *seed.lock_recover() = Some(serde_json::json!({ "carried": definition }));
    let seeded = session
        .send(TurnInput::text("run the seeded definition"))
        .output()
        .await
        .expect("the pressure frame's turn");
    assert!(seeded.is_success(), "seeded turn: {seeded:?}");
    assert_eq!(last_cell_finish(&seeded), Some(serde_json::json!(13)));
    let after = wait_edges(&fixture, |edges| {
        let frame: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == "frame_environment")
            .collect();
        frame.len() == 1 && frame[0].id != old_frame && frame[0].artifact_ref == module_ref
    })
    .await;
    assert!(
        !after.iter().any(|edge| edge.id == old_frame),
        "the ended frame's cleanup settled"
    );
    drop(session);
    drop(core);

    let reopened = rlm_core_with_plugins(double, responses, vec![hook]);
    serve_processes(double, &reopened);
    let session = reopened.session(session_id).open().await.expect("reopen");
    let result = session
        .send(TurnInput::text("run it after a reopen"))
        .output()
        .await
        .expect("the reopened turn");
    assert!(result.is_success(), "reopened turn: {result:?}");
    assert_eq!(last_cell_finish(&result), Some(serde_json::json!(13)));
}

#[tokio::test]
async fn first_turn_continue_as_fences_its_initial_frame() {
    let fixture = Fixture::new(0x4031_0004).await;
    let double = &fixture.double;
    let core = rlm_core(
        double,
        vec![
            response(
                "const old = async () => 5; await control.continue_as({ task: 'next frame' });",
            ),
            response("finish('next frame');"),
        ],
    );
    let session = created_session(&core, "artifact-referrers-first-switch")
        .await
        .open()
        .await
        .expect("session");
    let switched = session
        .send(TurnInput::text("switch on first turn"))
        .output()
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "first-turn switch: {switched:?}");
    // The session is resident from its current frame (ADR 0112), so the
    // initial frame is read back through the history pages.
    let history = core
        .session("artifact-referrers-first-switch")
        .durable()
        .await
        .expect("durable session")
        .history(
            lash::persistence::HistoryAnchor::Head,
            lash::persistence::HistoryBudget {
                max_nodes: std::num::NonZeroU32::new(256).expect("nonzero node budget"),
                max_bytes: std::num::NonZeroU64::new(1 << 24).expect("nonzero byte budget"),
            },
        )
        .await
        .expect("read the session history");
    assert!(history.next.is_none(), "one page holds the whole ancestry");
    let mut frames = Vec::new();
    for node in history.nodes.iter().rev() {
        if !frames.contains(&node.frame_node_id) {
            frames.push(node.frame_node_id.clone());
        }
    }
    assert_eq!(frames.len(), 2, "the switch opens a second frame");
    let first_frame = frames[0].as_str();
    let after = wait_edges(&fixture, |edges| {
        !edges
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == first_frame)
    })
    .await;
    assert!(
        !after
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == first_frame),
        "the first frame has no surviving edge"
    );
}

#[tokio::test]
async fn carried_definition_id_retains_the_closure_across_frame_switch() {
    let fixture = Fixture::new(0x4177_0012).await;
    let core = rlm_core(
        &fixture.double,
        vec![
            response(
                "const made = await processes.create({ source: 'const answer = async () => 31;', dialect: 'typescript' }); finish(made.id);",
            ),
            response(
                "await control.continue_as({ task: 'carry id', seed: { kept_id: made.id } });",
            ),
            response("finish(kept_id);"),
        ],
    );
    let session = created_session(&core, "definition-id-carry")
        .await
        .open()
        .await
        .expect("session");
    let first = session
        .send(TurnInput::text("create"))
        .output()
        .await
        .expect("create turn");
    assert!(first.is_success(), "{first:?}");
    let id_json = last_cell_finish(&first).expect("tagged id output");
    let id = lash_core::ProcessDefinitionId::from_tagged_json(&id_json).expect("id");
    let before = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let old_frame = before[0].id.clone();
    let module_ref = before[0].artifact_ref.clone();
    let roots = fixture.double.lash_backend().session_store_factory();
    let lash_core::ArtifactReferrer::FrameEnvironment(committed_frame) =
        lash_core::ArtifactReferrer::decode("frame_environment", &old_frame)
            .expect("committed frame referrer")
    else {
        panic!("frame edge must name a frame")
    };
    assert!(
        roots
            .artifact_frame_is_retained(&committed_frame)
            .await
            .expect("read committed frame roots")
    );
    let absent_frame = lash_core::FrameEnvironmentId::new(
        session.session_id(),
        lash_core::FrameNodeId::new("never-committed-frame").expect("absent frame id"),
    );
    assert!(
        !roots
            .artifact_frame_is_retained(&absent_frame)
            .await
            .expect("read absent frame roots")
    );
    let switched = session
        .send(TurnInput::text("switch"))
        .output()
        .await
        .expect("switch");
    assert!(switched.is_success(), "{switched:?}");
    assert_eq!(last_cell_finish(&switched), Some(id_json));
    wait_edges(&fixture, |edges| {
        edges.len() == 1
            && edges[0].kind == "frame_environment"
            && edges[0].id != old_frame
            && edges[0].artifact_ref == module_ref
    })
    .await;
    assert!(
        core.host_artifacts()
            .get_definition(&id)
            .await
            .expect("definition snapshot")
            .is_some()
    );
    assert!(
        fixture
            .double
            .lash_backend()
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("module snapshot")
            .is_some()
    );
}

#[tokio::test]
async fn uncarried_frame_switch_loses_an_uncarried_definition() {
    let fixture = Fixture::new(0x4177_0013).await;
    let core = rlm_core(
        &fixture.double,
        vec![
            response(
                "const made = await processes.create({ source: 'const answer = async () => 31;', dialect: 'typescript' }); finish(made.id);",
            ),
            response("await control.continue_as({ task: 'no carry' });"),
            response("finish('switched');"),
        ],
    );
    let session = created_session(&core, "definition-id-no-carry")
        .await
        .open()
        .await
        .expect("session");
    let first = session
        .send(TurnInput::text("create"))
        .output()
        .await
        .expect("create");
    assert!(first.is_success(), "{first:?}");
    let id = lash_core::ProcessDefinitionId::from_tagged_json(
        &last_cell_finish(&first).expect("id output"),
    )
    .expect("id");
    let before = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = before[0].artifact_ref.clone();
    let switched = session
        .send(TurnInput::text("switch"))
        .output()
        .await
        .expect("switch");
    assert!(switched.is_success(), "{switched:?}");
    wait_edges(&fixture, |edges| edges.is_empty()).await;
    wait_definition_reclaimed(&fixture, &id, &module_ref).await;
    assert!(
        core.host_artifacts()
            .get_definition(&id)
            .await
            .expect("definition snapshot")
            .is_none()
    );
    assert!(
        fixture
            .double
            .lash_backend()
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("module snapshot")
            .is_none()
    );
}

#[tokio::test]
async fn host_pin_keeps_a_definition_across_an_uncarried_switch() {
    let fixture = Fixture::new(0x4177_0014).await;
    let core = rlm_core(
        &fixture.double,
        vec![
            response(
                "const made = await processes.create({ source: 'const answer = async () => 31;', dialect: 'typescript' }); finish(made.id);",
            ),
            response("await control.continue_as({ task: 'host keeps id' });"),
            response("finish('switched');"),
        ],
    );
    let session = created_session(&core, "definition-id-host-pin")
        .await
        .open()
        .await
        .expect("session");
    let first = session
        .send(TurnInput::text("create"))
        .output()
        .await
        .expect("create");
    assert!(first.is_success(), "{first:?}");
    let id = lash_core::ProcessDefinitionId::from_tagged_json(
        &last_cell_finish(&first).expect("id output"),
    )
    .expect("id");
    let before = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = before[0].artifact_ref.clone();
    let artifacts = core.host_artifacts();
    let first_pin = lash::process::HostArtifactPin::mint();
    let last_pin = lash::process::HostArtifactPin::mint();
    artifacts
        .pin_definition(&first_pin, &id)
        .await
        .expect("first pin");
    artifacts
        .pin_definition(&last_pin, &id)
        .await
        .expect("last pin");
    let switched = session
        .send(TurnInput::text("switch"))
        .output()
        .await
        .expect("switch");
    assert!(switched.is_success(), "{switched:?}");
    wait_edges(&fixture, |edges| {
        edges.len() == 2
            && edges
                .iter()
                .all(|edge| edge.kind == "host_pin" && edge.artifact_ref == module_ref)
    })
    .await;
    assert!(
        artifacts
            .get_definition(&id)
            .await
            .expect("pinned snapshot")
            .is_some()
    );
    artifacts.release(first_pin).await.expect("release first");
    wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "host_pin"
    })
    .await;
    assert!(
        artifacts
            .get_definition(&id)
            .await
            .expect("last pin snapshot")
            .is_some()
    );
    artifacts.release(last_pin).await.expect("release last");
    wait_edges(&fixture, |edges| edges.is_empty()).await;
    wait_definition_reclaimed(&fixture, &id, &module_ref).await;
    assert!(
        artifacts
            .get_definition(&id)
            .await
            .expect("reclaimed snapshot")
            .is_none()
    );
    assert!(
        fixture
            .double
            .lash_backend()
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("reclaimed module")
            .is_none()
    );
}

/// The source every `processes.create` case compiles: one process, whose
/// answer a later turn reads back to prove it ran the created module.
const CREATED_SOURCE: &str = "const answer = async () => 40 + 2;";

fn create_definition_cell(binding: &str) -> String {
    format!(
        "const {binding} = await processes.create({{ source: {CREATED_SOURCE:?}, dialect: 'typescript' }}); finish('created');"
    )
}

/// FIG-3116: a definition `processes.create` returns is an RLM value like
/// any other (ADR 0113 §6). The call's realization publishes its module under
/// the realizing execution, the cell's global holds it in the frame, and a
/// later turn in the same frame starts it by value after a cold reopen.
#[tokio::test]
async fn created_definition_survives_cold_reopen_and_starts_by_value() {
    let fixture = Fixture::new(0x3116_0001).await;
    let double = &fixture.double;
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response(&create_definition_cell("made")),
        response("const run = await processes.start({ definition: made }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(double, Arc::clone(&responses));
    let first_session = created_session(&first_core, "processes-create-cold-reopen")
        .await
        .open()
        .await
        .expect("open first session");
    let first = first_session
        .send(TurnInput::text("create definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first.is_success(), "processes.create turn: {first:?}");
    let created = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = created[0].artifact_ref.clone();
    let frame_id = created[0].id.clone();
    drop(first_session);
    drop(first_core);

    let second_core = rlm_core_with_queue(double, Arc::clone(&responses));
    serve_processes(double, &second_core);
    let second_session = created_session(&second_core, "processes-create-cold-reopen")
        .await
        .open()
        .await
        .expect("cold reopen");
    let second = second_session
        .send(TurnInput::text("start created definition"))
        .output()
        .await
        .expect("second turn");
    assert!(
        second.is_success(),
        "a created definition survives cold reopen: {second:?}"
    );
    assert!(responses.lock_recover().is_empty());
    assert_eq!(last_cell_finish(&second), Some(serde_json::json!(42)));
    let after_start = wait_edges(&fixture, |edges| {
        edges.iter().any(|edge| {
            edge.kind == "frame_environment"
                && edge.id == frame_id
                && edge.artifact_ref == module_ref
        })
    })
    .await;
    assert_eq!(
        frame_artifacts(&after_start),
        BTreeSet::from([module_ref.as_str()]),
        "the frame holds exactly the created module"
    );
}

/// FIG-3116: deleting the session that created a definition ends the frame
/// that held it, and the relay reclaims the module (ADR 0113 §3.1): nothing
/// else keeps a created definition alive.
#[tokio::test]
async fn created_definition_is_reclaimed_after_session_deletion() {
    let fixture = Fixture::new(0x3116_0002).await;
    let double = &fixture.double;
    let core = rlm_core(double, vec![response(&create_definition_cell("made"))]);
    let session_id = "processes-create-deletion";
    let session = created_session(&core, session_id)
        .await
        .open()
        .await
        .expect("session");
    let created = session
        .send(TurnInput::text("create definition"))
        .output()
        .await
        .expect("create turn");
    assert!(created.is_success(), "processes.create turn: {created:?}");
    let held = wait_edges(&fixture, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = held[0].artifact_ref.clone();
    let modules = double.lash_backend().module_artifacts();
    assert!(
        modules
            .get_module_artifact(&module_ref)
            .await
            .expect("read created module")
            .is_some(),
        "the frame holds the created module"
    );
    drop(session);

    let handler = double
        .open_handler(lash_core::AdmittedScope::session_delete(
            lash_core::SessionId::from(session_id),
        ))
        .await
        .expect("open the delete handler");
    let deletion = {
        struct HandlerExecution<'a> {
            administration: lash_core::SessionAdministration,
            scoped: lash_core::ScopedEffectController<'a>,
        }
        impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
            fn administration(&self) -> &lash_core::SessionAdministration {
                &self.administration
            }
            fn scoped<'run>(
                &'run self,
                _: lash_core::AdmittedScope,
            ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError>
            {
                Ok(self.scoped.clone())
            }
        }
        let execution = HandlerExecution {
            administration: core.session_administration().await,
            scoped: handler.scoped(),
        };
        let context = lash_core::SessionDeleteContext::from_execution(&execution, session_id)
            .expect("delete context");
        let services = execution.administration.session_close();
        // A reconcile pass can outlive its tick and own the close when the
        // deleting caller arrives. Hold that claim until the law observes it.
        let intent = execution
            .administration
            .store_factory()
            .begin_session_close(context.session_id(), services.clock.timestamp_ms())
            .await
            .expect("record the close before its competing delivery")
            .expect("the recorded session has a close intent");
        let claimed = services
            .intents
            .claim(
                intent.obligation.as_ref().expect("close obligation"),
                &lash_core::store::ClaimToken::mint(),
                services.clock.timestamp_ms(),
                services.policy.claim_ttl_ms,
            )
            .await
            .expect("claim the close for its competing delivery")
            .expect("the close is due");
        assert_eq!(intent.state, lash_core::store::ControlIntentState::Pending);
        let closed = lash_core::session_close::close_session(&context)
            .await
            .expect("begin the recorded close")
            .expect("the session is recorded");
        assert_eq!(
            closed.applied,
            lash_core::store::ControlIntentState::Pending
        );

        let pending = tokio::sync::Notify::new();
        let acknowledged = async {
            loop {
                let stored = execution
                    .administration
                    .store_factory()
                    .load_intent(closed.intent.id)
                    .await
                    .expect("read the close intent")
                    .expect("the close intent is retained");
                match stored.state {
                    lash_core::store::ControlIntentState::Acknowledged { .. } => return,
                    lash_core::store::ControlIntentState::Pending => {
                        pending.notify_one();
                        tokio::task::yield_now().await;
                    }
                    state => panic!("the close did not acknowledge: {state:?}"),
                }
            }
        };
        tokio::pin!(acknowledged);
        tokio::select! {
            () = pending.notified() => {}
            () = &mut acknowledged => panic!("the held close was acknowledged prematurely"),
        }
        assert!(
            modules
                .get_module_artifact(&module_ref)
                .await
                .expect("read module while the close is pending")
                .is_some(),
            "the pending close still holds the created module"
        );
        let relay = lash_core::drive::ControlIntentRelay::new(
            Arc::clone(&services.intents),
            Arc::clone(execution.administration.store_factory()),
            Arc::clone(&services.work),
            Arc::clone(&services.scopes),
            Arc::clone(&services.scope_close_obligations),
            Arc::clone(&services.clock),
        )
        .with_policy(services.policy);
        lash_core::drive::relay::deliver_claimed(&relay, claimed, services.clock.as_ref())
            .await
            .expect("deliver the competing close claim");
        acknowledged.await;
        LashCore::delete_session(context).await
    };
    handler.close().await.expect("close the delete handler");
    assert!(
        matches!(deletion, Ok(lash::SessionDeletion::Deleted(_))),
        "the delete runs in the call: {deletion:?}"
    );

    let after = wait_edges(&fixture, |edges| edges.is_empty()).await;
    assert!(after.is_empty(), "no edge survives the session: {after:?}");
    while modules
        .get_module_artifact(&module_ref)
        .await
        .expect("read module after deletion")
        .is_some()
    {
        fixture.reconcile().await;
    }
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "artifact-referrers",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
