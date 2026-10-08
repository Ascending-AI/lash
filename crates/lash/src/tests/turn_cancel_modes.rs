//! The two turn-cancel modes on the durable turn path (FIG-635), through a
//! host's send and its cancel: `Immediate` aborts what runs; `AfterStep`
//! lands at the step boundary that closes a protocol iteration, letting the
//! step's tools finish uncancelled.

use super::*;
use crate::{TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::TurnCancelOutcome;
use std::sync::atomic::AtomicBool;

/// How long a cancelled turn may take to end.
const ENDS_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);
const WATCH: &str = "watch_tool";
const RETRY: &str = "retry_once";

/// A tool that reports whether it observed the cooperative token, and
/// either holds until released or waits for the token itself.
#[derive(Clone)]
struct TokenWatchingTool {
    executions: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Semaphore>,
    observed_cancelled: Arc<AtomicBool>,
    wait_for_token: bool,
}

impl Default for TokenWatchingTool {
    fn default() -> Self {
        Self {
            executions: Arc::default(),
            entered: Arc::default(),
            released: Arc::new(tokio::sync::Semaphore::new(0)),
            observed_cancelled: Arc::default(),
            wait_for_token: false,
        }
    }
}

impl TokenWatchingTool {
    fn release(&self) {
        self.released.add_permits(1);
    }
}

fn tool_definition(name: &str) -> lash_core::ToolDefinition {
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A cancel-mode probe.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name));
    if name == RETRY {
        definition.with_execution_policy(lash_core::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
            RETRY_AFTER_MS,
            RETRY_AFTER_MS,
        ))
    } else {
        definition
    }
}

#[async_trait]
impl ToolProvider for TokenWatchingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition(WATCH).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == WATCH).then(|| Arc::new(tool_definition(WATCH).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let token = call
            .context
            .cancellation_token()
            .cloned()
            .expect("an attempt carries the cooperative token");
        if self.wait_for_token {
            token.cancelled().await;
        } else {
            let _permit = self.released.acquire().await.expect("the gate stays open");
        }
        self.observed_cancelled
            .store(token.is_cancelled(), Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// The retry backoff, long enough for a cancel to land inside it.
const RETRY_AFTER_MS: u64 = 1_500;

/// A tool whose first attempt fails transiently with a retry after
/// [`RETRY_AFTER_MS`], and whose second answers.
#[derive(Clone, Default)]
struct RetryOnceTool {
    attempts: Arc<AtomicUsize>,
    failed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolProvider for RetryOnceTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition(RETRY).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == RETRY).then(|| Arc::new(tool_definition(RETRY).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            self.failed.notify_one();
            return lash_core::ToolOutcome::failure_with_delay(
                lash_core::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(RETRY_AFTER_MS),
            )
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// The text of the latest user message of `request`.
fn last_user(request: &lash_core::LlmRequest) -> String {
    last_user_text(request)
}

/// Whether `request` already carries a tool result.
fn has_tool_result(request: &lash_core::LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    })
}

/// A model that calls `tool` once for an input asking to use it, answers
/// `answered: <input>` otherwise, and records every request it was asked.
fn tool_calling_model(
    tool: &'static str,
    calls: &Arc<AtomicUsize>,
    seen: &Arc<StdMutex<Vec<String>>>,
) -> ProviderHandle {
    let calls = Arc::clone(calls);
    let seen = Arc::clone(seen);
    crate::testing::TestProvider::builder()
        .kind("cancel-modes")
        .complete(move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            seen.lock_recover().push(format!("{:?}", request.messages));
            let user = last_user(&request);
            let tool_round = user.contains("use the tool") && !has_tool_result(&request);
            async move {
                if tool_round {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "call-1".to_string(),
                            tool_name: tool.to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    });
                }
                Ok(text_response(&format!("answered: {user}")))
            }
        })
        .build()
        .into_handle()
}

/// A model whose first call holds until `release` opens and then calls
/// [`WATCH`], and whose later calls answer; `started` opens when the first
/// call starts.
fn gated_tool_calling_model(
    calls: &Arc<AtomicUsize>,
    seen: &Arc<StdMutex<Vec<String>>>,
    started: &Arc<tokio::sync::Notify>,
    release: &Arc<tokio::sync::Semaphore>,
) -> ProviderHandle {
    let (calls, seen) = (Arc::clone(calls), Arc::clone(seen));
    let (started, release) = (Arc::clone(started), Arc::clone(release));
    crate::testing::TestProvider::builder()
        .kind("cancel-modes-gated")
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            seen.lock_recover().push(format!("{:?}", request.messages));
            let (started, release) = (Arc::clone(&started), Arc::clone(&release));
            async move {
                if call > 0 {
                    return Ok(text_response("finished after the stop"));
                }
                started.notify_one();
                let _permit = release.acquire().await.expect("the gate stays open");
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "step-0-call".to_string(),
                        tool_name: WATCH.to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A session on a core serving `model` with `tools`.
async fn session_with(
    id: &str,
    model: ProviderHandle,
    tools: Arc<dyn ToolProvider>,
) -> Result<(LashCore, crate::DurableSession)> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(model, mock_llm_profile_spec())
    .tools(tools)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    Ok((core, session))
}

/// The open run's cancel outcome a receipt answers.
fn outcome(receipt: crate::CancelReceipt) -> TurnCancelOutcome {
    match receipt {
        crate::CancelReceipt::Cancelled { receipt, .. } => receipt.outcome,
        other => panic!("the cancel addressed the open run: {other:?}"),
    }
}

/// The ended turn's cancellation evidence.
async fn cancelled(
    handle: crate::SendHandle,
) -> Result<lash_core::facade_support::TurnCancellationEvidence> {
    let output = tokio::time::timeout(ENDS_WITHIN, handle.output())
        .await
        .expect("the cancelled turn ends")?;
    Ok(output
        .result
        .cancellation()
        .cloned()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.result.outcome)))
}

/// The step an after-step stop let finish stays in the session: the stop
/// lands while the model call streams, the call's tool runs, and the next
/// turn's prompt carries that tool call and its result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5373: a cancelled durable turn never commits its checkpointed steps to the head"]
async fn an_after_step_stop_keeps_its_completed_step_in_the_session() -> Result<()> {
    let tool = TokenWatchingTool::default();
    tool.release();
    let (calls, seen) = (Arc::default(), Arc::<StdMutex<Vec<String>>>::default());
    let (started, release) = (Arc::default(), Arc::new(tokio::sync::Semaphore::new(0)));
    let (core, session) = session_with(
        "after-step-keeps-step",
        gated_tool_calling_model(&calls, &seen, &started, &release),
        Arc::new(tool.clone()),
    )
    .await?;
    let handle = session
        .send(TurnInput::text("stop after this step"))
        .await?;
    started.notified().await;
    let requested = outcome(handle.cancel().mode(TurnCancelMode::AfterStep).await?);
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    release.add_permits(1);
    cancelled(handle).await?;
    tokio::time::timeout(ENDS_WITHIN, session.send(TurnInput::text("next")).output())
        .await
        .expect("the next turn answers")?;
    let next_prompt = seen
        .lock_recover()
        .last()
        .cloned()
        .expect("the next turn called the model");
    assert!(
        next_prompt.contains("step-0-call") && next_prompt.contains("ToolResult"),
        "the completed tool result is part of the stopped turn, not backtracked: {next_prompt}"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// An after-step stop requested while a tool runs, on session `id`: the
/// ended turn's evidence, the tool and the model's call count.
async fn after_step_stop_mid_tool(
    id: &str,
) -> Result<(
    lash_core::facade_support::TurnCancellationEvidence,
    TokenWatchingTool,
    Arc<AtomicUsize>,
)> {
    let tool = TokenWatchingTool::default();
    let (calls, seen) = (Arc::default(), Arc::default());
    let (core, session) = session_with(
        id,
        tool_calling_model(WATCH, &calls, &seen),
        Arc::new(tool.clone()),
    )
    .await?;
    let handle = session.send(TurnInput::text("use the tool")).await?;
    tool.entered.notified().await;
    let requested = outcome(
        handle
            .cancel()
            .request_id("stop-mid-tool")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !tool.observed_cancelled.load(Ordering::SeqCst),
        "the running tool's token stays quiet"
    );
    tool.release();
    let evidence = cancelled(handle).await?;
    core.shutdown().await?;
    Ok((evidence, tool, calls))
}

/// An after-step stop requested while a tool runs lets the tool finish
/// without signalling its token, and lands at the step boundary: the turn
/// ends cancelled with after-step evidence, and the model is not called
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_step_stop_mid_tool_call_lets_the_tool_finish_uncancelled() -> Result<()> {
    let (evidence, tool, calls) = after_step_stop_mid_tool("after-step-mid-tool").await?;
    assert_eq!(evidence.request_id, "stop-mid-tool");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
    assert!(!tool.observed_cancelled.load(Ordering::SeqCst));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no model call after the stop"
    );
    Ok(())
}

/// An after-step stop's evidence names the step it was honoured after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5364: the durable cancel evidence never records honoured_after_step"]
async fn an_after_step_stop_records_the_step_it_was_honoured_at() -> Result<()> {
    let (evidence, _, _) = after_step_stop_mid_tool("after-step-honoured").await?;
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0), "{evidence:?}");
    Ok(())
}

/// A repeated after-step stop names the first request, and an immediate one
/// after it escalates: the running tool is aborted, the turn ends with the
/// escalating request's immediate evidence, and a request after the end
/// opens nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn immediate_after_after_step_escalates_and_aborts_the_running_tool() -> Result<()> {
    let tool = TokenWatchingTool {
        wait_for_token: true,
        ..TokenWatchingTool::default()
    };
    let (calls, seen) = (Arc::default(), Arc::default());
    let (core, session) = session_with(
        "escalate-to-abort",
        tool_calling_model(WATCH, &calls, &seen),
        Arc::new(tool.clone()),
    )
    .await?;
    let run = crate::TurnId::parse("escalate-to-abort-run").expect("nonblank host identity");
    let handle = session
        .send(TurnInput::text("use the tool"))
        .id(run.clone())
        .await?;
    tool.entered.notified().await;
    let stop = outcome(
        handle
            .cancel()
            .request_id("stop-first")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(matches!(stop, TurnCancelOutcome::Requested(_)), "{stop:?}");
    let again = outcome(
        handle
            .cancel()
            .request_id("stop-again")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(&again, TurnCancelOutcome::AlreadyRequested(evidence) if evidence.request_id == "stop-first"),
        "{again:?}"
    );
    let abort = outcome(
        handle
            .cancel()
            .request_id("abort-now")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(&abort, TurnCancelOutcome::Escalated(evidence)
            if evidence.request_id == "abort-now" && evidence.mode == TurnCancelMode::Immediate),
        "{abort:?}"
    );
    let evidence = cancelled(handle).await?;
    assert_eq!(evidence.request_id, "abort-now");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let repeat = session
        .cancel(crate::CancelTarget::Run(run))
        .request_id("abort-late")
        .mode(TurnCancelMode::Immediate)
        .await?;
    assert!(
        !matches!(&repeat, crate::CancelReceipt::Cancelled { receipt, .. }
            if matches!(receipt.outcome, TurnCancelOutcome::Requested(_))),
        "a finished turn accepts no fresh request: {repeat:?}"
    );
    core.shutdown().await?;
    Ok(())
}

/// A stop in either mode never drains the next-turn work queued behind it:
/// an input queued while the stopped turn runs is answered by a turn of its
/// own afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_in_either_mode_never_drains_next_turn_work_queued_behind_it() -> Result<()> {
    for mode in [TurnCancelMode::AfterStep, TurnCancelMode::Immediate] {
        let tool = TokenWatchingTool {
            wait_for_token: mode.is_immediate(),
            ..TokenWatchingTool::default()
        };
        let (calls, seen) = (Arc::default(), Arc::default());
        let (core, session) = session_with(
            &format!("cancel-no-drain-{mode:?}").to_ascii_lowercase(),
            tool_calling_model(WATCH, &calls, &seen),
            Arc::new(tool.clone()),
        )
        .await?;
        let handle = session.send(TurnInput::text("use the tool")).await?;
        tool.entered.notified().await;
        let queued = session.send(TurnInput::text("queued behind")).await?;
        let requested = outcome(handle.cancel().mode(mode).await?);
        assert!(
            matches!(requested, TurnCancelOutcome::Requested(_)),
            "{mode:?}: {requested:?}"
        );
        tool.release();
        let evidence = cancelled(handle).await?;
        assert_eq!(evidence.mode, mode);
        if !mode.is_immediate() {
            assert!(
                !tool.observed_cancelled.load(Ordering::SeqCst),
                "an after-step stop never signals the tool's token"
            );
        }
        let output = tokio::time::timeout(ENDS_WITHIN, queued.output())
            .await
            .expect("the queued input runs")?;
        assert_eq!(
            output.assistant_message(),
            Some("answered: queued behind"),
            "{mode:?}: the queued input is answered by its own turn"
        );
        core.shutdown().await?;
    }
    Ok(())
}

/// What a disposition law witnesses of the cancelled turn's undelivered
/// input.
#[derive(Clone, Copy)]
enum Witness {
    /// What becomes of the input: kept for a later turn, or never delivered.
    Settlement,
    /// The cancelled run's report names the input and its disposition.
    Report,
}

/// An undelivered active-turn input settled by a cancel with
/// `disposition`, in either mode: `Defer` keeps it, and a later turn answers
/// it; `Drop` removes it, and no model request ever carries it. The turn's
/// evidence names the disposition.
async fn undelivered_input_settles_by(
    disposition: TurnCancelUndeliveredInputPolicy,
    witness: Witness,
) -> Result<()> {
    for mode in [TurnCancelMode::Immediate, TurnCancelMode::AfterStep] {
        let tool = TokenWatchingTool {
            wait_for_token: mode.is_immediate(),
            ..TokenWatchingTool::default()
        };
        let (calls, seen) = (Arc::default(), Arc::<StdMutex<Vec<String>>>::default());
        let name = format!("cancel-matrix-{mode:?}-{disposition:?}-{}", witness as u8)
            .to_ascii_lowercase();
        let (core, session) = session_with(
            &name,
            tool_calling_model(WATCH, &calls, &seen),
            Arc::new(tool.clone()),
        )
        .await?;
        let run = crate::TurnId::parse(format!("{name}-run")).expect("nonblank host identity");
        let handle = session
            .send(TurnInput::text("use the tool"))
            .id(run.clone())
            .await?;
        tool.entered.notified().await;
        // The steer is sent while the tool runs, and the cancel lands
        // before any checkpoint delivers it.
        let steer = session
            .send(TurnInput::text("unsent steer"))
            .ingress(crate::persistence::TurnInputIngress::active_turn(
                run,
                Default::default(),
            ))
            .await?;
        let requested = outcome(handle.cancel().mode(mode).undelivered(disposition).await?);
        assert!(
            matches!(requested, TurnCancelOutcome::Requested(_)),
            "{mode:?}/{disposition:?}: {requested:?}"
        );
        tool.release();
        let output = tokio::time::timeout(ENDS_WITHIN, handle.output())
            .await
            .expect("the cancelled turn ends")?;
        let evidence = output
            .result
            .cancellation()
            .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.result.outcome));
        assert_eq!(evidence.mode, mode);
        assert_eq!(
            evidence.undelivered, disposition,
            "{mode:?}/{disposition:?}"
        );
        match (witness, disposition) {
            (Witness::Report, _) => {
                let affected: Vec<_> = output
                    .result
                    .cancel_input_outcome
                    .affected_inputs
                    .iter()
                    .map(|affected| (affected.input_id.clone(), affected.disposition))
                    .collect();
                assert_eq!(
                    affected,
                    vec![(steer.input_id().clone(), disposition)],
                    "{mode:?}/{disposition:?}: the disposition applies to the undelivered active-turn input"
                );
            }
            (Witness::Settlement, TurnCancelUndeliveredInputPolicy::Defer) => {
                let output = tokio::time::timeout(ENDS_WITHIN, steer.output())
                    .await
                    .expect("the deferred input runs")?;
                assert_eq!(
                    output.assistant_message(),
                    Some("answered: unsent steer"),
                    "{mode:?}: Defer keeps the input for a later turn"
                );
            }
            (Witness::Settlement, TurnCancelUndeliveredInputPolicy::Drop) => {
                let ended = tokio::time::timeout(ENDS_WITHIN, steer.outcome())
                    .await
                    .expect("the dropped input's handle answers");
                assert!(
                    !matches!(&ended, Ok(outcome) if outcome.status() == crate::TurnStatus::Answered),
                    "{mode:?}: Drop never answers the input: {ended:?}"
                );
                assert!(
                    !seen
                        .lock_recover()
                        .iter()
                        .any(|request| request.contains("unsent steer")),
                    "{mode:?}: no model request carries the dropped input"
                );
            }
        }
        core.shutdown().await?;
    }
    Ok(())
}

/// A `Defer` cancel keeps the turn's undelivered input for a later turn, in
/// either mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_defer_cancel_keeps_the_turn_s_undelivered_input_for_a_later_turn() -> Result<()> {
    undelivered_input_settles_by(TurnCancelUndeliveredInputPolicy::Defer, Witness::Settlement).await
}

/// A `Drop` cancel never delivers the turn's undelivered input, in either
/// mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5365: the durable path never applies a Drop cancel's disposition"]
async fn a_drop_cancel_never_delivers_the_turn_s_undelivered_input() -> Result<()> {
    undelivered_input_settles_by(TurnCancelUndeliveredInputPolicy::Drop, Witness::Settlement).await
}

/// A cancelled run's report names the undelivered active-turn input its
/// disposition applied to, for either disposition in either mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5321: a durable cancelled run's report names no affected input"]
async fn a_cancelled_run_reports_the_undelivered_input_its_disposition_applied_to() -> Result<()> {
    for disposition in [
        TurnCancelUndeliveredInputPolicy::Defer,
        TurnCancelUndeliveredInputPolicy::Drop,
    ] {
        undelivered_input_settles_by(disposition, Witness::Report).await?;
    }
    Ok(())
}

/// Prose the first step streams and checkpoints.
const CHECKPOINTED: &str = "committed before the tool";
/// Prose a failed, retried attempt streamed.
const RETRACTED: &str = "discarded transient attempt";
/// Prose the stopped attempt streamed past the checkpoint.
const TAIL: &str = "uncommitted streamed tail";

/// An immediate stop after one checkpointed step: the host's activity of
/// the stopped turn, and the text blocks of the next turn's prompt.
struct ImmediateTail {
    evidence: lash_core::facade_support::TurnCancellationEvidence,
    host: Vec<crate::TurnActivity>,
    next: crate::TurnOutput,
    next_prompt: Vec<String>,
}

/// Run [`ImmediateTail`] on session `id`. The model's first call streams
/// [`CHECKPOINTED`] and calls a tool; its second streams [`RETRACTED`] and
/// fails transiently; its third, the retry, streams [`TAIL`] and holds until
/// the stop; its fourth answers the next turn.
async fn immediate_stop_after_a_checkpoint(id: &str) -> Result<ImmediateTail> {
    let requests = Arc::new(StdMutex::new(
        Vec::<Vec<lash_core::llm::types::LlmMessage>>::new(),
    ));
    let provider = {
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("checkpoint-tail")
            .requires_streaming(true)
            .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
            .options(lash_core::facade_support::ProviderOptions {
                reliability: lash_core::provider::ProviderReliability::default()
                    .max_attempts(2)
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..lash_core::facade_support::ProviderOptions::default()
            })
            .complete(move |request| {
                requests.lock_recover().push(request.messages.clone());
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    let delta = |block: &str, text: &str| LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new(block, 0),
                        text: text.to_string(),
                    };
                    match call {
                        0 => Ok(LlmResponse {
                            parts: vec![
                                LlmOutputPart::Text {
                                    text: CHECKPOINTED.to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::ToolCall {
                                    call_id: "checkpoint-tool".to_string(),
                                    tool_name: WATCH.to_string(),
                                    input_json: "{}".to_string(),
                                    replay: None,
                                },
                            ],
                            ..LlmResponse::default()
                        }),
                        1 => {
                            request
                                .stream_events
                                .expect("stream sender")
                                .send(delta("failed-block", RETRACTED));
                            Err(crate::provider::LlmTransportError::new("transient provider failure")
                                .with_kind(lash_core::ProviderFailureKind::Stream)
                                .with_retry_verdict(
                                    lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                                ))
                        }
                        2 => {
                            request
                                .stream_events
                                .expect("stream sender")
                                .send(delta("tail-block", TAIL));
                            std::future::pending().await
                        }
                        3 => Ok(text_response("next turn answered")),
                        other => panic!("unexpected model call {other}"),
                    }
                }
            })
            .build()
            .into_handle()
    };
    let tool = TokenWatchingTool::default();
    tool.release();
    let (core, session) = session_with(id, provider, Arc::new(tool)).await?;
    let handle = session.send(TurnInput::text("first turn")).await?;
    let mut events = handle.events();
    let mut host = Vec::new();
    tokio::time::timeout(ENDS_WITHIN, async {
        while let Some(activity) = events.next_activity().await {
            let activity = activity?;
            let tail = matches!(&activity.event,
                TurnEvent::AssistantProseDelta { text, .. } if &**text == TAIL);
            host.push(activity);
            if tail {
                return Result::Ok(());
            }
        }
        panic!("the run ended before its tail streamed: {host:?}")
    })
    .await
    .expect("the host sees the streamed tail")?;
    let requested = outcome(
        handle
            .cancel()
            .request_id("stop-after-checkpoint")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    let evidence = cancelled(handle).await?;
    while let Some(activity) = tokio::time::timeout(ENDS_WITHIN, events.next_activity())
        .await
        .expect("the run's activity ends with the run")
    {
        host.push(activity?);
    }
    let next = tokio::time::timeout(
        ENDS_WITHIN,
        session.send(TurnInput::text("next turn")).output(),
    )
    .await
    .expect("the next turn answers")?;
    let next_prompt = requests
        .lock_recover()
        .get(3)
        .expect("the next turn reaches the model")
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect();
    drop(session);
    core.shutdown().await?;
    Ok(ImmediateTail {
        evidence,
        host,
        next,
        next_prompt,
    })
}

/// An immediate stop while the model streams past a checkpoint: the host
/// saw the checkpointed prose before the checkpoint, the failed attempt's
/// prose retracted by its attempt reset, and the stopped attempt's tail;
/// neither the tail nor the retracted prose is part of the next turn's
/// prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn immediate_stop_tail_after_checkpoint_is_absent_from_next_turn_context() -> Result<()> {
    let ImmediateTail {
        evidence,
        host,
        next,
        next_prompt,
    } = immediate_stop_after_a_checkpoint("immediate-tail").await?;
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    let prose = |activity: &crate::TurnActivity, wanted: &str| matches!(&activity.event, TurnEvent::AssistantProseDelta { text, .. } if &**text == wanted);
    let checkpoint = host
        .iter()
        .rposition(|activity| matches!(activity.event, TurnEvent::CheckpointRecorded { .. }))
        .unwrap_or_else(|| panic!("the first iteration records a checkpoint: {host:?}"));
    assert!(
        host[..checkpoint]
            .iter()
            .any(|activity| prose(activity, CHECKPOINTED)),
        "the checkpointed prose streams before its checkpoint: {host:?}"
    );
    let retracted = host[checkpoint + 1..]
        .iter()
        .find(|activity| prose(activity, RETRACTED))
        .unwrap_or_else(|| panic!("the failed attempt's delta reached the host: {host:?}"));
    let reset = host[checkpoint + 1..]
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => Some(assistant_prose_correlation_ids),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the retry retracts the failed attempt: {host:?}"));
    assert!(reset.contains(&retracted.correlation_id));
    let tail: String = host[checkpoint + 1..]
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. }
                if !reset.contains(&activity.correlation_id) =>
            {
                Some(text.as_ref())
            }
            _ => None,
        })
        .collect();
    assert_eq!(tail, TAIL);
    assert_eq!(next.assistant_message(), Some("next turn answered"));
    assert!(
        !next_prompt.iter().any(|text| text.contains(TAIL)),
        "the streamed tail is absent from the next prompt: {next_prompt:?}"
    );
    assert!(
        !next_prompt.iter().any(|text| text.contains(RETRACTED)),
        "the retracted attempt is absent from the next prompt: {next_prompt:?}"
    );
    Ok(())
}

/// The prose an immediate stop's turn checkpointed before the stop is part
/// of the next turn's prompt: only the tail past the checkpoint is
/// discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5373: a cancelled durable turn never commits its checkpointed steps to the head"]
async fn an_immediate_stop_keeps_the_prose_checkpointed_before_it() -> Result<()> {
    let tail = immediate_stop_after_a_checkpoint("immediate-keeps-checkpoint").await?;
    assert!(
        tail.next_prompt
            .iter()
            .any(|text| text.contains(CHECKPOINTED)),
        "the checkpointed prose is in the next prompt: {:?}",
        tail.next_prompt
    );
    Ok(())
}

/// An after-step stop requested while a `Repeatable` call waits out its
/// retry backoff does not cut the backoff short: the retry runs at its due
/// time, the step finishes, and the stop lands at its boundary with no
/// further model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_step_stop_during_retry_sleep_lands_at_wake_and_stops_at_the_boundary() -> Result<()>
{
    let tool = RetryOnceTool::default();
    let (calls, seen) = (Arc::default(), Arc::default());
    let (core, session) = session_with(
        "after-step-during-sleep",
        tool_calling_model(RETRY, &calls, &seen),
        Arc::new(tool.clone()),
    )
    .await?;
    let handle = session.send(TurnInput::text("use the tool")).await?;
    tool.failed.notified().await;
    let requested_at = std::time::Instant::now();
    let requested = outcome(
        handle
            .cancel()
            .request_id("stop-in-sleep")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    assert_eq!(tool.attempts.load(Ordering::SeqCst), 1, "the backoff holds");
    let evidence = cancelled(handle).await?;
    assert_eq!(evidence.request_id, "stop-in-sleep");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        2,
        "the retry runs at its due time; the step finishes before the stop lands"
    );
    assert!(
        requested_at.elapsed() >= std::time::Duration::from_millis(RETRY_AFTER_MS / 2),
        "an after-step stop does not wake the backoff early"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no model call after the stop"
    );
    core.shutdown().await?;
    Ok(())
}

/// A `Drop` request recorded before its turn's owner is lost holds across
/// the loss: the next deployment over the same stores ends the turn
/// cancelled with the request's evidence, and the undelivered input
/// addressed to it is never delivered to any later turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5365: the durable path never applies a Drop cancel's disposition"]
async fn drop_request_survives_owner_failure_before_finish_and_prevents_redelivery() -> Result<()> {
    const ID: &str = "drop-cancel-owner-failure";
    let backend = sqlite_memory_store_backend().await;
    let (calls, seen) = (Arc::default(), Arc::<StdMutex<Vec<String>>>::default());
    let deploy = |tool: TokenWatchingTool| -> Result<LashCore> {
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(
                tool_calling_model(WATCH, &calls, &seen),
                mock_llm_profile_spec(),
            )
            .tools(Arc::new(tool))
            .build(crate::testing::runtime_lease_owner())
    };
    // The first owner's tool never finishes: the owner is lost with the
    // request recorded and the turn unfinished.
    let tool = TokenWatchingTool::default();
    let core = deploy(tool.clone())?;
    let session_id = crate::SessionId::parse(ID).expect("nonblank host identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let run = crate::TurnId::parse("turn-that-cannot-finish").expect("nonblank host identity");
    let handle = session
        .send(TurnInput::text("use the tool"))
        .id(run.clone())
        .await?;
    tool.entered.notified().await;
    session
        .send(TurnInput::text("must be dropped after owner failure"))
        .ingress(crate::persistence::TurnInputIngress::active_turn(
            run.clone(),
            Default::default(),
        ))
        .await?;
    let requested = outcome(
        handle
            .cancel()
            .request_id("drop-before-owner-failure")
            .origin("test-user")
            .reason("drop undelivered input")
            .mode(TurnCancelMode::AfterStep)
            .undelivered(TurnCancelUndeliveredInputPolicy::Drop)
            .await?,
    );
    assert!(
        matches!(&requested, TurnCancelOutcome::Requested(evidence)
            if evidence.undelivered == TurnCancelUndeliveredInputPolicy::Drop),
        "{requested:?}"
    );
    drop((handle, session));
    core.shutdown().await?;

    let released = TokenWatchingTool::default();
    released.release();
    let core = deploy(released)?;
    let session = core.session(session_id).open().await?;
    let evidence = cancelled(session.attach_id(run)).await?;
    assert_eq!(evidence.request_id, "drop-before-owner-failure");
    assert_eq!(evidence.undelivered, TurnCancelUndeliveredInputPolicy::Drop);
    let next = tokio::time::timeout(ENDS_WITHIN, session.send(TurnInput::text("next")).output())
        .await
        .expect("the next turn answers")?;
    assert_eq!(next.assistant_message(), Some("answered: next"));
    assert!(
        !seen
            .lock_recover()
            .iter()
            .any(|request| request.contains("must be dropped")),
        "Drop keeps the undelivered input out of every later admission"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// L03: the durable cancel request, not a live owner's flag, decides a
/// retry after the owner dies with the call's backoff still pending. The
/// owner is lost while the `Repeatable` call waits out its backoff, the host
/// then cancels the turn, and the next deployment over the same stores ends
/// it cancelled: no second attempt runs, even past the retry's due time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_cancel_survives_its_owners_loss_in_a_retry_backoff_without_a_second_body()
-> Result<()> {
    const ID: &str = "cancel-across-owner-loss-in-backoff";
    let backend = sqlite_memory_store_backend().await;
    let tool = RetryOnceTool::default();
    let (calls, seen) = (Arc::default(), Arc::<StdMutex<Vec<String>>>::default());
    let deploy = || -> Result<LashCore> {
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(
                tool_calling_model(RETRY, &calls, &seen),
                mock_llm_profile_spec(),
            )
            .tools(Arc::new(tool.clone()))
            .build(crate::testing::runtime_lease_owner())
    };
    let core = deploy()?;
    let session_id = crate::SessionId::parse(ID).expect("nonblank host identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let run = crate::TurnId::parse("turn-in-backoff").expect("nonblank host identity");
    let handle = session
        .send(TurnInput::text("use the tool"))
        .id(run.clone())
        .await?;
    tool.failed.notified().await;
    // The owner dies with the call's backoff pending.
    core.shutdown().await?;
    let requested = outcome(
        handle
            .cancel()
            .request_id("abort-after-owner-loss")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    drop((handle, session));

    let core = deploy()?;
    let session = core.session(session_id).open().await?;
    let evidence = cancelled(session.attach_id(run)).await?;
    assert_eq!(evidence.request_id, "abort-after-owner-loss");
    tokio::time::sleep(std::time::Duration::from_millis(RETRY_AFTER_MS * 2)).await;
    assert_eq!(
        tool.attempts.load(Ordering::SeqCst),
        1,
        "no second body runs after the accepted cancel"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no model call after it");
    drop(session);
    core.shutdown().await?;
    Ok(())
}
