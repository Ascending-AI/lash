//! A cell race's losing call on the durable turn (L06, L17, C04): it keeps
//! running while the cell that raced it waits on something else, and when
//! the cell's run closes it is stopped, under the closing's own cause.

use super::*;
use crate::TurnInput;
use lash_core::ToolDefinitionBindingExt as _;
use std::time::{Duration, Instant};

/// How long the cell sleeps after its race.
const SLEEP_MS: u64 = 3_000;
const FAST: &str = "fast";
const SLOW: &str = "slow";

/// `fast` answers at once; `slow` opens `entered`, waits for `gate`, then
/// opens `midpoint` and answers.
#[derive(Clone, Default)]
struct RaceTools {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
    midpoint: Arc<tokio::sync::Notify>,
}

fn definition(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A race operand.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

#[async_trait]
impl ToolProvider for RaceTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [FAST, SLOW]
            .into_iter()
            .map(|name| definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [FAST, SLOW]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == SLOW {
            self.entered.notify_one();
            self.gate.notified().await;
            self.midpoint.notify_one();
        }
        lash_core::ToolOutcome::ok(serde_json::json!(call.name())).into()
    }
}

/// An RLM core over SQLite memory whose model answers with one cell running
/// `source`, serving `tools`; the cell may sleep.
fn race_core(
    backend: lash_core::Backend,
    source: &str,
    tools: Arc<dyn ToolProvider>,
) -> Result<LashCore> {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(lash_protocol_rlm::RlmAbilities::all()),
        Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    )
    .with_worker_service(untimed_fixture_workers());
    Ok(
        explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
            .serve_test_llm_profile(
                queued_text_provider(vec![typescript_block(source)]),
                mock_llm_profile_spec(),
            )
            .tools(tools)
            .build(crate::testing::runtime_lease_owner())?,
    )
}

/// A race loser's body progresses while the program sleeps: the cell races
/// `slow` against `fast`, `fast` wins, and the cell sleeps. Released while
/// the cell sleeps, the losing `slow` reaches its midpoint long before the
/// sleep is due, and the turn answers only after the sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l06_race_loser_body_progresses_while_the_program_sleeps() -> Result<()> {
    let tools = RaceTools::default();
    let core = race_core(
        sqlite_memory_store_backend().await,
        &format!(
            "const winner = await Promise.race([tools.{SLOW}({{}}), tools.{FAST}({{}})]);\n\
             await sleep({SLEEP_MS});\n\
             finish(winner);"
        ),
        Arc::new(tools.clone()),
    )?;
    let session = core
        .session(crate::SessionId::parse("race-loser-progress").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let sent = Instant::now();
    let handle = session.send(TurnInput::text("race then sleep")).await?;
    tokio::time::timeout(Duration::from_secs(30), tools.entered.notified())
        .await
        .expect("the losing call's body starts");
    // The winner answers at once and the cell goes on to its sleep.
    tokio::time::sleep(Duration::from_millis(SLEEP_MS / 6)).await;
    tools.gate.notify_one();
    tokio::time::timeout(
        Duration::from_millis(SLEEP_MS / 2),
        tools.midpoint.notified(),
    )
    .await
    .expect("the loser reaches its midpoint while the program sleeps");
    let progressed_at = sent.elapsed();
    let output = tokio::time::timeout(Duration::from_secs(30), handle.output())
        .await
        .expect("the turn answers")?;
    let answered_at = sent.elapsed();
    assert!(output.is_success(), "{:?}", output.result.outcome);
    assert!(
        progressed_at < Duration::from_millis(SLEEP_MS)
            && answered_at >= Duration::from_millis(SLEEP_MS),
        "the loser progressed at {progressed_at:?}, inside the sleep the turn answered after at \
         {answered_at:?}"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// How a closing-race loser answers its stop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loser {
    /// It watches its stop, finishes the work it owes after it, and answers
    /// its own cancellation.
    Cooperative,
    /// It never answers.
    Silent,
}

/// `fast` answers once `slow`'s body is live; `slow` answers its stop as
/// its [`Loser`] says, and `settled` records that its body finished.
#[derive(Clone)]
struct ClosingTools {
    loser: Loser,
    entered: Arc<tokio::sync::Semaphore>,
    settled: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolProvider for ClosingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [FAST, SLOW]
            .into_iter()
            .map(|name| definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [FAST, SLOW]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == FAST {
            let _live = self.entered.acquire().await.expect("the gate stays open");
            return lash_core::ToolOutcome::ok(serde_json::json!(FAST)).into();
        }
        let stop = call
            .context
            .cancellation_token()
            .cloned()
            .expect("an attempt carries the cooperative token");
        self.entered.add_permits(1);
        if self.loser == Loser::Silent {
            std::future::pending::<()>().await;
        }
        stop.cancelled().await;
        // Work the body still owes after its stop: dropping the body here
        // loses it.
        tokio::task::yield_now().await;
        self.settled.store(true, Ordering::SeqCst);
        lash_core::ToolOutcome::cancelled("A observed its stop").into()
    }
}

/// A turn whose cell races `slow` against `fast` and finishes with the
/// winner: whether the loser's body settled, and the loser's recorded
/// completion.
async fn closing_race(loser: Loser) -> Result<(bool, lash_core::ToolCallOutput)> {
    let tools = ClosingTools {
        loser,
        entered: Arc::new(tokio::sync::Semaphore::new(0)),
        settled: Arc::default(),
    };
    let core = race_core(
        sqlite_memory_store_backend().await,
        &format!(
            "const winner = await Promise.race([tools.{SLOW}({{}}), tools.{FAST}({{}})]);\n\
             finish(winner);"
        ),
        Arc::new(tools.clone()),
    )?;
    let session = core
        .session(crate::SessionId::parse("closing-race").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let events = RecordingEvents::default();
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        session.send(TurnInput::text("race")).output_into(&events),
    )
    .await
    .expect("the turn answers, its loser stopped within the grace")?;
    assert!(
        matches!(
            &report.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ),
        "{:?}",
        report.outcome
    );
    let completions: Vec<_> = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|activity| match activity.event {
            crate::TurnEvent::ToolCallCompleted { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    let loser_completion = completions
        .iter()
        .find(|output| !output.is_success())
        .cloned()
        .unwrap_or_else(|| panic!("the loser's completion is recorded: {completions:?}"));
    drop(session);
    core.shutdown().await?;
    Ok((tools.settled.load(Ordering::SeqCst), loser_completion))
}

/// The loser's recorded cancellation, and its origin.
fn cancellation(output: &lash_core::ToolCallOutput) -> &lash_core::ToolCancellation {
    match &output.outcome {
        lash_core::ToolCallOutcome::Cancelled(cancellation) => cancellation,
        other => panic!("the loser is recorded cancelled: {other:?}"),
    }
}

/// L06: closing stops a loser cooperatively. Its body observes the stop
/// and finishes the work it owes after it; the turn waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l06_closing_lets_a_cooperative_loser_finish_its_own_work() -> Result<()> {
    let (settled, completion) = closing_race(Loser::Cooperative).await?;
    assert!(settled, "closing dropped the loser's body after its stop");
    cancellation(&completion);
    Ok(())
}

/// L06: the cooperative loser's record is its own cancellation, attributed
/// to the run's closing rather than to a turn that never stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5379: a closing race loser's record drops its own cancellation and its RunClosing origin"]
async fn l06_closing_lets_a_cooperative_loser_record_its_own_cancellation() -> Result<()> {
    let (_, completion) = closing_race(Loser::Cooperative).await?;
    let cancelled = cancellation(&completion);
    assert_eq!(cancelled.message, "A observed its stop", "{completion:?}");
    assert_eq!(
        cancelled.origin,
        Some(lash_core::CancelOrigin::RunClosing),
        "{completion:?}"
    );
    Ok(())
}

/// L06: a loser that never answers its stop is dropped after the bounded
/// grace, and its runtime cancellation names closing as its cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5379: a closing race loser's record drops its own cancellation and its RunClosing origin"]
async fn l06_closing_records_its_own_cause_for_a_loser_it_drops() -> Result<()> {
    let (settled, completion) = closing_race(Loser::Silent).await?;
    assert!(!settled);
    assert_eq!(
        cancellation(&completion).origin,
        Some(lash_core::CancelOrigin::RunClosing),
        "{completion:?}"
    );
    Ok(())
}

/// The losing call's retry backoff: far longer than closing may take.
const BACKOFF_MS: u64 = 10_000;

/// `slow`, `Repeatable`, fails transiently on its first attempt with a
/// retry after [`BACKOFF_MS`]; `fast` answers once that failure landed.
#[derive(Clone)]
struct BackoffTools {
    attempts: Arc<AtomicUsize>,
    failed: Arc<tokio::sync::Semaphore>,
}

impl Default for BackoffTools {
    fn default() -> Self {
        Self {
            attempts: Arc::default(),
            failed: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}

fn backoff_definition(name: &str) -> lash_core::ToolDefinition {
    let definition = definition(name);
    if name == SLOW {
        definition.with_execution_policy(lash_core::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
            BACKOFF_MS,
            BACKOFF_MS,
        ))
    } else {
        definition
    }
}

#[async_trait]
impl ToolProvider for BackoffTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [FAST, SLOW]
            .into_iter()
            .map(|name| backoff_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [FAST, SLOW]
            .contains(&name)
            .then(|| Arc::new(backoff_definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == FAST {
            let _failed = self.failed.acquire().await.expect("the gate stays open");
            return lash_core::ToolOutcome::ok(serde_json::json!(FAST)).into();
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.failed.add_permits(1);
        lash_core::ToolOutcome::failure_with_delay(
            lash_core::ToolFailureClass::External,
            "transient",
            "transient failure",
            Some(BACKOFF_MS),
        )
        .into()
    }
}

/// A turn whose cell races a `slow` loser in its retry backoff against
/// `fast` and finishes with the winner: how long it took to answer, the
/// loser's attempts, and its recorded completion, if any.
async fn closing_in_backoff() -> Result<(Duration, usize, Option<lash_core::ToolCallOutput>)> {
    let tools = BackoffTools::default();
    let core = race_core(
        sqlite_memory_store_backend().await,
        &format!(
            "const winner = await Promise.race([tools.{SLOW}({{}}), tools.{FAST}({{}})]);\n\
             finish(winner);"
        ),
        Arc::new(tools.clone()),
    )?;
    let session = core
        .session(crate::SessionId::parse("closing-backoff").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let events = RecordingEvents::default();
    let sent = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        session.send(TurnInput::text("race")).output_into(&events),
    )
    .await
    .expect("the turn answers")?;
    let answered = sent.elapsed();
    assert!(
        matches!(
            &report.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ),
        "{:?}",
        report.outcome
    );
    // Past any next attempt closing might wrongly start.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let loser = events
        .snapshot()
        .await
        .into_iter()
        .find_map(|activity| match activity.event {
            crate::TurnEvent::ToolCallCompleted { output, .. } if !output.is_success() => {
                Some(output)
            }
            _ => None,
        });
    drop(session);
    core.shutdown().await?;
    Ok((answered, tools.attempts.load(Ordering::SeqCst), loser))
}

/// L17: closing cuts a losing call's retry backoff at once; it never waits
/// out the backoff or starts the next attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l17_closing_cuts_a_loser_in_retry_backoff_promptly() -> Result<()> {
    let (answered, attempts, _) = closing_in_backoff().await?;
    assert!(
        answered < Duration::from_millis(BACKOFF_MS / 2),
        "closing waited out the loser's backoff: answered after {answered:?}"
    );
    assert_eq!(attempts, 1, "closing starts no next attempt");
    Ok(())
}

/// L17: closing decides the loser in its backoff cancelled, under its own
/// cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5379: a closing race loser's record drops its own cancellation and its RunClosing origin"]
async fn l17_closing_decides_a_loser_in_retry_backoff_cancelled() -> Result<()> {
    let (_, _, loser) = closing_in_backoff().await?;
    let loser = loser.expect("the loser's completion is recorded");
    assert_eq!(
        cancellation(&loser).origin,
        Some(lash_core::CancelOrigin::RunClosing),
        "{loser:?}"
    );
    Ok(())
}
