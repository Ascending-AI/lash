use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::rlm::RlmTurnBuilderExt as _;
use lash::{LashCore, TurnEvent, TurnInput};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

use crate::grading::RunEvidence;
use crate::tasks::Task;
use crate::world::{SharedWorld, World};

const TURN_BUDGET: usize = 8;
const NO_PROGRESS_BUDGET: usize = 3;
const TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub(crate) async fn run_task(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    run: usize,
) -> (World, RunEvidence) {
    let world = SharedWorld::new(task.seed.clone());
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        run_turn(task, dialect, model, api_key, run, &world),
    )
    .await;
    let final_world = world.snapshot();
    match result {
        Ok(Ok(output)) => {
            let iterations = output
                .activities
                .iter()
                .filter_map(|activity| match &activity.event {
                    TurnEvent::ModelRequestStarted { protocol_iteration } => {
                        Some(*protocol_iteration)
                    }
                    _ => None,
                })
                .collect::<BTreeSet<_>>()
                .len();
            let failed_execution_errors = output
                .activities
                .iter()
                .filter_map(|activity| match &activity.event {
                    TurnEvent::CodeBlockCompleted {
                        success: false,
                        error,
                        output,
                        ..
                    } => Some(
                        error
                            .as_deref()
                            .filter(|message| !message.trim().is_empty())
                            .unwrap_or(output)
                            .trim()
                            .to_string(),
                    ),
                    _ => None,
                })
                .collect();
            (
                final_world,
                RunEvidence {
                    completed: output.is_success(),
                    completion_error: (!output.is_success())
                        .then(|| format!("turn outcome: {:?}", output.result.outcome)),
                    finish_value: output.final_value().cloned(),
                    iterations,
                    tool_call_count: output.result.tool_calls.len(),
                    failed_execution_errors,
                },
            )
        }
        Ok(Err(error)) => (
            final_world,
            RunEvidence {
                completion_error: Some(format!("{error:#}")),
                ..RunEvidence::default()
            },
        ),
        Err(_) => (
            final_world,
            RunEvidence {
                completion_error: Some(format!(
                    "turn exceeded the {} second wall-clock limit",
                    TURN_TIMEOUT.as_secs()
                )),
                ..RunEvidence::default()
            },
        ),
    }
}

async fn run_turn(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    run: usize,
    world: &SharedWorld,
) -> Result<lash::TurnOutput> {
    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key.to_string(), OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = LashCore::rlm_builder(lash::TurnBudget::bounded(TURN_BUDGET), factory)
        .no_progress_budget(lash::NoProgressBudget::bounded(NO_PROGRESS_BUDGET))
        .without_queued_work()
        .plugins(lash::plugins::runtime_plugin_stack())
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model)
                .context_window_tokens(200_000)
                .build()
                .context("build model metadata")?,
        )
        .tools(world.provider())
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "toolbench",
            format!("run-{run}-{}-{}", dialect.language_id(), task.id),
        ))
        .context("build Lash core")?;
    let session_id = format!("toolbench-{run}-{}-{}", dialect.language_id(), task.id);
    let session = core
        .session(session_id)
        .plugin_option(
            lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
            lash::rlm::RlmCreateExtras {
                dialect: Some(dialect),
                ..lash::rlm::RlmCreateExtras::default()
            },
        )
        .context("encode dialect session option")?
        .open()
        .await
        .context("open toolbench session")?;
    session
        .turn(TurnInput::text(task.prompt))
        .require_finish()
        .context("require RLM finish value")?
        .run()
        .await
        .context("run toolbench turn")
}
