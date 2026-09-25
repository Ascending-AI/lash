//! Per-seed provider attempts that fail after reporting usage.
//!
//! The generated workload's provider turns all complete on their first
//! attempt, so the durable-content oracle would never see a failed attempt
//! that billed tokens. This probe runs two real Anthropic turns per seed over
//! their own store, each with seed-drawn text and usage:
//!
//! * **retried** — attempt 1 streams `message_start` (reporting input usage)
//!   and then disconnects retryably; the session accepts duplicate billing, so
//!   attempt 2 runs and completes;
//! * **exhausted** — the same failing attempt under the default charge-safety
//!   policy, which refuses the unsafe retry, so every attempt of the call fails.
//!
//! Their content joins the durable-content evidence. The registered law checks
//! the retried turn's committed message and completed-attempt usage; the
//! failed-attempt law (FIG-3514) checks that both failed attempts' usage was
//! ledgered too.

use super::*;
use crate::content_oracle::SessionContent;
use crate::generator::{generated_text, generated_usage};
use crate::runtime_providers::{ScriptedUsage, runtime_script_value_for_turn};

const RETRIED_SESSION: &str = "sim-attempt-probe-retried";
const EXHAUSTED_SESSION: &str = "sim-attempt-probe-exhausted";

pub(super) async fn drive_attempt_usage_probe(
    seed: u64,
) -> Result<Vec<SessionContent>, FixedScriptRunnerError> {
    let retried_failure = failing_attempt_script(seed, RETRIED_SESSION)?;
    let text = generated_text(seed, &format!("{RETRIED_SESSION} answer "));
    let usage = generated_usage(seed, &format!("{RETRIED_SESSION} answer "));
    let success = runtime_script_value_for_turn(ANTHROPIC, &text, Some(&usage))
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let success = ProviderWireScript::from_json_str(&success.to_string())?;
    let retried = probe_session(
        seed,
        RETRIED_SESSION,
        vec![retried_failure, success],
        lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 1,
            max_duplicate_cost_tokens: None,
        },
        2,
    )
    .await?;
    let exhausted = probe_session(
        seed,
        EXHAUSTED_SESSION,
        vec![failing_attempt_script(seed, EXHAUSTED_SESSION)?],
        lash_core::ChargeSafetyPolicy::default(),
        1,
    )
    .await?;
    Ok(vec![retried, exhausted])
}

/// An Anthropic attempt that reports seed-drawn input usage on `message_start`
/// and then loses its stream to a retryable disconnect.
fn failing_attempt_script(
    seed: u64,
    session: &str,
) -> Result<ProviderWireScript, FixedScriptRunnerError> {
    let drawn = generated_usage(seed, &format!("{session} failed attempt "));
    // Only `message_start` arrives, and it carries the input-side buckets.
    let usage = ScriptedUsage {
        output_tokens: 0,
        reasoning_output_tokens: 0,
        ..drawn
    };
    let mut script = runtime_script_value_for_turn(ANTHROPIC, "", Some(&usage))
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let timeline = script
        .get_mut("timeline")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| FixedScriptRunnerError::Assertion("probe script timeline".to_string()))?;
    timeline.truncate(2);
    timeline.push(json!({
        "at": 21,
        "event": "disconnect",
        "message": "probe stream lost after message_start",
        "retryable": true,
    }));
    script["name"] = json!(format!("anthropic.probe-failed-after-usage.{session}"));
    script["expected_provider"] = json!({"stream_error": "probe stream lost after message_start"});
    Ok(ProviderWireScript::from_json_str(&script.to_string())?)
}

async fn probe_session(
    seed: u64,
    session_id: &str,
    scripts: Vec<ProviderWireScript>,
    charge_safety: lash_core::ChargeSafetyPolicy,
    expected_exchanges: usize,
) -> Result<SessionContent, FixedScriptRunnerError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts(scripts.clone())?);
    let (mut provider_handle, model, _) = runtime_provider_components(ANTHROPIC, &transport)
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let mut options = provider_handle.options();
    options.reliability = lash_core::provider::ProviderReliability::default()
        .max_attempts(2)
        .base_delay_ms(0)
        .max_delay_ms(0);
    options.reliability.retry.jitter_ms = 0;
    provider_handle.set_options(options);
    let collector = CheckpointWriteCollector::default();
    let engine = crate::backend::SimEngine::new(seed).await?;
    let store_factory: Arc<dyn SessionStoreFactory> =
        lash::Backend::session_store_factory(engine.backend().as_ref());
    let backend = Arc::new(
        crate::backend::DecoratedBackend::over_engine(&engine).observing(collector.clone()),
    );
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .without_queued_work()
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .charge_safety(charge_safety)
        .provider(provider_handle)
        .model(model)
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session(session_id.to_string())
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    // The exhausted turn is expected to fail; the retried one to succeed. The
    // oracle judges the durable outcome, so either result is evidence here and
    // only the exchange count is a precondition.
    let _outcome = engine
        .run_turn(
            &session,
            format!("{session_id}-turn"),
            Arc::new(super::runtime_proofs::RuntimeProofRecordingEvents::default()),
            Arc::new(|session: &lash::LashSession| {
                Ok(session.turn(lash::TurnInput::text("Run the attempt usage probe.")))
            }),
        )
        .await?;
    let exchanged = transport.exchanges()?.len();
    if exchanged != expected_exchanges {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "attempt probe `{session_id}` ran {exchanged} provider exchange(s), expected {expected_exchanges}; the probe no longer produces the attempt shape it exists for"
        )));
    }
    session_content(
        session_id,
        transport.as_ref(),
        &scripts,
        Vec::new(),
        &collector.events(),
        store_factory.as_ref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a real generated run the durable-content law is green, the attempt
    /// probe produced the failed-after-usage shape it exists for, and corrupting
    /// one byte of a reopened message or dropping one committed delta turns it red.
    #[tokio::test]
    async fn durable_content_law_bites_on_real_run_evidence() {
        let workload = generate_workload(5, "fast-random", 96).expect("workload");
        let mut world = GeneratedRuntimeWorld::new().await.expect("world");
        drive_generated_workload(&mut world, &workload)
            .await
            .expect("drive");
        let mut content = world
            .content_evidence(world.reopen_factory().as_ref())
            .await
            .expect("content evidence");
        content.extend(drive_attempt_usage_probe(5).await.expect("probe"));
        let verdict = crate::content_oracle::durable_content(&content);
        assert!(verdict.is_passed(), "{}", verdict.message);

        assert!(
            content.iter().any(|session| {
                session
                    .emitted_attempts
                    .iter()
                    .any(|attempt| !attempt.completed && attempt.usage.is_some())
                    && session
                        .emitted_attempts
                        .iter()
                        .any(|attempt| attempt.completed)
            }),
            "the probe must fail an attempt after it reported usage and then complete a retry"
        );

        let mut corrupted = content.clone();
        let message = corrupted
            .iter_mut()
            .filter_map(|session| session.reopened.as_mut())
            .flat_map(|reopened| reopened.assistant_messages.iter_mut())
            .find(|message| !message.text.is_empty())
            .expect("a committed assistant message");
        message.text.pop();
        let verdict = crate::content_oracle::durable_content(&corrupted);
        assert!(!verdict.is_passed());
        assert!(
            verdict.message.contains("diverged after reopen"),
            "{}",
            verdict.message
        );

        let mut dropped = content;
        dropped
            .iter_mut()
            .find(|session| !session.committed_usage.is_empty())
            .expect("a session with committed usage")
            .committed_usage
            .pop();
        let verdict = crate::content_oracle::durable_content(&dropped);
        assert!(!verdict.is_passed());
        assert!(
            verdict.message.contains("committed delta"),
            "{}",
            verdict.message
        );
    }
}
