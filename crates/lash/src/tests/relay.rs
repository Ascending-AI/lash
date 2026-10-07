//! Relay (FIG-4441): an RLM session under the relay execution policy keeps
//! nothing between steps but the arguments of its last committed
//! `control.next` call.
//!
//! The laws drive the shipped RLM plugin through the facade with a scripted
//! model that answers by step number, which every relay request names in its
//! harness message, so a redriven step gets the same answer as the first time.

use super::*;
use crate::TurnInput;
use tokio::sync::oneshot;

/// A script answer starting with this is sent as bare prose, not as a cell.
const PROSE_REPLY: &str = "prose:";

/// Every request the model served, in order.
type Served = Arc<StdMutex<Vec<LlmRequest>>>;

/// What the scripted model answers to one request: the program of the step
/// its harness message names, for the turn input it shows.
type Script = Arc<dyn Fn(&RelayRequest) -> String + Send + Sync>;

/// One relay request as the laws read it.
struct RelayRequest {
    step: usize,
    harness: String,
    context: Vec<String>,
    context_breakpoint: bool,
}

impl RelayRequest {
    fn of(request: &LlmRequest) -> Self {
        let texts = |message: &lash_core::llm::types::LlmMessage| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text {
                        text,
                        cache_breakpoint,
                        ..
                    } => Some((text.to_string(), *cache_breakpoint)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let harness = request
            .messages
            .last()
            .map(|message| {
                texts(message)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect::<String>()
            })
            .unwrap_or_default();
        let context = match request.messages.as_slice() {
            [context, _harness] => texts(context),
            _ => Vec::new(),
        };
        let step = harness
            .strip_prefix("=== HARNESS · step ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|step| step.parse().ok())
            .expect("a relay request names its step");
        Self {
            step,
            harness,
            context_breakpoint: context.last().is_some_and(|(_, marked)| *marked),
            context: context.into_iter().map(|(text, _)| text).collect(),
        }
    }
}

fn relay_provider(served: &Served, script: Script) -> ProviderHandle {
    let served = Arc::clone(served);
    crate::testing::TestProvider::builder()
        .kind("relay-law")
        .complete(move |request| {
            let served = Arc::clone(&served);
            let program = script(&RelayRequest::of(&request));
            async move {
                served.lock_recover().push(request);
                Ok(text_response(&match program.strip_prefix(PROSE_REPLY) {
                    Some(prose) => prose.to_string(),
                    None => typescript_block(&program),
                }))
            }
        })
        .build()
        .into_handle()
}

fn relay_factory(
    backend: &lash_core::Backend,
    budget_tokens: Option<usize>,
) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    let mut config = lash_protocol_rlm::RlmProtocolPluginConfig::builder()
        .channel(lash_protocol_rlm::RlmChannel::Cell)
        .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
        .build()
        .with_execution_policy(lash_protocol_rlm::RlmExecutionPolicy::Relay);
    if budget_tokens.is_some() {
        config.continue_as_soft_warn_tokens = budget_tokens;
    }
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        config,
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        backend,
    )
    .with_worker_service(untimed_fixture_workers())
}

/// A relay session over `backend` whose model follows `script`.
async fn relay_session(
    backend: lash_core::Backend,
    id: &str,
    served: &Served,
    script: Script,
    tools: Arc<dyn ToolProvider>,
    budget_tokens: Option<usize>,
) -> Result<(LashCore, crate::LashSession)> {
    let factory = relay_factory(&backend, budget_tokens);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(relay_provider(served, script), mock_llm_profile_spec())
        .tools(tools)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, session))
}

fn requests(served: &Served) -> Vec<RelayRequest> {
    served.lock_recover().iter().map(RelayRequest::of).collect()
}

/// The committed transcript, read from the store's head: the session's
/// turns run on its node's session actor, not in this handle.
async fn transcript_texts(session: &crate::LashSession) -> Vec<String> {
    session
        .observe()
        .snapshot()
        .await
        .expect("the session's committed head")
        .read_view
        .messages()
        .iter()
        .map(crate::message_text)
        .collect()
}

/// A tool that counts its calls: an effect the harness must not repeat.
struct BumpTools {
    calls: Arc<AtomicUsize>,
}

fn bump_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::ToolDefinition::raw(
        "tool:bump",
        "bump",
        "Bump the counter.",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], "bump"))
}

#[async_trait]
impl ToolProvider for BumpTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![bump_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "bump").then(|| Arc::new(bump_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        lash_core::ToolOutcome::ok(serde_json::json!({ "count": count })).into()
    }
}

/// No tools: a relay step's only calls are its control calls.
struct NoTools;

#[async_trait]
impl ToolProvider for NoTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::err_fmt("a relay law declares no tools").into()
    }
}

fn no_tools() -> Arc<dyn ToolProvider> {
    Arc::new(NoTools)
}

fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}

fn script(program: impl Fn(&RelayRequest) -> String + Send + Sync + 'static) -> Script {
    Arc::new(program)
}

/// A committed `next` is the next step's whole state: its `context` is the
/// prompt's context, entry for entry with the cache breakpoint on the last,
/// its `vars` are the globals, and nothing else the step bound survives.
#[tokio::test]
async fn a_committed_next_sets_the_next_steps_context_and_globals() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-commit",
        &served,
        script(|request| match request.step {
            1 => r#"const scratch = "left behind";
const n = 41;
await control.next({ context: ["fact: n is 41", "todo: add one"], vars: { n } });"#
                .to_string(),
            _ => r#"await control.send_user_output({ text: `n+1 = ${n + 1}; context ${context.length}` });
await control.next({ context: [...context, "answered"], final: true });"#
                .to_string(),
        }),
        no_tools(),
        None,
    )
    .await?;

    let output = session
        .send(TurnInput::text("add one to n"))
        .output()
        .await?;

    assert_eq!(output.assistant_message(), Some("n+1 = 42; context 2"));
    let requests = requests(&served);
    assert_eq!(requests.len(), 2, "final: true ends the turn after step 2");
    assert!(requests[0].context.is_empty(), "a new session starts empty");
    assert_eq!(requests[1].context, ["fact: n is 41", "todo: add one"]);
    assert!(requests[1].context_breakpoint);
    assert!(requests[1].harness.contains("add one to n"));
    assert!(requests[1].harness.contains("- `n`: number"));
    assert!(
        requests[1]
            .harness
            .contains("dropped (not in vars):\n- `scratch`: string (11 chars)\n"),
        "a dropped variable is named with its kind and size, never its value: {}",
        requests[1].harness
    );
    let transcript = transcript_texts(&session).await;
    assert_eq!(transcript, ["add one to n", "n+1 = 42; context 2"]);
    Ok(())
}

/// A step that throws commits no context, vars or output, and the REPL
/// mutations it made before throwing are gone at the next step.
#[tokio::test]
async fn a_step_that_throws_commits_nothing_and_leaves_no_state() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-throw",
        &served,
        script(|request| match request.step {
            1 => r#"await control.next({ context: ["c1"], vars: { count: 1 } });"#.to_string(),
            2 => r#"count = count + 100;
context.push("mutated");
await control.send_user_output({ text: "must not be delivered" });
throw new Error("boom");"#
                .to_string(),
            _ => r#"await control.send_user_output({ text: `count=${count} context=${context.join(",")}` });
await control.next({ context, final: true });"#
                .to_string(),
        }),
        no_tools(),
        None,
    )
    .await?;

    let output = session.send(TurnInput::text("count")).output().await?;

    assert_eq!(output.assistant_message(), Some("count=1 context=c1"));
    let requests = requests(&served);
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].context, ["c1"]);
    assert!(
        requests[2].harness.contains("NOT committed"),
        "{}",
        requests[2].harness
    );
    assert!(requests[2].harness.contains("boom"));
    assert!(
        !transcript_texts(&session)
            .await
            .iter()
            .any(|text| text == "must not be delivered")
    );
    Ok(())
}

/// A step that never calls `next` commits nothing, and the next harness
/// message says so.
#[tokio::test]
async fn a_step_that_never_calls_next_commits_nothing() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-no-next",
        &served,
        script(|request| match request.step {
            1 => r#"await control.next({ context: ["kept"] });"#.to_string(),
            2 => r#"await control.send_user_output({ text: "lost" });
const forgotten = 1;"#
                .to_string(),
            _ => r#"await control.send_user_output({ text: `context=${context.join(",")}` });
await control.next({ context, final: true });"#
                .to_string(),
        }),
        no_tools(),
        None,
    )
    .await?;

    let output = session.send(TurnInput::text("go")).output().await?;

    assert_eq!(output.assistant_message(), Some("context=kept"));
    let requests = requests(&served);
    assert_eq!(requests[2].context, ["kept"]);
    assert!(
        requests[2].harness.contains("never called `control.next`"),
        "{}",
        requests[2].harness
    );
    assert!(
        !transcript_texts(&session)
            .await
            .iter()
            .any(|text| text == "lost")
    );
    Ok(())
}

/// A `next` whose context is over the session's budget is refused: the step
/// does not commit and the refusal names the budget.
#[tokio::test]
async fn an_over_budget_next_is_refused() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-budget",
        &served,
        script(|request| match request.step {
            1 => r#"await control.next({ context: ["x".repeat(100)] });"#.to_string(),
            _ => r#"await control.send_user_output({ text: `entries=${context.length}` });
await control.next({ context: ["short"], final: true });"#
                .to_string(),
        }),
        no_tools(),
        // 10 tokens: a 40-character context budget.
        Some(10),
    )
    .await?;

    let output = session.send(TurnInput::text("go")).output().await?;

    assert_eq!(output.assistant_message(), Some("entries=0"));
    let requests = requests(&served);
    assert!(requests[1].context.is_empty());
    assert!(
        requests[1].harness.contains("over the 40-character budget"),
        "{}",
        requests[1].harness
    );
    assert!(requests[1].harness.contains("NOT committed"));
    Ok(())
}

/// The effects of a step that ran and committed nothing are listed in the
/// next harness message, and the harness never runs them again.
#[tokio::test]
async fn a_failed_attempts_effect_receipts_reach_the_next_harness_once() -> Result<()> {
    let served = Served::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-receipts",
        &served,
        script(|request| match request.step {
            1 => r#"await tools.bump({});
throw new Error("after the effect");"#
                .to_string(),
            _ => r#"await control.send_user_output({ text: "bumped once" });
await control.next({ context: ["bumped"], final: true });"#
                .to_string(),
        }),
        Arc::new(BumpTools {
            calls: Arc::clone(&calls),
        }),
        None,
    )
    .await?;

    let output = session.send(TurnInput::text("bump")).output().await?;

    assert_eq!(output.assistant_message(), Some("bumped once"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the effect ran exactly once"
    );
    let requests = requests(&served);
    let harness = &requests[1].harness;
    assert!(
        harness.contains("--- Effects of steps that did not commit ---"),
        "{harness}"
    );
    assert!(
        harness.contains("- step 1:\n  - tools.bump → ok"),
        "{harness}"
    );
    Ok(())
}

/// `final: true` ends the turn once committed, and only with output: a final
/// step that sends nothing is refused and the turn goes on.
#[tokio::test]
async fn final_ends_the_turn_and_needs_output() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-final",
        &served,
        script(|request| match request.step {
            1 => r#"await control.next({ context: ["silent"], final: true });"#.to_string(),
            _ => r#"await control.send_user_output({ text: "done" });
await control.next({ context: ["done"], final: true });"#
                .to_string(),
        }),
        no_tools(),
        None,
    )
    .await?;

    let output = session.send(TurnInput::text("finish")).output().await?;

    assert_eq!(output.assistant_message(), Some("done"));
    let requests = requests(&served);
    assert_eq!(requests.len(), 2, "the committed final step ends the turn");
    assert!(
        requests[1].context.is_empty(),
        "the refused step kept nothing"
    );
    assert!(
        requests[1]
            .harness
            .contains("`final: true` needs the same step"),
        "{}",
        requests[1].harness
    );

    // The next turn starts from the last committed context.
    let output = session.send(TurnInput::text("again")).output().await?;
    assert_eq!(output.assistant_message(), Some("done"));
    let requests = self::requests(&served);
    assert_eq!(requests[2].context, ["done"]);
    assert!(requests[2].harness.contains("again"));
    assert!(!requests[2].harness.contains("finish"));
    Ok(())
}

/// User input sent while a relay turn runs is held for the next turn: the
/// running turn's later steps never see it, and the follow-on turn does.
#[tokio::test]
async fn mid_turn_user_input_waits_for_the_next_turn() -> Result<()> {
    let served = Served::default();
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let release_rx = Arc::new(StdMutex::new(Some(release_rx)));
    let provider_served = Arc::clone(&served);
    let provider = crate::testing::TestProvider::builder()
        .kind("relay-law")
        .complete(move |request| {
            let served = Arc::clone(&provider_served);
            let relay = RelayRequest::of(&request);
            let gate = (relay.step == 1 && relay.harness.contains("primary")).then(|| {
                (
                    started_tx.lock_recover().take(),
                    release_rx.lock_recover().take(),
                )
            });
            let program = match (relay.step, relay.harness.contains("steer")) {
                (_, true) => {
                    r#"await control.send_user_output({ text: "steer seen" });
await control.next({ context: [...context, "steered"], final: true });"#
                }
                (1, false) => r#"await control.next({ context: ["primary step 1"] });"#,
                (_, false) => {
                    r#"await control.send_user_output({ text: "primary done" });
await control.next({ context: [...context, "primary done"], final: true });"#
                }
            };
            async move {
                served.lock_recover().push(request);
                if let Some((started, release)) = gate {
                    if let Some(started) = started {
                        let _ = started.send(());
                    }
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                }
                Ok(text_response(&typescript_block(program)))
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let factory = relay_factory(&backend, None);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(no_tools())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("relay-mid-turn").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let active_turn_id = "relay-primary-turn";
    let primary = session
        .send(TurnInput::text("primary"))
        .id(crate::TurnId::parse(active_turn_id).expect("nonblank host identity"))
        .await?;
    let turn = tokio::spawn(async move { primary.outcome().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), started_rx)
        .await
        .expect("the first step's model call starts")
        .expect("start signal");
    let steer = session
        .send(TurnInput::text("steer"))
        .id(crate::TurnId::parse("relay-steer").expect("nonblank host identity"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await?;
    release_tx.send(()).expect("release the first step");
    let primary = within("the primary turn", turn).await.expect("turn task")?;
    assert_eq!(primary.status(), crate::TurnStatus::Answered);
    let steered = within("the follow-on turn", steer.output()).await?;
    assert_eq!(steered.assistant_message(), Some("steer seen"));

    let requests = requests(&served);
    assert_eq!(
        requests.len(),
        3,
        "primary steps 1 and 2, then the follow-on"
    );
    assert_eq!(requests[1].step, 2);
    assert!(
        !requests[1].harness.contains("steer"),
        "the running turn's next step never sees mid-turn input: {}",
        requests[1].harness
    );
    assert_eq!(
        requests[2].step, 1,
        "the held input starts a turn of its own"
    );
    assert!(requests[2].harness.contains("steer"));
    assert_eq!(requests[2].context, ["primary step 1", "primary done"]);
    let transcript = transcript_texts(&session).await;
    assert_eq!(
        transcript,
        ["primary", "primary done", "steer", "steer seen"],
        "the held input is committed after the turn that held it"
    );
    Ok(())
}

/// Cancellation still reaches a relay turn mid-turn.
#[tokio::test]
async fn cancellation_reaches_a_relay_turn_mid_turn() -> Result<()> {
    let served = Served::default();
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let provider_served = Arc::clone(&served);
    let provider = crate::testing::TestProvider::builder()
        .kind("relay-law")
        .complete(move |request| {
            let served = Arc::clone(&provider_served);
            let relay = RelayRequest::of(&request);
            let started = (relay.step == 2)
                .then(|| started_tx.lock_recover().take())
                .flatten();
            async move {
                served.lock_recover().push(request);
                if let Some(started) = started {
                    let _ = started.send(());
                    std::future::pending::<()>().await;
                }
                Ok(text_response(&typescript_block(
                    r#"await control.next({ context: ["step 1"] });"#,
                )))
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let factory = relay_factory(&backend, None);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(no_tools())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("relay-cancel").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let turn_id = "relay-cancelled-turn";
    let running = session
        .send(TurnInput::text("run until cancelled"))
        .id(crate::TurnId::parse(turn_id).expect("nonblank host identity"))
        .await?;
    let turn = tokio::spawn(async move { running.outcome().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), started_rx)
        .await
        .expect("step 2's model call starts")
        .expect("start signal");
    let stopped = session
        .cancel(crate::CancelTarget::Run(lash_sansio::TurnId::from(turn_id)))
        .await?;
    assert!(matches!(stopped, crate::CancelReceipt::Requested { .. }));
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the cancelled run settles")
        .expect("turn task")?;
    assert_eq!(cancelled.status(), crate::TurnStatus::Cancelled);
    Ok(())
}

/// Bounded so a law that regresses fails rather than hangs.
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish"))
}

/// A relay turn moved to another node by a drain resumes from its committed
/// rows: the step the next build asks reads the context and the vars of the
/// last committed `next`, and a step asked again sees the same context and
/// harness.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_resumed_on_another_node_reads_the_committed_context_and_vars() -> Result<()> {
    let stores = sqlite_memory_store_set().await;
    let served = Served::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let held_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = || {
        let served = Arc::clone(&served);
        let entered = Arc::clone(&entered);
        let gate = Arc::clone(&gate);
        let held_once = Arc::clone(&held_once);
        crate::testing::TestProvider::builder()
            .kind("relay-law")
            .complete(move |request| {
                let served = Arc::clone(&served);
                let entered = Arc::clone(&entered);
                let gate = Arc::clone(&gate);
                let step = RelayRequest::of(&request).step;
                let program = match step {
                    1 => {
                        r#"const secret = 7;
await control.next({ context: ["the secret is kept in vars"], vars: { secret, label: "s" } });"#
                    }
                    _ => {
                        r#"await control.send_user_output({ text: `${label}=${secret}` });
await control.next({ context: [...context, "reported"], final: true });"#
                    }
                };
                let hold = step == 1 && !held_once.swap(true, Ordering::SeqCst);
                async move {
                    served.lock_recover().push(request);
                    if hold {
                        // The old build is asked to drain while step 1's
                        // first model call is in flight.
                        entered.notify_one();
                        gate.notified().await;
                    }
                    Ok(text_response(&typescript_block(program)))
                }
            })
            .build()
            .into_handle()
    };
    let build = |owner: &str| -> Result<LashCore> {
        let backend = lash_conformance::backend_over(Arc::clone(&stores) as _);
        let factory = relay_factory(&backend, None);
        explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
            .serve_test_llm_profile(provider(), mock_llm_profile_spec())
            .tools(no_tools())
            .build(lash_core::LeaseOwnerIdentity::opaque(owner, "boot-1"))
    };

    let old = build("relay-old-build")?;
    let session_id = crate::SessionId::parse("relay-moved").expect("nonblank host identity");
    let session = old
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let held = tokio::spawn(session.send(TurnInput::text("report the secret")).output());
    within("step 1's model call", entered.notified()).await;
    let drain = old.drain();
    tokio::pin!(drain);
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(drain.as_mut(), cx))
    })
    .await;
    assert!(
        first.is_pending(),
        "the old build drains only once its turn reaches a committed phase"
    );
    gate.notify_one();
    let report = within("the drain", drain).await.expect("the node drained");
    assert_eq!(report.sessions, vec![session_id]);
    assert!(
        requests(&served).iter().all(|request| request.step == 1),
        "the old build stopped before step 2"
    );

    let new = build("relay-new-build")?;
    let output = within("the resumed turn", held)
        .await
        .expect("the handle task")?;
    assert_eq!(output.assistant_message(), Some("s=7"));
    let served = served.lock_recover().clone();
    let steps = served.iter().map(RelayRequest::of).collect::<Vec<_>>();
    assert_eq!(
        steps.iter().filter(|request| request.step == 2).count(),
        1,
        "step 2 is asked once, on the next build: {:?}",
        steps.iter().map(|request| request.step).collect::<Vec<_>>()
    );
    // The next build may ask the step the old one drained under again; a
    // step asked twice sees the same context and harness.
    let step_one = served
        .iter()
        .filter(|request| RelayRequest::of(request).step == 1)
        .collect::<Vec<_>>();
    assert!(
        step_one
            .windows(2)
            .all(|pair| pair[0].messages == pair[1].messages),
        "a step asked again sees the same context and harness"
    );
    let step_two = steps
        .iter()
        .find(|request| request.step == 2)
        .expect("step 2 was asked");
    assert_eq!(step_two.context, ["the secret is kept in vars"]);
    new.shutdown().await?;
    old.shutdown().await?;
    Ok(())
}

/// The cached prefix holds: the system prompt and the tools a relay request
/// renders are byte-identical across the steps of a turn and across turns.
#[tokio::test]
async fn system_prompt_and_tools_are_byte_identical_across_steps_and_turns() -> Result<()> {
    let served = Served::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-stable-prefix",
        &served,
        script(|request| match request.step {
            1 => r#"await tools.bump({});
await control.next({ context: [...context, "bumped"], vars: { n: 1 } });"#
                .to_string(),
            _ => r#"await control.send_user_output({ text: `entries ${context.length}` });
await control.next({ context, final: true });"#
                .to_string(),
        }),
        Arc::new(BumpTools {
            calls: Arc::clone(&calls),
        }),
        None,
    )
    .await?;
    session.send(TurnInput::text("first")).output().await?;
    session.send(TurnInput::text("second")).output().await?;

    let requests = served.lock_recover().clone();
    assert_eq!(requests.len(), 4, "two steps in each of two turns");
    let prefix = |request: &LlmRequest| {
        let body = lash_provider_anthropic::testing::serialize_request(
            request,
            lash_core::provider::CacheRetention::Short,
        )
        .expect("anthropic body");
        serde_json::to_vec(&(&body["system"], &body["tools"])).expect("prefix bytes")
    };
    assert!(
        requests[0]
            .instructions
            .as_deref()
            .is_some_and(|system| system.contains("bump")),
        "the system prompt describes the tools"
    );
    let first = prefix(&requests[0]);
    for (index, request) in requests.iter().enumerate().skip(1) {
        assert!(
            prefix(request) == first,
            "request {index}'s system prompt or tools differ from the first's"
        );
    }
    Ok(())
}

/// A reply in bare prose commits nothing, and the next harness hands that
/// prose back inside the exact cell that would send it to the user; the
/// model copying it ends the turn with the prose as the reply.
#[tokio::test]
async fn a_prose_reply_comes_back_as_the_cell_that_sends_it() -> Result<()> {
    let served = Served::default();
    let (_core, session) = relay_session(
        sqlite_memory_store_backend().await,
        "relay-prose-reply",
        &served,
        script(|request| {
            let cell = request
                .harness
                .split_once("reply with exactly this program.\n<typescript>\n")
                .and_then(|(_, rest)| rest.split_once("\n</typescript>"))
                .map(|(cell, _)| cell.to_string());
            match cell {
                Some(cell) => cell,
                None => format!("{PROSE_REPLY}There is no \"Q3\" message; which inbox?"),
            }
        }),
        no_tools(),
        None,
    )
    .await?;

    let output = session.send(TurnInput::text("find Q3")).output().await?;

    assert_eq!(
        output.assistant_message(),
        Some("There is no \"Q3\" message; which inbox?")
    );
    let requests = requests(&served);
    assert_eq!(requests.len(), 2, "the copied cell ends the turn");
    assert!(
        requests[1].harness.contains(
            "await control.send_user_output({ text: \"There is no \\\"Q3\\\" message; which inbox?\" });\nawait control.next({ context, final: true });"
        ),
        "{}",
        requests[1].harness
    );
    Ok(())
}
