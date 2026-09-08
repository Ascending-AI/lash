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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_task(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    run: usize,
    channel: crate::ChannelSelection,
    effort: crate::ReasoningEffort,
    turn_wall_limit_secs: u64,
) -> (World, RunEvidence) {
    let started = std::time::Instant::now();
    // Every run owns its world, telemetry, provider and in-memory stores. No
    // process environment mutations, listeners or filesystem stores are used.
    let telemetry = Arc::new(crate::telemetry::Telemetry::default());
    let world = SharedWorld::new(task.seed.clone());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(turn_wall_limit_secs),
        run_turn(
            task, dialect, model, api_key, run, channel, effort, &world, &telemetry,
        ),
    )
    .await;
    let (completed, completion_error, finish_value, decisions) = match result {
        Ok(Ok((output, decisions))) => (
            output.is_success(),
            (!output.is_success()).then(|| format!("turn outcome: {:?}", output.result.outcome)),
            if channel == crate::ChannelSelection::Standard {
                world.submissions().first().cloned()
            } else {
                output.final_value().cloned()
            },
            decisions,
        ),
        Ok(Err(error)) => (
            false,
            Some(format!("{error:#}")),
            world.submissions().first().cloned(),
            Vec::new(),
        ),
        Err(_) => (
            false,
            Some("wall_limit".into()),
            world.submissions().first().cloned(),
            Vec::new(),
        ),
    };
    let activities = telemetry.activities();
    let attempts = telemetry.rows(&decisions, channel == crate::ChannelSelection::Standard);
    let iterations = activities
        .iter()
        .filter_map(|activity| match activity.event {
            TurnEvent::ModelRequestStarted { protocol_iteration } => Some(protocol_iteration),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len();
    let executions = activities
        .iter()
        .filter(|activity| matches!(activity.event, TurnEvent::CodeBlockStarted { .. }))
        .count();
    let tool_call_count = activities.iter().filter(|activity| matches!(&activity.event, TurnEvent::ToolCallStarted { name, .. } if name != "submit")).count();
    let failed_execution_errors = activities
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
        world.snapshot(),
        RunEvidence {
            standard: channel == crate::ChannelSelection::Standard,
            rounds: attempts.len(),
            submit_count: telemetry.submit_count(),
            attempts,
            wall_ms: started.elapsed().as_millis(),
            completed,
            completion_error,
            finish_value,
            iterations,
            executions,
            tool_call_count,
            failed_execution_errors,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    run: usize,
    channel: crate::ChannelSelection,
    effort: crate::ReasoningEffort,
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
    let budget = if task.id.starts_with("__") {
        lash::TurnBudget::bounded(1)
    } else {
        lash::TurnBudget::Unbounded
    };
    let builder = match channel {
        crate::ChannelSelection::Standard => LashCore::standard_builder(budget),
        crate::ChannelSelection::Cell | crate::ChannelSelection::Native => {
            let factory = lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                    .wall_clock(lash::rlm::WallClockBound::secs(30))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build()
                    .with_channel(if channel == crate::ChannelSelection::Cell {
                        lash::rlm::RlmChannel::Cell
                    } else {
                        lash::rlm::RlmChannel::NativeTool
                    }),
                Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
            );
            LashCore::rlm_builder(budget, factory)
        }
    };
    let core = builder
        .no_progress_budget(lash::NoProgressBudget::Unbounded)
        .without_queued_work()
        .plugins(lash::plugins::runtime_plugin_stack().configure(|stack| {
            stack.push(telemetry.plugin());
        }))
        .provider(provider)
        .model(model_spec(model, effort)?)
        .tools(if channel == crate::ChannelSelection::Standard {
            world.standard_provider()
        } else {
            world.provider()
        })
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
    let session_builder = core.session(session_id);
    let session_builder = if channel == crate::ChannelSelection::Standard {
        session_builder
    } else {
        session_builder
            .plugin_option(lash::rlm::RLM_PROTOCOL_PLUGIN_ID, session_options(dialect))
            .context("encode dialect session option")?
    };
    let session = session_builder
        .open()
        .await
        .context("open toolbench session")?;
    let turn = session.turn(TurnInput::text(
        task.prompt_for(channel == crate::ChannelSelection::Standard),
    ));
    let turn = if channel == crate::ChannelSelection::Standard {
        turn
    } else {
        turn.require_finish().context("require RLM finish value")?
    };
    let result = turn
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
const PREFLIGHT_ATTEMPTS: usize = 2;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn preflight(
    task: &Task,
    dialect: lash::rlm::RlmDialect,
    model: &str,
    api_key: &str,
    channel: crate::ChannelSelection,
    effort: crate::ReasoningEffort,
    turn_wall_limit_secs: u64,
) -> Result<(), String> {
    let mut probe = task.clone();
    probe.id = "__native_probe";
    probe.prompt = "Call execute_code exactly once with code that finishes with the number 1. Do not call any host operations.".into();
    if channel == crate::ChannelSelection::Standard {
        probe.prompt = "Call submit exactly once with value set to the JSON number 1, not the string \"1\". Do not call any host tools.".into();
    }
    probe.tool_calls = 0;
    probe.expected_world = probe.seed.clone();
    probe.finish = crate::tasks::FinishMatcher::Exact(serde_json::json!(1));
    let mut failures = Vec::with_capacity(PREFLIGHT_ATTEMPTS);
    for attempt in 0..PREFLIGHT_ATTEMPTS {
        let (_, evidence) = run_task(
            &probe,
            dialect,
            model,
            api_key,
            attempt,
            channel,
            effort,
            turn_wall_limit_secs,
        )
        .await;
        if crate::grading::grade(&probe, &probe.seed, &evidence, f64::INFINITY).passed
            && evidence.tool_call_count == 0
            && (channel == crate::ChannelSelection::Standard || evidence.executions == 1)
        {
            return Ok(());
        }
        failures.push(evidence.completion_error.unwrap_or_else(|| {
            format!(
                "{} one-call probe finished with {:?} instead of 1",
                channel.name(),
                evidence.finish_value
            )
        }));
    }
    Err(format!(
        "{PREFLIGHT_ATTEMPTS} probe attempts failed: {}",
        failures.join(" | ")
    ))
}

fn model_spec(model: &str, effort: crate::ReasoningEffort) -> Result<lash::ModelSpec> {
    use lash::provider::{ModelCapability, ReasoningCapability, ReasoningSelection};
    let variant = match effort {
        crate::ReasoningEffort::None => ReasoningSelection::ProviderDefault,
        _ => ReasoningSelection::Effort(effort.name().into()),
    };
    lash::ModelSpec::builder(model)
        .context_window_tokens(200_000)
        .variant(variant)
        .capability(ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .build()
        .context("build model metadata")
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
    fn reasoning_selection_is_shared_and_none_keeps_provider_default() {
        use crate::ReasoningEffort;
        use lash::provider::ReasoningSelection;
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            let spec = super::model_spec("route/model", effort).unwrap();
            assert_eq!(
                spec.variant,
                ReasoningSelection::Effort(effort.name().into())
            );
            assert!(
                spec.capability
                    .reasoning
                    .unwrap()
                    .efforts
                    .contains(&effort.name().to_string())
            );
        }
        assert_eq!(
            super::model_spec("route/model", ReasoningEffort::None)
                .unwrap()
                .variant,
            ReasoningSelection::ProviderDefault
        );
    }

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
