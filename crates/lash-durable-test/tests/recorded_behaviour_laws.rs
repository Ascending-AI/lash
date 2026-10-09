//! FIG-4398 laws on a served node: a session runs under the protocol
//! behaviour it recorded at creation, never under the configuration of the
//! deployment that runs it (ADR 0105 §1). Ported by FIG-5310 from the
//! deleted redrive laws of lash-protocol-standard's and lash-protocol-rlm's
//! `recorded_behaviour_tests.rs`.
//!
//! The creating deployment's core records the session, drains its node and
//! writes the turn's input as session mail; a redeploying deployment's core,
//! whose protocol factory states other behaviour, claims the session and
//! runs the turn. The model reports which behaviour its prompt offered.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/kernel_process.rs"]
mod kernel_process;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};

use lash_core::llm::types::LlmRequest;
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG};

/// The `batch` maximum the creating standard deployment offers.
const RECORDED_MAX_MEMBERS: usize = 3;

/// What the standard model answers when its prompt offers `batch` at the
/// recorded maximum.
const RECORDED_ANSWER: &str = "batch offered at the recorded maximum";

/// What a model answers when its prompt carries the redeploying factory's
/// behaviour: a session that lost its recorded behaviour ends here.
const LOST_ANSWER: &str = "the prompt carried the redeploying factory's behaviour";

/// The loop the RLM model's cell runs: far past the redeploying bound and
/// far inside the recorded one.
const LOOP_ITERATIONS: usize = 5_000;

/// Every request the model served, rendered.
#[derive(Default)]
struct Requests(Mutex<Vec<String>>);

impl Requests {
    fn all(&self) -> Vec<String> {
        self.0.lock_recover().clone()
    }
}

/// The model: `answer` reads the rendered request and says what to reply.
fn model(
    requests: &Arc<Requests>,
    answer: fn(&str) -> String,
) -> lash_core::facade_support::ProviderHandle {
    let requests = Arc::clone(requests);
    lash_core::testing::TestProvider::builder()
        .kind("recorded-behaviour-model")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let requests = Arc::clone(&requests);
            async move {
                let rendered = serde_json::to_string(&request).expect("a request encodes");
                let reply = answer(&rendered);
                requests.0.lock_recover().push(rendered);
                Ok(served::text(&request, &reply))
            }
        })
        .build()
        .into_handle()
}

fn standard_answer(rendered: &str) -> String {
    if rendered.contains(&format!("at most {RECORDED_MAX_MEMBERS} per batch")) {
        RECORDED_ANSWER.to_owned()
    } else {
        LOST_ANSWER.to_owned()
    }
}

/// The looping cell when the prompt offers `continue_as` (the recorded
/// decomposition feature), prose otherwise.
fn rlm_answer(rendered: &str) -> String {
    if rendered.contains("continue_as") {
        format!(
            "<typescript>\nlet i = 0;\nwhile (i < {LOOP_ITERATIONS}) {{ i = i + 1; }}\nfinish(\"ran \" + String(i));\n</typescript>"
        )
    } else {
        LOST_ANSWER.to_owned()
    }
}

/// The standard deployment that creates the session: `batch` offered, at
/// most [`RECORDED_MAX_MEMBERS`] per call.
fn standard_creating() -> lash::plugins::StandardProtocolConfig {
    lash::plugins::StandardProtocolConfig::default().batch(lash::plugins::BatchSugar::Enabled {
        max_members: std::num::NonZeroUsize::new(RECORDED_MAX_MEMBERS).expect("nonzero maximum"),
    })
}

/// The standard deployment that runs it: `batch` withheld.
fn standard_redeploying() -> lash::plugins::StandardProtocolConfig {
    lash::plugins::StandardProtocolConfig::default().batch(lash::plugins::BatchSugar::Disabled)
}

/// The RLM deployment that creates the session: a generous instruction
/// bound and decomposition offered.
fn rlm_creating() -> lash::rlm::RlmProtocolPluginConfig {
    lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build()
}

/// The RLM deployment that runs it: an instruction bound the loop exhausts,
/// decomposition withheld, label annotations off, a smaller output limit and
/// no soft warning.
fn rlm_redeploying() -> lash::rlm::RlmProtocolPluginConfig {
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(50))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(1))
        .build();
    config.prompt_features.decomposition = false;
    config.max_output_chars = 100;
    config.continue_as_soft_warn_tokens = None;
    config
}

/// Which protocol a deployment runs, under which of its configurations.
#[derive(Clone, Copy)]
enum Deployment {
    StandardCreating,
    StandardRedeploying,
    RlmCreating,
    RlmRedeploying,
}

impl Deployment {
    fn build(self) -> &'static str {
        match self {
            Self::StandardCreating | Self::RlmCreating => "creating-build",
            Self::StandardRedeploying | Self::RlmRedeploying => "redeploying-build",
        }
    }

    /// A core of this deployment over `backend`, served by the law's model.
    fn core(self, backend: &lash::Backend, requests: &Arc<Requests>) -> lash::LashCore {
        let (builder, answer): (lash::LashCoreBuilder, fn(&str) -> String) = match self {
            Self::StandardCreating | Self::StandardRedeploying => {
                let config = match self {
                    Self::StandardCreating => standard_creating(),
                    _ => standard_redeploying(),
                };
                (
                    lash::LashCore::builder(backend.clone()).protocol_plugin(Arc::new(
                        lash::plugins::StandardProtocolPluginFactory::with_config(config),
                    )),
                    standard_answer,
                )
            }
            Self::RlmCreating | Self::RlmRedeploying => {
                let config = match self {
                    Self::RlmCreating => rlm_creating(),
                    _ => rlm_redeploying(),
                };
                let factory = lash::rlm::RlmProtocolPluginFactory::new(
                    config,
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(sim::untimed_workers());
                (
                    lash::LashCore::rlm_builder(backend.clone(), factory),
                    rlm_answer,
                )
            }
        };
        builder
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .serve_test_llm_profile(model(requests, answer), served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("recorded-behaviour-deployment"),
                lash::persistence::LeaseIncarnationId::new(self.build()),
            ))
            .expect("the core builds")
    }
}

/// Record `session` on `creating`'s core, drain it, write the turn's input
/// through it, and let `redeploying`'s core run the turn: the settled
/// output, and every request the model served.
async fn run_on_the_redeploying_core(
    tier: Tier,
    creating: Deployment,
    redeploying: Deployment,
    session: &str,
) -> Option<(lash::TurnOutput, Vec<String>)> {
    let (stores, keep) = served::stores(tier).await?;
    let backend = served::backend(stores);
    let requests = Arc::new(Requests::default());
    let session_id = lash::SessionId::try_from(session.to_owned()).expect("a session id");

    let first = creating.core(&backend, &requests);
    let recorded = first
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            served::spec(64),
        ))
        .await
        .expect("the creating deployment records the session");
    first
        .drain()
        .await
        .expect("the creating deployment's node drains");
    let sent = Box::pin(
        recorded
            .send(lash::TurnInput::text("loop it"))
            .into_future(),
    )
    .await
    .expect("the drained core still writes the input's mail");

    let second = redeploying.core(&backend, &requests);
    let _ = second.session(session_id);
    let output = tokio::time::timeout(WATCHDOG, sent.output())
        .await
        .expect("deadlock watchdog: the redeploying node never ran the turn")
        .expect("the turn answers");
    second.shutdown().await.expect("the core shuts down");
    first.shutdown().await.expect("the core shuts down");
    drop(keep);
    Some((output, requests.all()))
}

/// A standard turn run by a deployment that withholds `batch` is offered
/// `batch` at the maximum its session recorded.
async fn a_standard_run_executes_under_its_recorded_behaviour(tier: Tier) {
    let Some((output, requests)) = run_on_the_redeploying_core(
        tier,
        Deployment::StandardCreating,
        Deployment::StandardRedeploying,
        "standard-recorded",
    )
    .await
    else {
        return;
    };
    assert_eq!(requests.len(), 1, "one model call answers the turn");
    assert!(
        requests[0].contains("\"batch\""),
        "the recorded batch tool is offered: {}",
        requests[0]
    );
    assert_eq!(
        output.assistant_message(),
        Some(RECORDED_ANSWER),
        "the run's prompt offered batch at the recorded maximum"
    );
}

/// An RLM turn run by a deployment with other bounds and features executes
/// under the ones its session recorded: its prompt offers `continue_as`
/// (recorded decomposition) and its cell runs a loop the redeploying bound
/// would stop.
async fn an_rlm_run_executes_under_its_recorded_behaviour(tier: Tier) {
    let Some((output, requests)) = run_on_the_redeploying_core(
        tier,
        Deployment::RlmCreating,
        Deployment::RlmRedeploying,
        "rlm-recorded",
    )
    .await
    else {
        return;
    };
    let outcome = serde_json::to_string(&output.result.outcome).expect("outcome JSON");
    assert!(
        !outcome.contains(LOST_ANSWER),
        "the run's prompt withheld continue_as: {outcome}"
    );
    assert!(
        matches!(output.result.outcome, lash::TurnOutcome::Finished(_))
            && outcome.contains(&format!("ran {LOOP_ITERATIONS}")),
        "the run's cell ran its loop under the recorded bound: {outcome}"
    );
    assert_eq!(
        requests.len(),
        1,
        "one model call: the cell's finish ends the run"
    );
}

/// The host process's kernel document: a loop of [`LOOP_ITERATIONS`] turns,
/// far past the redeploying bound and far inside the recorded one, then its
/// answer. Published under a host pin; answers the start payload of its
/// `looper` entry.
async fn looping_process_payload(backend: &lash::Backend) -> serde_json::Value {
    let tens = "[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]";
    let text = format!(
        "kernel 1\nnumbers float\nentry looper() -> Any\n\nfn looper() {{\n  let last = 0\n  \
         for a in {tens} {{\n    for b in {tens} {{\n      for c in {tens} {{\n        \
         for d in [0, 1, 2, 3, 4] {{\n          set last = d\n        }}\n      }}\n    }}\n  }}\n  \
         return \"ran {LOOP_ITERATIONS}\"\n}}\n\nmain {{\n  finish null\n}}\n"
    );
    kernel_process::payload(backend, &text, "looper").await
}

/// A host starts a kernel process on the creating deployment under an
/// environment with no RLM namespace: its row records that deployment's
/// behaviour, and the redeploying deployment's node runs it under the
/// recorded behaviour, so its loop finishes where the running deployment's
/// instruction bound would stop it.
async fn a_host_started_process_runs_under_the_behaviour_its_creation_recorded(tier: Tier) {
    let Some((stores, keep)) = served::stores(tier).await else {
        return;
    };
    let backend = served::backend(stores);
    let requests = Arc::new(Requests::default());
    let payload = looping_process_payload(&backend).await;

    let first = Deployment::RlmCreating.core(&backend, &requests);
    let _ = first.session(lash::SessionId::try_from("host-process-anchor".to_owned()).unwrap());
    first
        .drain()
        .await
        .expect("the creating deployment's node drains");
    let env_ref = first
        .host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(16),
                    lash_core::NoProgressBudget::bounded(12),
                ),
                lash_core::SessionToolAccess::ambient(),
            ),
        )
        .await
        .expect("the environment is published");
    let started = first
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessInput::Engine {
                    kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.to_owned(),
                    payload,
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_env_ref(env_ref),
            first.effect_host(),
        )
        .await
        .expect("the drained core still records the start")
        .process_id;
    let record = backend
        .process_registry()
        .get_process(&started)
        .await
        .expect("read the process")
        .expect("the process is recorded");
    let recorded = record
        .engine_config
        .as_ref()
        .expect("creation records engine settings");
    assert_eq!(
        recorded["charge"],
        serde_json::json!(1_000_000),
        "the row records the creating deployment's bound"
    );

    let second = Deployment::RlmRedeploying.core(&backend, &requests);
    let _ = second.session(lash::SessionId::try_from("host-process-anchor".to_owned()).unwrap());
    let output = tokio::time::timeout(WATCHDOG, second.processes().await_output(&started))
        .await
        .expect("deadlock watchdog: the redeploying node never ran the process")
        .expect("the process's end is read");
    let spelled = serde_json::to_string(&output).expect("terminal JSON");
    assert!(
        matches!(
            output,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ) && spelled.contains(&format!("ran {LOOP_ITERATIONS}")),
        "the process ran its loop under the recorded bound: {spelled}"
    );
    second.shutdown().await.expect("the core shuts down");
    first.shutdown().await.expect("the core shuts down");
    drop(keep);
}

tiered_laws!(
    a_standard_run_executes_under_its_recorded_behaviour,
    an_rlm_run_executes_under_its_recorded_behaviour,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded,
);
