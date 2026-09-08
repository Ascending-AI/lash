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
    channel: lash::rlm::RlmChannel,
) -> (World, RunEvidence) {
    let started = std::time::Instant::now();
    // Every run owns its world, telemetry, provider and in-memory stores. No
    // process environment mutations, listeners or filesystem stores are used.
    let telemetry = Arc::new(crate::telemetry::Telemetry::default());
    let world = SharedWorld::new(task.seed.clone());
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        run_turn(
            task, dialect, model, api_key, run, channel, &world, &telemetry,
        ),
    )
    .await;
    let final_world = world.snapshot();
    match result {
        Ok(Ok((output, decisions))) => {
            let attempts = telemetry.rows(&decisions);
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
                            .as_ref()
                            .map(|failure| failure.message.as_str())
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
                    attempts,
                    wall_ms: started.elapsed().as_millis(),
                    completed: output.is_success(),
                    completion_error: (!output.is_success())
                        .then(|| format!("turn outcome: {:?}", output.result.outcome)),
                    finish_value: output.final_value().cloned(),
                    iterations,
                    code_blocks: output
                        .activities
                        .iter()
                        .filter_map(|activity| match &activity.event {
                            TurnEvent::CodeBlockStarted { code, .. } => Some(code.clone()),
                            _ => None,
                        })
                        .collect(),
                    tool_call_count: output.result.tool_calls.len(),
                    failed_execution_errors,
                },
            )
        }
        Ok(Err(error)) => (
            final_world,
            RunEvidence {
                attempts: telemetry.rows(&[]),
                wall_ms: started.elapsed().as_millis(),
                completion_error: Some(format!("{error:#}")),
                ..RunEvidence::default()
            },
        ),
        Err(_) => (
            final_world,
            RunEvidence {
                attempts: telemetry.rows(&[]),
                wall_ms: started.elapsed().as_millis(),
                completion_error: Some(format!(
                    "turn exceeded the {} second wall-clock limit",
                    TURN_TIMEOUT.as_secs()
                )),
                ..RunEvidence::default()
            },
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    run: usize,
    channel: lash::rlm::RlmChannel,
    world: &SharedWorld,
    telemetry: &Arc<crate::telemetry::Telemetry>,
) -> Result<(lash::TurnOutput, Vec<String>)> {
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
            .build()
            .with_channel(channel),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = LashCore::rlm_builder(
        lash::TurnBudget::bounded(if task.id == "__native_probe" {
            1
        } else {
            TURN_BUDGET
        }),
        factory,
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(NO_PROGRESS_BUDGET))
    .without_queued_work()
    .plugins(lash::plugins::runtime_plugin_stack().configure(|stack| {
        stack.push(telemetry.plugin());
    }))
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
        .plugin_option(lash::rlm::RLM_PROTOCOL_PLUGIN_ID, session_options(dialect))
        .context("encode dialect session option")?
        .open()
        .await
        .context("open toolbench session")?;
    let result = session
        .turn(TurnInput::text(task.prompt.clone()))
        .require_finish()
        .context("require RLM finish value")?
        .stream_to(telemetry.as_ref())
        .await
        .context("run toolbench turn")?;
    let decisions = session
        .read_view()
        .active_events()
        .iter()
        .filter_map(|record| {
            let lash::persistence::SessionHistoryRecord::Protocol(event) = record else {
                return None;
            };
            if event.plugin_id != lash::rlm::RLM_PROTOCOL_PLUGIN_ID {
                return None;
            }
            let diagnostic = event.payload.get("RlmDiagnostic")?;
            let phase = diagnostic.get("phase")?.as_str()?;
            if !["llm_extraction", "native_extraction"].contains(&phase) {
                return None;
            }
            diagnostic
                .get("payload")?
                .get("decision")?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    Ok((
        lash::TurnOutput {
            result,
            activities: telemetry.activities(),
        },
        decisions,
    ))
}

/// Native capability probes are sampled model behaviour, so one stochastic
/// miss must not exclude a whole cohort; the route is excluded only when
/// every probe attempt fails.
const PREFLIGHT_ATTEMPTS: usize = 3;

pub(crate) async fn preflight(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
) -> Result<(), String> {
    let mut probe = task.clone();
    probe.id = "__native_probe";
    probe.prompt = "Call execute_code exactly once with code that finishes with the number 1. Do not call any host operations.".into();
    let mut failures = Vec::with_capacity(PREFLIGHT_ATTEMPTS);
    for attempt in 0..PREFLIGHT_ATTEMPTS {
        let (_, evidence) = run_task(
            &probe,
            dialect,
            model,
            api_key,
            attempt,
            lash::rlm::RlmChannel::NativeTool,
        )
        .await;
        if evidence.completed && evidence.finish_value == Some(serde_json::json!(1)) {
            return Ok(());
        }
        failures.push(evidence.completion_error.unwrap_or_else(|| {
            format!(
                "native one-call probe finished with {:?} instead of 1",
                evidence.finish_value
            )
        }));
    }
    Err(format!(
        "{PREFLIGHT_ATTEMPTS} probe attempts failed: {}",
        failures.join(" | ")
    ))
}

fn session_options(dialect: lash::rlm::RlmDialect) -> lash::rlm::RlmCreateExtras {
    lash::rlm::RlmCreateExtras {
        dialect: Some(dialect),
        final_answer_format: Some(lash::rlm::RlmFinalAnswerFormat::RawFinalValue),
        ..lash::rlm::RlmCreateExtras::default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn benchmark_sessions_use_raw_finish_values_for_every_dialect() {
        for dialect in lash::rlm::RlmDialect::ALL {
            let options = super::session_options(dialect);
            assert_eq!(options.dialect, Some(dialect));
            assert_eq!(
                options.final_answer_format,
                Some(lash::rlm::RlmFinalAnswerFormat::RawFinalValue)
            );
        }
    }
}
