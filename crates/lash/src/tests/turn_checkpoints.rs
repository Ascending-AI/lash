//! Turn and checkpoint laws on the durable engine: a facade core over SQLite
//! memory stores runs each sent turn, and the law reads its settled report,
//! its committed state and what its model was asked.

use super::*;
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use lash_core::{FailureCode, MessageRole};

/// One scripted model answer: a response or a transport failure.
type Answer = std::result::Result<LlmResponse, LlmTransportError>;

/// A model whose `n`th request (from 1) answers `answers(n)`; every request
/// is recorded.
fn scripted(
    requests: Arc<StdMutex<Vec<LlmRequest>>>,
    answers: Arc<dyn Fn(usize) -> Answer + Send + Sync>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("turn-checkpoints")
        .complete(move |request: LlmRequest| {
            let mut seen = requests.lock_recover();
            seen.push(request);
            let answer = answers(seen.len());
            async move { answer }
        })
        .build()
        .into_handle()
}

/// A standard core over `backend` running `model`, with `tools`.
fn core_with(
    backend: lash_core::Backend,
    model: ProviderHandle,
    tools: Option<Arc<dyn ToolProvider>>,
) -> LashCore {
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(model, mock_llm_profile_spec());
    match tools {
        Some(tools) => builder.tools(tools),
        None => builder,
    }
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core")
}

async fn created(core: &LashCore, id: &'static str) -> crate::DurableSession {
    core.session(crate::SessionId::from(id))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created")
}

/// The text of every user block `request` carries.
fn request_text(request: &LlmRequest) -> String {
    format!("{:?}", request.messages)
}

/// A turn commits its input's text verbatim as its user message, and the
/// message names the turn and the accepted input it came from (FIG-972).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_turn_stores_effective_user_text_in_state() {
    const INPUT: &str = "/yolopush\n\n<skill>\nbody\n</skill>";
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = core_with(
        sqlite_memory_store_backend().await,
        scripted(
            Arc::clone(&requests),
            Arc::new(|_| Ok(text_response("Done"))),
        ),
        None,
    );
    let session = created(&core, "skill-command-visibility").await;
    let turn = crate::TurnId::from("skill-command-visibility-turn");
    let handle = session
        .send(crate::TurnInput::text(INPUT))
        .id(turn.clone())
        .await
        .expect("accepted");
    let input_id = handle.input_id().clone();
    let output = handle.output().await.expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let read_view = output.result.state.read_view();
    let user_message = read_view
        .messages()
        .iter()
        .find(|message| message.role == MessageRole::User)
        .expect("the user message is committed");
    assert_eq!(
        user_message
            .parts
            .first()
            .map(|part| part.content())
            .as_deref(),
        Some(INPUT)
    );
    assert_eq!(
        user_message.origin,
        Some(lash_core::MessageOrigin::TurnInput {
            turn_id: turn,
            input_id: Some(input_id),
        })
    );
    core.shutdown().await.expect("shutdown");
}

/// A turn sent through an open session answers the committed head: its
/// report's state and the session's own reads carry the turn it ran
/// (FIG-5346).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_sessions_report_and_reads_reach_the_committed_head() {
    let core = core_with(
        sqlite_memory_store_backend().await,
        scripted(
            Arc::new(StdMutex::new(Vec::new())),
            Arc::new(|_| Ok(text_response("Done"))),
        ),
        None,
    );
    created(&core, "live-committed-head").await;
    let live = core
        .session(crate::SessionId::from("live-committed-head"))
        .open()
        .await
        .expect("opened");
    let output = live
        .send(crate::TurnInput::text("hi"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let committed =
        |state: &lash_core::SessionSnapshot| (state.turn_index, state.read_view().messages().len());
    assert_eq!(committed(&output.result.state), (1, 2), "the report");
    let read_view = live.observe().read_view();
    assert_eq!(read_view.messages().len(), 2, "the session's reads");
    assert_eq!(live.read_view().messages().len(), 2, "the session's reads");
    core.shutdown().await.expect("shutdown");
}

fn transient_500() -> Answer {
    Err(LlmTransportError::new("provider unavailable")
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
        .with_code(FailureCode::provider("http_500")))
}

/// A model call whose every attempt fails transiently retries until its
/// retry budget is spent, then fails the turn: the turn's call ledger holds
/// one logical call with every attempt on it, each a typed provider failure,
/// each but the last scheduled for retry and the last declined as exhausted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_llm_failures_exhaust_and_fail_turn() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = core_with(
        sqlite_memory_store_backend().await,
        scripted(Arc::clone(&requests), Arc::new(|_| transient_500())),
        None,
    );
    let session = created(&core, "retryable-error").await;
    let output = session
        .send(crate::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn settles");
    assert!(
        matches!(
            output.result.outcome,
            crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError)
        ),
        "{:?}",
        output.result.outcome
    );
    let [call] = output.result.llm_calls.as_slice() else {
        panic!("one logical model call: {:?}", output.result.llm_calls);
    };
    let attempts = &call.attempts;
    assert!(
        attempts.len() > 1,
        "a retryable failure retries: {attempts:?}"
    );
    assert_eq!(
        requests.lock_recover().len(),
        attempts.len(),
        "each attempt is one provider request"
    );
    for (index, attempt) in attempts.iter().enumerate() {
        let error = attempt.error.as_ref().expect("a failed attempt's error");
        assert_eq!(error.code, Some(FailureCode::provider("http_500")));
        let decision = attempt.retry_decision.as_ref().expect("a retry decision");
        if index + 1 < attempts.len() {
            assert!(decision.is_scheduled(), "attempt {index}: {decision:?}");
        } else {
            assert_eq!(
                decision,
                &lash_core::RetryDecision::Declined(
                    lash_core::RetryDeclineCause::RetryBudgetExhausted
                )
            );
        }
    }
    core.shutdown().await.expect("shutdown");
}

/// A non-retryable provider failure fails the turn on its first attempt,
/// and the turn's call ledger carries the failure's typed kind, code and
/// status with the attempt's typed refusal to retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_surfaces_typed_kind_and_retryability_on_turn_issue() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = core_with(
        sqlite_memory_store_backend().await,
        scripted(
            Arc::clone(&requests),
            Arc::new(|_| {
                Err(LlmTransportError::new("bad request")
                    .with_http_status(400)
                    .with_code(FailureCode::provider("400")))
            }),
        ),
        None,
    );
    let session = created(&core, "typed-provider-failure").await;
    let output = session
        .send(crate::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn settles");
    assert!(matches!(
        output.result.outcome,
        crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError)
    ));
    let [call] = output.result.llm_calls.as_slice() else {
        panic!("one logical model call: {:?}", output.result.llm_calls);
    };
    let [attempt] = call.attempts.as_slice() else {
        panic!("one attempt: {:?}", call.attempts);
    };
    let error = attempt.error.as_ref().expect("the attempt's typed error");
    assert_eq!(error.class, lash_core::ProviderFailureKind::Validation);
    assert_eq!(error.code, Some(FailureCode::provider("400")));
    assert_eq!(error.http_status, Some(400));
    assert_eq!(
        attempt.retry_decision,
        Some(lash_core::RetryDecision::Declined(
            lash_core::RetryDeclineCause::NotRetryable
        ))
    );
    assert_eq!(requests.lock_recover().len(), 1);
    core.shutdown().await.expect("shutdown");
}

/// The tool whose body sends what its law sends to the running turn.
const SEND_TOOL: &str = "send_to_turn";

fn send_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{SEND_TOOL}"),
        SEND_TOOL,
        "Sends the law's input to the running turn.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// What the tool body sends while the turn's round runs.
type Sender =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// [`SEND_TOOL`]: its body runs the law's sender once, then answers.
struct SendTool(Sender);

#[async_trait]
impl ToolProvider for SendTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![send_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SEND_TOOL).then(|| Arc::new(send_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (self.0)().await;
        lash_core::ToolOutcome::ok(serde_json::json!("sent")).into()
    }
}

/// The first request calls [`SEND_TOOL`]; every later one answers.
fn send_then_answer(n: usize) -> Answer {
    if n == 1 {
        return Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "send-call".to_string(),
                tool_name: SEND_TOOL.to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            ..LlmResponse::default()
        });
    }
    Ok(text_response(&format!("answer {n}")))
}

/// Send `input` to `session`'s running turn `run`, delivered at its next
/// work checkpoint.
async fn steer(
    core: &LashCore,
    session: &'static str,
    run: &crate::TurnId,
    input: crate::TurnInput,
) {
    core.session(crate::SessionId::from(session))
        .durable()
        .await
        .expect("the session's durable handle")
        .send(input)
        .ingress(lash_core::TurnInputIngress::active_turn(
            run.clone(),
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await
        .expect("the running turn accepts its steer");
}

/// The core a law's tool body reaches: built after the tool that holds it.
type LateCore = Arc<std::sync::OnceLock<LashCore>>;

/// A steer that carries an image, delivered at a work checkpoint, reaches
/// the model's next request with its attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_checkpoint_input_preserves_images() {
    const SESSION: &str = "image-attachment";
    let run = crate::TurnId::from("image-attachment-turn");
    let late: LateCore = Arc::default();
    let sender: Sender = {
        let late = Arc::clone(&late);
        let run = run.clone();
        Arc::new(move || {
            let late = Arc::clone(&late);
            let run = run.clone();
            Box::pin(async move {
                let core = late.get().expect("the core is built");
                steer(
                    core,
                    SESSION,
                    &run,
                    crate::TurnInput::text("see image").with_attachment(
                        lash_core::AttachmentSource::inline(
                            lash_core::MediaType::parse("image/png").expect("media type"),
                            vec![1, 2, 3],
                        ),
                    ),
                )
                .await;
            })
        })
    };
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = core_with(
        sqlite_memory_store_backend().await,
        scripted(Arc::clone(&requests), Arc::new(send_then_answer)),
        Some(Arc::new(SendTool(sender))),
    );
    assert!(late.set(core).is_ok());
    let core = late.get().expect("the core");
    let session = core
        .session(crate::SessionId::from(SESSION))
        .create(crate::SessionCreation::root(
            mock_session_spec()
                .attachment_acceptance(lash_core::attachments::attachment_test_acceptance()),
        ))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
        .id(run)
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2, "the steer joined the running turn");
    assert!(requests[1].messages.iter().any(|message| {
        message.role == lash_core::llm::types::LlmRole::User
            && message.blocks.iter().any(|block| {
                matches!(
                    block,
                    lash_core::llm::types::LlmContentBlock::Attachment { .. }
                )
            })
    }));
    core.shutdown().await.expect("shutdown");
}

/// A plugin whose checkpoint observer fails every checkpoint.
fn failing_checkpoint_observer() -> Arc<dyn PluginFactory> {
    let hook: lash_core::plugin::CheckpointHook = Arc::new(|_| {
        Box::pin(async {
            Err(lash_core::PluginError::Session(
                "reject checkpoint delivery".to_string(),
            ))
        })
    });
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("failing-checkpoint-observer"),
        lash_core::facade_support::PluginSpec::new()
            .with_checkpoint(lash_core::hook_key!("failing-checkpoint-observer"), hook),
    ))
}

/// A checkpoint observer has no veto, but its failure fails the checkpoint,
/// which then accepts nothing: the turn stops, and the steer it was to
/// deliver is neither applied by that turn nor in its history. It stays the
/// session's mail and is admitted by a later run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_observer_failure_leaves_active_input_pending_without_application_evidence() {
    const SESSION: &str = "checkpoint-observer-failure";
    const STEER: &str = "must remain pending";
    let run = crate::TurnId::from("checkpoint-observer-failure-turn");
    let steer_run = crate::TurnId::from("checkpoint-observer-failure-steer");
    let late: LateCore = Arc::default();
    let sender: Sender = {
        let late = Arc::clone(&late);
        let (run, steer_run) = (run.clone(), steer_run.clone());
        Arc::new(move || {
            let late = Arc::clone(&late);
            let (run, steer_run) = (run.clone(), steer_run.clone());
            Box::pin(async move {
                let core = late.get().expect("the core is built");
                core.session(crate::SessionId::from(SESSION))
                    .durable()
                    .await
                    .expect("the session's durable handle")
                    .send(crate::TurnInput::text(STEER))
                    .id(steer_run)
                    .ingress(lash_core::TurnInputIngress::active_turn(
                        run,
                        lash_core::TurnInputCheckpointBoundary::AfterWork,
                    ))
                    .await
                    .expect("the running turn accepts its steer");
            })
        })
    };
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        scripted(Arc::clone(&requests), Arc::new(send_then_answer)),
        mock_llm_profile_spec(),
    )
    .tools(Arc::new(SendTool(sender)))
    .plugin(failing_checkpoint_observer())
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    assert!(late.set(core).is_ok());
    let core = late.get().expect("the core");
    let session = created(core, SESSION).await;
    let output = session
        .send(crate::TurnInput::text("hello"))
        .id(run.clone())
        .output()
        .await
        .expect("the turn settles");
    assert!(
        matches!(output.result.outcome, crate::TurnOutcome::Stopped(_)),
        "a failed checkpoint stops the turn: {:?}",
        output.result.outcome
    );
    let steered = session.attach_id(steer_run.clone());
    assert!(
        output
            .activities
            .iter()
            .all(|activity| match &activity.event {
                lash_core::TurnEvent::QueuedInputAccepted { applications } => applications
                    .iter()
                    .all(|application| application.input_id != *steered.input_id()),
                _ => true,
            }),
        "a failed checkpoint emits no application evidence for its steer"
    );
    let read_view = output.result.state.read_view();
    assert!(
        read_view
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| part.content() != STEER),
        "a steer the failed checkpoint did not accept is not in its history"
    );
    assert!(
        requests
            .lock_recover()
            .iter()
            .all(|request| !request_text(request).contains(STEER)),
        "the failed turn never showed the steer to its model"
    );
    assert_ne!(
        steered.run().await.expect("the steer's run"),
        Some(run),
        "the steer is not bound to the turn whose checkpoint failed"
    );
    core.shutdown().await.expect("shutdown");
}
