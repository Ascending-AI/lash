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
use lash_provider_fixtures::runtime_providers::{ScriptedUsage, runtime_script_value_for_turn};

const RETRIED_SESSION: &str = "sim-attempt-probe-retried";
const EXHAUSTED_SESSION: &str = "sim-attempt-probe-exhausted";

pub(super) async fn execute_attempt_usage_probe(
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
        .max_attempts(Some(2))
        .base_delay_ms(0)
        .max_delay_ms(0);
    options.reliability.retry.jitter_ms = 0;
    provider_handle.set_options(options);
    let collector = CheckpointWriteCollector::default();
    let engine = crate::backend::SimEngine::new(seed).await?;
    let reopen = engine.backend().session_store_factory();
    let backend: lash::Backend = crate::backend::DecoratedBackend::over_engine(&engine)
        .observing(collector.clone())
        .into();
    let core = lash::LashCore::standard_builder(backend.clone())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider_handle, model.clone())
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    core.session(lash_core::SessionId::fixture(session_id.to_string()))
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                model.wire_model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
            .charge_safety(charge_safety),
        ))
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session(lash_core::SessionId::fixture(session_id.to_string()))
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    // The exhausted turn is expected to fail; the retried one to succeed. The
    // oracle judges the durable outcome, so either result is evidence here and
    // only the exchange count is a precondition.
    let events = Arc::new(super::runtime_proofs::RuntimeProofRecordingEvents::default());
    let accepted = session
        .durable()
        .send(lash::TurnInput::text("Run the attempt usage probe."))
        .id(lash_core::TurnId::fixture(format!("{session_id}-turn")))
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let _outcome = crate::backend::settle_handle(accepted, events.clone()).await;
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
        &session.durable(),
        reopen.as_ref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a real generated run the durable-content law is green, the attempt
    /// probe produced the failed-after-usage shape it exists for, and corrupting
    /// one byte of a reopened message or dropping one recorded attempt turns it red.
    #[tokio::test]
    async fn durable_content_law_bites_on_real_run_evidence() {
        lifetime_usage_history().await;
        let workload = generate_workload(5, "fast-random", 96).expect("workload");
        let mut world = GeneratedRuntimeWorld::new(workload.seed)
            .await
            .expect("world");
        drive_generated_workload(&mut world, &workload)
            .await
            .expect("shift");
        let mut content = world.content_evidence().await.expect("content evidence");
        content.extend(execute_attempt_usage_probe(5).await.expect("probe"));
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
            .find(|session| !session.recorded_usage.is_empty())
            .expect("a session with committed usage")
            .recorded_usage
            .pop();
        let verdict = crate::content_oracle::durable_content(&dropped);
        assert!(!verdict.is_passed());
        assert!(
            verdict.message.contains("recorded attempts diverged"),
            "{}",
            verdict.message
        );
    }
    /// V11: a scripted lifetime, alongside the generated content history.
    /// Expected counts are literal provider facts, independent of row folding.
    async fn lifetime_usage_history() {
        use lash_durable::{ActorKey, ActorState, CommitLabel};
        use lash_durable_test::{Fault, Matrix, Script, SimNodes, SimNodesConfig, Tripwire};
        use lash_execution_contracts::{BackendParts, NoProjectionProviders};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        struct Renders;
        impl lash_durable_test::OffClockWork for Renders {
            fn busy(&self) -> bool {
                lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().0
            }
            fn ended(&self) -> usize {
                lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().1
            }
        }
        let clock = SimClock::new();
        clock.wait_out(Arc::new(Renders));
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
                lash_sqlite_store::SqliteStoreSetOptions {
                    inline_calls: true,
                    ..lash_sqlite_store::SqliteStoreSetOptions::memory()
                },
                clock.clone(),
            )
            .await
            .unwrap(),
        );
        let backend = lash_execution_contracts::Backend::assemble(BackendParts {
            stores: stores.clone(),
            settings: lash_core_execution::DurableSettings {
                idle_evict: Duration::from_secs(1),
                ..Default::default()
            },
            engines: Vec::new(),
            providers: Arc::new(NoProjectionProviders),
            formats: lash::formats::actor_state_surfaces(),
        })
        .unwrap();
        let mixed_sends = Arc::new(AtomicUsize::new(0));
        let provider = lash::testing::TestProvider::builder().kind("lifetime-usage")
            .options(lash_core::provider::ProviderOptions {
                reliability: lash_core::provider::ProviderReliability::default()
                    .max_attempts(Some(2)).base_delay_ms(0).max_delay_ms(0), ..Default::default()
            })
            .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
            .complete(move |request: LlmRequest| {
                let mixed_sends = mixed_sends.clone();
                async move {
                    let (code, usage) = if request.scope.session_id().unwrap().as_str() == "usage-child" {
                        ("await control.finish('child');", lash::LlmUsage { cache_read_input_tokens: 9, ..Default::default() })
                    } else if request.scope.turn.as_ref().unwrap().turn_id.as_str() == "usage-prior" {
                        ("await control.finish('prior');", lash::LlmUsage { input_tokens: 5, output_tokens: 2, ..Default::default() })
                    } else if request.scope.turn.as_ref().unwrap().turn_id.as_str() == "usage-after" {
                        ("await control.finish('after');", lash::LlmUsage { input_tokens: 3, output_tokens: 1, ..Default::default() })
                    } else {
                        match mixed_sends.fetch_add(1, Ordering::SeqCst) {
                            0 => return Err(LlmTransportError::new("retry before the owner is lost")
                                .with_retry_verdict(lash::provider::TransportRetryVerdict::RetryableTransient)),
                            1 => ("console.log('retried');", lash::LlmUsage { input_tokens: 11, output_tokens: 7, ..Default::default() }),
                            2 => {
                                let stream = request.stream_events.as_ref().unwrap();
                                stream.send(LlmStreamEvent::Usage(lash::LlmUsage { input_tokens: 2, output_tokens: 1, ..Default::default() }));
                                stream.send(LlmStreamEvent::Block(lash_core::llm::types::StreamBlockEvent::Delta {
                                    kind: lash_core::llm::types::StreamBlockKind::AssistantText,
                                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                                    text: "<typescript>\nconsole.log('accepted tail');\n</typescript>".to_owned(),
                                }));
                                return std::future::pending().await;
                            }
                            3 => ("await control.finish('mixed');", lash::LlmUsage { input_tokens: 11, output_tokens: 7, ..Default::default() }),
                            n => panic!("unexpected mixed send {n}"),
                        }
                    };
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text { text: format!("<typescript>\n{code}\n</typescript>"), response_meta: None }],
                        usage, terminal_reason: LlmTerminalReason::Stop, ..Default::default()
                    })
                }
            }).build().into_handle();
        let mut workers = lash::vm::WorkerService::default().config().clone();
        let off_clock = Duration::from_secs(365 * 24 * 3600);
        workers.deadlines.compute = off_clock;
        workers.deadlines.serialization = off_clock;
        workers.deadlines.cumulative_cpu = off_clock;
        workers.deadlines.execution_wall = off_clock;
        workers.deadlines.checkout = off_clock;
        workers.protocol.no_response_watchdog = off_clock;
        let worker_clock = clock.clone();
        let workers = lash::vm::WorkerService::new(workers)
            .with_call_hold(Arc::new(move || Box::new(worker_clock.hold())));
        let protocol = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(workers);
        let total = Duration::from_secs(10);
        let core = lash::LashCore::rlm_builder(backend.clone(), protocol)
            .serve_sessions(false)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(
                lash::ExecutionBudgets::new(lash_core::ExecutionBudgetsConfig {
                    model_total: total,
                    stop_grace: Duration::from_millis(50),
                    provider: lash_core::ProviderAttemptLimits::new(total, total, total, 2)
                        .unwrap(),
                    ..lash_core::ExecutionBudgetsConfig::recommended()
                })
                .unwrap(),
            )
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .serve_test_llm_profile(
                provider,
                lash::LlmProfileMetadata::builder("usage-model")
                    .cache_retention(lash_core::provider::CacheRetention::Short)
                    .context_window_tokens(200_000)
                    .build()
                    .unwrap(),
            )
            .build(crate::sim_process_owner())
            .unwrap();
        let script = Script::new();
        script.cut_on(
            "a",
            CommitLabel::new("model.attempt"),
            1,
            Fault::CommitThenAbort,
        );
        let nodes = SimNodes::new(
            Arc::new(stores.durable_store()),
            clock.clone(),
            script,
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: backend.formats().decodes(),
                serves: lash::testing::node_serves(&core),
                max_active: 4,
            },
            Arc::new(
                lash_core::runtime::durable::session::SessionActivation::new(
                    backend.clone(),
                    lash::testing::session_turn_services(&core),
                    Arc::new(Tripwire::new()) as _,
                ),
            ),
        );
        let spec = || {
            lash::SessionSpec::new(
                "usage-model",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12))
        };
        let parent_id = lash::SessionId::fixture("usage-parent");
        let parent = core
            .session(parent_id.clone())
            .create(lash::SessionCreation::root(spec()))
            .await
            .unwrap();
        let child = core
            .session(lash::SessionId::fixture("usage-child"))
            .create(lash::SessionCreation::child_of(parent_id, spec()))
            .await
            .unwrap();
        nodes.start("a");
        let mut turns = Vec::new();
        for (index, (name, expected)) in [
            ("usage-prior", (5, 2, 0)),
            ("usage-mixed", (29, 17, 1)),
            ("usage-after", (32, 18, 1)),
        ]
        .into_iter()
        .enumerate()
        {
            let turn = lash::TurnId::fixture(name);
            let handle = parent
                .send(lash::TurnInput::text(name))
                .id(turn.clone())
                .await
                .unwrap();
            if index == 1 {
                until(&nodes, &clock, || {
                    nodes
                        .script()
                        .trace()
                        .iter()
                        .any(|cut| cut.point.label == CommitLabel::new("model.attempt"))
                })
                .await;
                nodes.kill("a");
                nodes.quiesce().await;
                nodes.start("b");
            }
            idle(&nodes, &clock, "usage-parent").await;
            assert!(handle.output().await.unwrap().is_success());
            let head = parent.read().await.unwrap().unwrap();
            let usage = head.token_usage();
            assert_eq!(
                (usage.input_tokens, usage.output_tokens, usage.unknown_calls),
                expected
            );
            turns.push(parent.read_turn_model_calls(&turn).await.unwrap());
        }
        let mixed = &turns[1];
        assert_eq!(mixed.len(), 3);
        assert_eq!(mixed[0].record.attempts.len(), 3);
        for lost in &mixed[0].record.attempts[..2] {
            assert_eq!(
                lost.outcome,
                lash_core::llm::types::AttemptOutcome::Interrupted
            );
            assert_eq!(
                lost.protocol_position,
                lash_core::llm::types::ProtocolPosition::Unobserved
            );
            assert_eq!(lost.usage, None);
        }
        assert_eq!(
            mixed[1].record.attempts[0].outcome,
            lash_core::llm::types::AttemptOutcome::Interrupted
        );
        assert!(mixed[1].record.attempts[0].usage.is_some());
        let expected = lash::LlmUsage {
            input_tokens: 24,
            output_tokens: 15,
            unknown_calls: 1,
            ..Default::default()
        };
        let sum = |rows: &[lash::turn::ModelCallRecord]| {
            rows.iter()
                .try_fold(lash::LlmUsage::default(), |sum, call| {
                    sum.checked_add(&call.usage()?)
                })
                .unwrap()
        };
        for (rows, (input, output, unknown)) in
            turns.iter().zip([(5, 2, 0), (24, 15, 1), (3, 1, 0)])
        {
            let usage = sum(rows);
            assert_eq!(
                (usage.input_tokens, usage.output_tokens, usage.unknown_calls),
                (input, output, unknown)
            );
        }
        assert_eq!(sum(mixed), expected);
        let mut known_zero = mixed.clone();
        for lost in &mut known_zero[0].record.attempts[..2] {
            lost.usage = Some(Default::default());
        }
        assert_ne!(
            sum(&known_zero),
            expected,
            "known zero must not replace absent crash usage"
        );
        let child_turn = lash::TurnId::fixture("usage-child-turn");
        let handle = child
            .send(lash::TurnInput::text("child"))
            .id(child_turn.clone())
            .await
            .unwrap();
        idle(&nodes, &clock, "usage-child").await;
        assert!(handle.output().await.unwrap().is_success());
        let child_rows = child.read_turn_model_calls(&child_turn).await.unwrap();
        let child_expected = lash::LlmUsage {
            cache_read_input_tokens: 9,
            ..Default::default()
        };
        assert_eq!(sum(&child_rows), child_expected);
        assert_eq!(
            child.read().await.unwrap().unwrap().token_usage(),
            &child_expected
        );
        assert!(
            child
                .read_turn_model_calls(&lash::TurnId::fixture("usage-mixed"))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            parent
                .read_turn_model_calls(&child_turn)
                .await
                .unwrap()
                .is_empty()
        );
        assert_ne!(
            sum(mixed),
            sum(&child_rows),
            "projecting parent rows onto the child must fail"
        );
        let head = parent.read().await.unwrap().unwrap();
        assert_eq!(
            (
                head.token_usage().input_tokens,
                head.token_usage().output_tokens,
                head.token_usage().cache_read_input_tokens
            ),
            (32, 18, 0)
        );
        nodes.kill("a");
        nodes.kill("b");
        drop(parent);
        drop(child);
        drop(core);

        async fn until(nodes: &SimNodes, clock: &SimClock, done: impl Fn() -> bool) {
            let end = clock.logical_ms() + 600_000;
            nodes.quiesce().await;
            while !done() {
                assert!(
                    clock.logical_ms() < end && nodes.step().await.is_some(),
                    "{}",
                    nodes.script().rendered_trace()
                );
            }
        }
        async fn idle(nodes: &SimNodes, clock: &SimClock, session: &str) {
            let actor = ActorKey::session(session).unwrap();
            let end = clock.logical_ms() + 600_000;
            loop {
                nodes.quiesce().await;
                if matches!(nodes.database().actor(&actor).await, Ok(Some(row)) if row.state == ActorState::Idle)
                {
                    break;
                }
                assert!(
                    clock.logical_ms() < end && nodes.step().await.is_some(),
                    "{}",
                    nodes.script().rendered_trace()
                );
            }
        }
    }
}
