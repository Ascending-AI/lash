//! A zombie segment execution after a SubstrateLost recovery (FIG-3818).
//!
//! Restate's kill does not wait for the deployment: a deployment can still be
//! executing an attempt of an invocation that was killed and purged, while a
//! fresh invocation of the same segment arrives, finds the start the old
//! execution recorded and a journal it cannot read, and ends the process
//! `Abandoned(ResumeRefused { SubstrateLost })`.
//!
//! The zombie is the execution that admitted the segment: its nonce is the one
//! the start recorded, so no admission fence refuses it. What keeps the
//! process consistent is the terminal write: first writer wins. Whichever of
//! the recovery and the zombie stores its terminal first, that terminal
//! stands; the other is answered `Superseded` with the stored terminal, and
//! awaiters read the stored one. Both orders are forced here, with the
//! non-blocking `server.kill`.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::Arc;
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::{
    ProcessAwaitOutput, ProcessEventLogTestSupport as _, ProcessId, ProcessWorkSubstrate as _,
    StoreSet as _,
};
use lash_restate_test::{RestateTestBackend, ServerConfig};
use lashlang::testing::ast_builders as b;
use serde_json::json;

const PROCESS: &str = "main";
const SIGNAL: &str = "go";

fn model_spec() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// An RLM core over `restate`. The process runs no model; the provider only
/// has to exist.
fn build_core(restate: &RestateTestBackend) -> lash::LashCore {
    let backend = restate.lash_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("substrate-lost-zombie")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse::default())
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(model_spec())
        .without_queued_work()
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "substrate-lost-zombie",
        ))
        .expect("build the lash core")
}

/// `process main() signals { go: any } { value = wait_signal("go") finish value }`
async fn publish_process(
    restate: &RestateTestBackend,
    process_id: &str,
) -> lash_core::ProcessStartRequest {
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            vec![b::signal(SIGNAL, lashlang::TypeExpr::Any)],
            b::block(vec![
                b::assign("value", b::wait_signal(SIGNAL)),
                b::finish(b::var("value")),
            ]),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(
        program,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::default(),
        ),
    )
    .expect("link the process");
    lashlang::LashlangArtifacts::new(restate.lash_backend().module_artifacts())
        .publish_module_artifact(
            &lash_core::ArtifactOwner::host("substrate-lost-zombie"),
            &linked.artifact,
        )
        .await
        .expect("publish the process artifact");
    let signal_event_types = linked
        .artifact
        .ir()
        .process(PROCESS)
        .map(lash_lashlang_runtime::lashlang_process_signal_event_types)
        .unwrap_or_default();
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref(PROCESS)
            .expect("the process ref")
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: PROCESS.to_owned(),
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .expect("the process input serializes");
    lash_core::ProcessStartRequest::new(
        ProcessId::from(process_id),
        input,
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessOriginator::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: model_spec(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    ))
    .with_extra_event_types(
        lash_lashlang_runtime::lashlang_process_event_types()
            .into_iter()
            .chain(signal_event_types),
    )
}

/// The terminal the zombie proposes: the output its own run would store.
fn zombie_terminal() -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(json!({
        "zombie": true
    })))
}

/// The terminal write the zombie's completion step makes: the process's
/// workflow key is its whole authority.
async fn zombie_writes_its_terminal(
    restate: &RestateTestBackend,
    process_id: &ProcessId,
) -> lash_core::ProcessCompletionOutcome {
    restate
        .lash_backend()
        .process_registry()
        .complete_process(
            process_id,
            zombie_terminal(),
            lash_core::ProcessCompletionAuthority::WorkflowKey {
                workflow_key: process_id.to_string(),
            },
        )
        .await
        .expect("the zombie's terminal write is answered")
}

fn is_substrate_lost(output: &ProcessAwaitOutput) -> bool {
    matches!(
        output,
        ProcessAwaitOutput::Abandoned { evidence, .. }
            if matches!(
                evidence.writer,
                lash_core::AbandonWriter::ResumeRefused {
                    reason: lash_core::ProcessResumeRefusal::SubstrateLost
                }
            )
    )
}

/// Which terminal write lands first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum First {
    Recovery,
    Zombie,
}

/// Start the process and let its root segment park on the signal; kill its
/// invocation without waiting for the deployment and purge it, as retention
/// does; then run the two terminal writes in the order `first` says.
async fn zombie_after_substrate_lost(first: First, seed: u64) {
    let restate = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let core = build_core(&restate);
    restate.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );
    let process_id = format!("zombie-{first:?}").to_lowercase();
    let request = publish_process(&restate, &process_id).await;
    let process_id = request.id.clone();
    {
        let core = core.clone();
        let request = request.clone();
        let admitted = lash_core::AdmittedScope::runtime_operation("start".to_owned());
        tokio::time::timeout(
            Duration::from_secs(20),
            restate.run_in_handler(
                admitted,
                Arc::new(move |scoped| {
                    let core = core.clone();
                    let request = request.clone();
                    Box::pin(async move {
                        core.processes()
                            .start(request, scoped)
                            .await
                            .expect("start the process");
                    })
                }),
            ),
        )
        .await
        .expect("the start finishes")
        .expect("the start's handler completes");
    }
    let parked = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Some(process)) = core.processes().get(&process_id).await
                && process.wait.is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(parked.is_ok(), "the root segment parks on the signal");
    let server = restate.server();
    let root = server
        .invocations()
        .into_iter()
        .find(|view| view.target == format!("LashProcessWorkflow/{process_id}/run"))
        .expect("the root segment's invocation");
    let input = root_input(&restate, &root.id);
    // Restate's kill does not wait for the deployment's attempt to stop.
    assert_eq!(server.kill(&root.id), Some(true), "the kill lands");
    assert_eq!(server.purge(&root.id), Some(true), "retention purges it");

    let registry = restate.lash_backend().process_registry();
    let sweep = lash_restate::RestateProcessIngressRunner::new(
        restate.connection(),
        Arc::clone(&registry),
        restate.stores().process_continuations(),
    );
    let zombie = match first {
        First::Zombie => Some(zombie_writes_its_terminal(&restate, &process_id).await),
        First::Recovery => None,
    };
    // The recovery: a fresh invocation of the segment finds the start the
    // killed execution recorded and ends the process SubstrateLost, unless a
    // terminal is already stored, which it publishes instead.
    let ingress = restate.ingress();
    let fresh = ingress.call_workflow_json::<_, serde_json::Value>(
        "LashProcessWorkflow",
        process_id.as_str(),
        "run",
        &input,
    );
    let recovered = tokio::time::timeout(Duration::from_secs(20), fresh)
        .await
        .expect("the fresh invocation ends")
        .expect("the fresh invocation's output");
    let _ = sweep.admit_pending_processes("fig-3818").await;
    let zombie = match zombie {
        Some(zombie) => zombie,
        None => zombie_writes_its_terminal(&restate, &process_id).await,
    };

    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process exists");
    let stored = record
        .outcome
        .clone()
        .expect("exactly one terminal is stored");
    match first {
        First::Recovery => {
            assert!(
                recovered.to_string().contains("substrate_lost"),
                "the fresh invocation ended the process SubstrateLost: {recovered}"
            );
            assert!(
                is_substrate_lost(&stored),
                "the recovery's terminal stands: {stored:?}"
            );
            assert!(
                matches!(
                    &zombie,
                    lash_core::ProcessCompletionOutcome::Superseded { stored }
                        if stored.outcome.as_ref().is_some_and(is_substrate_lost)
                ),
                "the zombie's write is superseded by the recovery's: {zombie:?}"
            );
        }
        First::Zombie => {
            assert_eq!(stored, zombie_terminal(), "the zombie's terminal stands");
            // The recovery's SubstrateLost proposal was superseded: its
            // completion step answered with the stored terminal, which the
            // fresh invocation published.
            assert!(
                recovered.to_string().contains("\"zombie\":true"),
                "the recovery publishes the stored terminal: {recovered}"
            );
            assert!(
                matches!(zombie, lash_core::ProcessCompletionOutcome::Committed(_)),
                "the zombie's write committed: {zombie:?}"
            );
        }
    }
    let events = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read the process events");
    let terminals = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "process.completed" | "process.failed" | "process.cancelled" | "process.abandoned"
            )
        })
        .count();
    assert_eq!(terminals, 1, "no double settle: {events:#?}");
    let awaited = tokio::time::timeout(
        Duration::from_secs(20),
        core.processes().await_output(&process_id),
    )
    .await
    .expect("await_output returns")
    .expect("await the process");
    assert_eq!(awaited, stored, "awaiters read the stored terminal");
}

/// The input the killed invocation was submitted with: its journal's input
/// command carries it, the JSON after the protobuf envelope.
fn root_input(restate: &RestateTestBackend, invocation: &str) -> serde_json::Value {
    let journal = restate
        .server()
        .journal(invocation)
        .expect("the root segment's journal");
    let input = journal.first().expect("the input command");
    let bytes = &input.payload;
    let start = bytes
        .iter()
        .position(|byte| *byte == b'{')
        .expect("the input carries JSON");
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'}')
        .expect("the input carries JSON");
    serde_json::from_slice(&bytes[start..=end]).expect("decode the segment input")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_substrate_lost_the_recovery_terminal_stands_over_a_later_zombie_write() {
    zombie_after_substrate_lost(First::Recovery, 0x3818).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_substrate_lost_a_zombie_terminal_written_first_stands() {
    zombie_after_substrate_lost(First::Zombie, 0x3818).await;
}
