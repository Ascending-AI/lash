//! RLM frame evidence for ADR 0113: which frames, executions and host pins
//! hold a definition's module. Every law runs on the core's own node over
//! SQLite or PostgreSQL stores; the core's cleanup relay reclaims what an
//! ended frame held.

#![cfg(all(feature = "rlm", feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "backend-parametrized acceptance laws assert each step's result"
)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::{LashCore, TurnInput, TurnOutput};
use lash_sansio::sync::MutexExt;

#[path = "artifact_referrers_evidence/fixture.rs"]
mod fixture;
use fixture::{Backend, Fixture};

async fn stored_module_refusals_preserve_causes_and_terminal_semantics(backend: Backend) {
    use lash_lashlang_runtime::LashlangProcessInput;
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
    for (bytes, generation) in [
        (b"invalid JSON".to_vec(), false),
        (serde_json::to_vec(&generation).unwrap(), true),
        (serde_json::to_vec(&corrupt_hash).unwrap(), false),
        (serde_json::to_vec(&unlifted).unwrap(), false),
    ]
    .into_iter()
    {
        let fixture = Fixture::new(backend).await;
        let backend = fixture.backend.clone();
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
        // The process runs on the core's node: its engine loads the stored
        // module in its first VM step and owns the refusal.
        let core = rlm_core(&fixture, Vec::new());
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &process_environment())
            .await
            .expect("publish the process environment");
        let started = core
            .processes()
            .start(
                lash_core::ProcessStartRequest::new(
                    lash_core::ProcessInput::Engine {
                        kind: lash_lashlang_runtime::LASHLANG_ENGINE_KIND.to_owned(),
                        payload: serde_json::to_value(&input).unwrap(),
                    },
                    lash_core::ProcessOriginator::host(),
                    lash_core::LifetimeDecision::Detached,
                )
                .with_env_ref(env_ref),
                core.effect_host(),
            )
            .await
            .expect("the process starts")
            .process_id;
        let decoded = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            core.processes().await_output(&started),
        )
        .await
        .expect("the process ends within a minute")
        .expect("read the process's end");
        let encoded = serde_json::to_vec(&decoded).unwrap();
        assert_eq!(
            serde_json::from_slice::<lash_core::ProcessAwaitOutput>(&encoded).unwrap(),
            decoded,
            "durable terminal retains all evidence"
        );
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
        let registry = backend.process_registry();
        let retained = registry
            .get_process(&started)
            .await
            .expect("read durable terminal")
            .expect("retained process");
        assert_eq!(retained.outcome().as_ref(), Some(&decoded));
        let events = registry
            .recent_events(&started, 4)
            .await
            .expect("read durable terminal event");
        let terminals: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.fact {
                lash_core::ProcessLifecycleFact::Terminal { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 1, "the process writes one terminal event");
        assert_eq!(
            lash_core::ProcessAwaitOutput::from(terminals[0].clone()),
            decoded,
            "event retains the cause"
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    artifact_ref: String,
    kind: String,
    id: String,
}

/// Wait for the edge table to satisfy `predicate`. The bound is wall-clock
/// time, so a law that pauses the runtime's clock cannot spend it.
async fn wait_edges(fixture: &Fixture, predicate: impl Fn(&[Edge]) -> bool) -> Vec<Edge> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let edges = fixture.edges().await;
        if predicate(&edges) {
            return edges;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edges never reached the awaited state: {edges:#?}"
        );
        fixture.settle().await;
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
    let backend = fixture.backend.clone();
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
        fixture.settle().await;
    }
}

/// Move past the former deadline after a real store read. Manual polling
/// checks the pending wait without letting a paused clock auto-advance
/// PostgreSQL's connection timeouts.
async fn wait_edges_waits_for_condition_past_former_deadline(backend: Backend) {
    use futures_util::FutureExt as _;

    let fixture = Fixture::new(backend).await;
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
            // The RLM event rides its format-stamped envelope (FIG-5028).
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } if event.plugin_id == "rlm_protocol" => event
                .payload
                .get("event")?
                .get("RlmTrajectoryEntry")?
                .get("final_output")
                .cloned(),
            _ => None,
        })
        .next_back()
}

/// `text`'s run, and when it switched frames, the frame task's own run
/// that follows it (FIG-5232): the output the switch's work ends in.
async fn send_through(session: &lash::LashSession, text: &str) -> lash::Result<TurnOutput> {
    let output = session.send(TurnInput::text(text)).output().await?;
    let lash::TurnOutcome::AgentFrameSwitch { frame_key, .. } = &output.result.outcome else {
        return Ok(output);
    };
    let follow_on = lash_core::runtime::durable::session_mail::frame_task_run(frame_key);
    let continued = session.attach_id(follow_on).output().await?;
    assert!(
        continued.is_success(),
        "the frame's task runs: {continued:?}"
    );
    Ok(continued)
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

fn rlm_core(fixture: &Fixture, responses: Vec<LlmResponse>) -> LashCore {
    rlm_core_with_queue(fixture, Arc::new(Mutex::new(VecDeque::from(responses))))
}

fn rlm_core_with_queue(fixture: &Fixture, queue: Arc<Mutex<VecDeque<LlmResponse>>>) -> LashCore {
    rlm_core_with_plugins(fixture, queue, Vec::new())
}

#[expect(clippy::expect_used, reason = "test fixture validates its setup")]
fn rlm_core_with_plugins(
    fixture: &Fixture,
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
    let backend = fixture.backend.clone();
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
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("artifact-referrers")
                .context_window_tokens(16_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
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

async fn cold_reopen_globals_across_turns(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    // A reopened core may use either provider handle for the same route.
    // Both handles therefore draw from the two-turn script in call order.
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response("const saved = async () => 7; finish('bound');"),
        response("const run = await processes.start({ definition: saved }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(&fixture, Arc::clone(&responses));
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

    let second_core = rlm_core_with_queue(&fixture, Arc::clone(&responses));
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

async fn overwrite_retains_old_module_until_frame_end(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
        send_through(&session, "switch")
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

async fn continue_as_carries_only_seeded_definition(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
    let switched = send_through(&session, "carry one")
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
            seed: lash_protocol_rlm::rlm_seed_initial_nodes(
                seed,
                lash_core::FleetFormat::current(),
            ),
        })
    }
}

/// FIG-4134: a context-pressure hook's RLM seed carries its module-backed
/// values into the frame it opens, exactly as a `continue_as` seed does
/// (ADR 0113 §3.1). The hook seeds a definition the first frame bound; once
/// the old frame's cleanup settles, the new frame alone holds the module's
/// edge, and after a cold reopen the seeded definition still starts.
async fn a_pressure_seed_carries_its_module_into_the_new_frame(backend: Backend) {
    let fixture = Fixture::new(backend).await;
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
            lash::plugins::PluginDeclaration::initial("artifact-referrers-seeding-pressure"),
            lash_core::facade_support::PluginSpec::new().with_context_pressure_hook(
                100,
                Arc::new(SeedingPressureHook {
                    seed: Arc::clone(&seed),
                }),
            ),
        ));
    let core = rlm_core_with_plugins(&fixture, Arc::clone(&responses), vec![Arc::clone(&hook)]);
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

    let reopened = rlm_core_with_plugins(&fixture, responses, vec![hook]);
    let session = reopened
        .session(lash::SessionId::parse(session_id).expect("nonblank host identity"))
        .open()
        .await
        .expect("reopen");
    let result = session
        .send(TurnInput::text("run it after a reopen"))
        .output()
        .await
        .expect("the reopened turn");
    assert!(result.is_success(), "reopened turn: {result:?}");
    assert_eq!(last_cell_finish(&result), Some(serde_json::json!(13)));
}

async fn first_turn_continue_as_fences_its_initial_frame(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
    let switched = send_through(&session, "switch on first turn")
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "first-turn switch: {switched:?}");
    // The session is resident from its current frame (ADR 0112), so the
    // initial frame is read back through the history pages.
    let history = core
        .session(
            lash::SessionId::parse("artifact-referrers-first-switch")
                .expect("nonblank host identity"),
        )
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

async fn carried_definition_id_retains_the_closure_across_frame_switch(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
    let roots = fixture.backend.clone().session_store_factory();
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
    let switched = send_through(&session, "switch").await.expect("switch");
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
            .backend
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("module snapshot")
            .is_some()
    );
}

async fn uncarried_frame_switch_loses_an_uncarried_definition(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
    let switched = send_through(&session, "switch").await.expect("switch");
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
            .backend
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("module snapshot")
            .is_none()
    );
}

async fn host_pin_keeps_a_definition_across_an_uncarried_switch(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
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
    let switched = send_through(&session, "switch").await.expect("switch");
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
            .backend
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("reclaimed module")
            .is_none()
    );
}

/// The environment a host-started process runs under.
fn process_environment() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash::NoProgressBudget::bounded(12),
        ),
    )
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
async fn created_definition_survives_cold_reopen_and_starts_by_value(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response(&create_definition_cell("made")),
        response("const run = await processes.start({ definition: made }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(&fixture, Arc::clone(&responses));
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

    let second_core = rlm_core_with_queue(&fixture, Arc::clone(&responses));
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
async fn created_definition_is_reclaimed_after_session_deletion(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(&fixture, vec![response(&create_definition_cell("made"))]);
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
    let modules = fixture.backend.module_artifacts();
    assert!(
        modules
            .get_module_artifact(&module_ref)
            .await
            .expect("read created module")
            .is_some(),
        "the frame holds the created module"
    );
    drop(session);

    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(
        administration
            .delete_context(session_id)
            .expect("delete context"),
    )
    .await;
    assert!(
        matches!(deletion, Ok(lash::SessionDeletion::Requested { .. })),
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
        fixture.settle().await;
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
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                "artifact-referrers",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}

macro_rules! tiered {
    ($($law:ident),* $(,)?) => {$(
        mod $law {
            #[tokio::test]
            async fn sqlite() {
                super::$law(super::Backend::Sqlite).await;
            }

            #[tokio::test]
            #[ignore = "requires PostgreSQL; select inside a pg16 gate"]
            async fn postgres() {
                super::$law(super::Backend::Postgres).await;
            }
        }
    )*};
}

/// [`tiered!`] for laws a known bug keeps red: both legs are ignored, the
/// SQLite leg naming the bug's ticket.
macro_rules! tiered_ignored {
    ($reason:literal: $($law:ident),* $(,)?) => {$(
        mod $law {
            #[tokio::test]
            #[ignore = $reason]
            async fn sqlite() {
                super::$law(super::Backend::Sqlite).await;
            }

            #[tokio::test]
            #[ignore = "requires PostgreSQL; select inside a pg16 gate"]
            async fn postgres() {
                super::$law(super::Backend::Postgres).await;
            }
        }
    )*};
}

tiered!(
    stored_module_refusals_preserve_causes_and_terminal_semantics,
    wait_edges_waits_for_condition_past_former_deadline,
    cold_reopen_globals_across_turns,
    overwrite_retains_old_module_until_frame_end,
    first_turn_continue_as_fences_its_initial_frame,
    uncarried_frame_switch_loses_an_uncarried_definition,
    host_pin_keeps_a_definition_across_an_uncarried_switch,
    created_definition_survives_cold_reopen_and_starts_by_value,
    created_definition_is_reclaimed_after_session_deletion
);

tiered_ignored!(
    "FIG-5354: a durable turn's frame switch carries none of its seed's modules":
    continue_as_carries_only_seeded_definition,
    carried_definition_id_retains_the_closure_across_frame_switch,
);

tiered_ignored!(
    "FIG-5327: context-pressure compaction opens no frame on a durable turn":
    a_pressure_seed_carries_its_module_into_the_new_frame,
);

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    let executable = std::env::current_exe().expect("test executable");
    let law = "stored_module_refusals_preserve_causes_and_terminal_semantics::postgres";
    for url in [None, Some(""), Some(" \t ")] {
        let mut command = std::process::Command::new(&executable);
        command
            .args(["--exact", law, "--include-ignored", "--nocapture"])
            .env_remove("LASH_POSTGRES_DATABASE_URL");
        if let Some(url) = url {
            command.env("LASH_POSTGRES_DATABASE_URL", url);
        }
        let output = command.output().expect("run PostgreSQL variant");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("running 1 test"), "{stdout}\n{stderr}");
        assert!(
            !output.status.success() && stdout.contains("0 passed; 1 failed"),
            "{law} with URL {url:?} passed vacuously: {stdout}\n{stderr}"
        );
        assert!(
            stderr.contains("LASH_POSTGRES_DATABASE_URL"),
            "{stdout}\n{stderr}"
        );
    }
}
