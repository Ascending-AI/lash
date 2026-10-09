//! A host's panic at a tool or provider seam is a typed failure of the
//! turn that met it, and the session runs its next turn, through a host's
//! `send()` on the core's node over SQLite memory stores (FIG-5307; the
//! runtime laws FIG-5190 deleted with the engine double). Containment is
//! quiet unless a harness turns it loud, and nothing in this binary does.

use super::*;

use crate::support::TurnInput;
use lash_core::llm::types::LlmOutputPart;
use std::collections::VecDeque;

/// The fixture echo tool panics during future construction or polling.
#[derive(Default)]
struct PanicTool {
    calls: AtomicUsize,
    construction: bool,
}

impl ToolProvider for PanicTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![panic_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == lash_core::testing::FIXTURE_ECHO_TOOL)
            .then(|| Arc::new(panic_tool_definition().contract()))
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _call: lash_core::ToolCall<'life1>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = lash_core::ToolAttemptOutcome> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.construction {
            panic!("tool payload only");
        }
        Box::pin(async { panic!("tool payload only") })
    }
}

fn panic_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinitionBindingExt::with_tool_binding(
        lash_core::testing::fixture_echo_definition(),
        lash_core::ToolBinding::new(["tools"], "fixture_echo"),
    )
}

/// Answers `replies` in order; `panic_first` makes the first call panic
/// instead.
fn provider(panic_first: bool, replies: Vec<LlmResponse>) -> ProviderHandle {
    let replies = Arc::new(StdMutex::new(VecDeque::from(replies)));
    let panic_next = Arc::new(std::sync::atomic::AtomicBool::new(panic_first));
    crate::testing::TestProvider::builder()
        .kind("panic-containment")
        .complete(move |_request| {
            let replies = Arc::clone(&replies);
            let panic_now = panic_next.swap(false, Ordering::SeqCst);
            async move {
                if panic_now {
                    panic!("provider payload only");
                }
                Ok(replies
                    .lock_recover()
                    .pop_front()
                    .expect("scripted reply queue is exhausted"))
            }
        })
        .build()
        .into_handle()
}

async fn session(
    id: &str,
    provider: ProviderHandle,
    tools: Option<Arc<dyn ToolProvider>>,
) -> Result<(LashCore, crate::LashSession)> {
    let mut builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec());
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    let core = builder.build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, session))
}

/// FIG-5775: a host tool panic stops its turn before another model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_tool_panic_stops_before_another_model_call() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let model = crate::testing::TestProvider::builder()
        .kind("panic-containment")
        .complete(move |_| {
            let ordinal = counted.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(if ordinal == 0 {
                    panic_tool_response()
                } else {
                    text_response("next turn works")
                })
            }
        })
        .build()
        .into_handle();
    let (core, session) = session(
        "tool-panic-session",
        model,
        Some(Arc::new(PanicTool::default())),
    )
    .await?;
    let first = session
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;
    assert_tool_panic(&first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = session.send(TurnInput::text("continue")).output().await?;
    assert_eq!(next.assistant_message(), Some("next turn works"));
    core.shutdown().await?;
    Ok(())
}

fn panic_tool_response() -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "panic-call".to_owned(),
            tool_name: lash_core::testing::FIXTURE_ECHO_TOOL.to_owned(),
            input_json: r#"{"value":"boom"}"#.to_owned(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

fn assert_tool_panic(output: &crate::TurnOutput) {
    let crate::TurnOutcome::Stopped(crate::TurnStop::ToolPanicked {
        tool_name,
        call_id,
        message,
    }) = &output.result.outcome
    else {
        panic!("expected ToolPanicked, got {:?}", output.result.outcome);
    };
    assert_eq!(tool_name, "fixture_echo");
    assert_eq!(message, "tool payload only");
    let failed = output
        .result
        .tool_calls
        .iter()
        .find(|call| call.output.tool_panic_stop().is_some())
        .expect("committed panicked call");
    let lash_core::ToolCallOutcome::Failure(failure) = &failed.output.outcome else {
        unreachable!()
    };
    assert!(
        failure
            .message
            .contains("Outside work may already have happened")
    );
    assert_eq!(
        failed.output.tool_panic_stop(),
        Some(crate::TurnStop::ToolPanicked {
            tool_name: tool_name.clone(),
            call_id: call_id.clone(),
            message: message.clone(),
        })
    );
    assert_eq!(output.assistant_message(), None);
}

#[cfg(feature = "rlm")]
async fn cell_panic(process: bool) -> Result<()> {
    let script = if process {
        r#"const run = async () => {
  let result;
  try { result = await tools.fixture_echo({value: "boom"}); } catch (e) { result = e; }
  await tools.fixture_echo({value: "after panic"});
  return 1;
};
const handle = await processes.start({definition: run});
let result;
try { result = await handle; } catch (e) { result = e; }
finish("must not finish");"#
    } else {
        r#"let result;
try { result = await tools.fixture_echo({value: "boom"}); } catch (e) { result = e; }
await tools.fixture_echo({value: "after panic"});
finish("must not finish");"#
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let model = crate::testing::TestProvider::builder()
        .kind("panic-cell")
        .complete(move |_| {
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                Ok(text_response(&typescript_block(if first {
                    script
                } else {
                    "finish(2);"
                })))
            }
        })
        .build()
        .into_handle();
    let tool = Arc::new(PanicTool::default());
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(model, mock_llm_profile_spec())
            .tools(tool.clone())
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ))
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse(if process {
                "process-panic"
            } else {
                "cell-panic"
            })
            .expect("session id"),
        )
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;
    assert_eq!(
        tool.calls.load(Ordering::SeqCst),
        1,
        "tool must run: {:?}",
        output.activities
    );
    assert_tool_panic(&output);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    core.shutdown().await?;
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_tool_panic_stops_before_another_model_call() -> Result<()> {
    cell_panic(false).await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_tool_panic_stops_before_another_model_call() -> Result<()> {
    cell_panic(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_panic_records_the_typed_attempt_releases_the_lease_and_next_turn_succeeds()
-> Result<()> {
    let (core, session) = session(
        "provider-panic-session",
        provider(true, vec![text_response("next turn works")]),
        None,
    )
    .await?;

    let failed = session
        .send(TurnInput::text("panic provider"))
        .output()
        .await?;
    assert!(
        matches!(
            failed.result.outcome,
            crate::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
        ),
        "{:?}",
        failed.result.outcome
    );
    assert_eq!(
        panicked_attempt(&failed).0.as_deref(),
        Some("lash:provider_panicked")
    );

    // The session's next turn is admitted and runs at once: the failed
    // turn released its lease on its typed failure path.
    let next = session.send(TurnInput::text("continue")).output().await?;
    assert_eq!(next.assistant_message(), Some("next turn works"));
    core.shutdown().await?;
    Ok(())
}

/// The typed attempt a provider panic leaves on its turn's report.
fn panicked_attempt(output: &crate::TurnOutput) -> (Option<String>, String) {
    let attempt = output
        .result
        .llm_calls
        .first()
        .and_then(|call| call.attempts.first())
        .expect("provider panic attempt record");
    (
        attempt
            .error
            .as_ref()
            .and_then(|error| error.code.as_ref())
            .map(|code| code.namespaced()),
        format!("{:?}", output.result.outcome),
    )
}

/// ADR 0054: loudness changes propagation only. A loud provider panic
/// reaches the process's panic hook after its turn committed the typed
/// attempt a quiet run commits, and the provider runs once. Each mode runs
/// in its own test process, since loudness is process-scoped.
#[allow(
    clippy::disallowed_methods,
    reason = "isolated test processes own the process-scoped panic mode"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_panic_effect_is_identical_before_quiet_return_or_loud_reraise() -> Result<()> {
    const TEST: &str = "tests::panic_containment::provider_panic_effect_is_identical_before_quiet_return_or_loud_reraise";
    const CASE: &str = "LASH_LOUD_PANIC_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let mut effects = Vec::new();
        for case in ["quiet", "loud"] {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                tokio::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", TEST, "--nocapture"])
                    .args(["--test-threads=1"])
                    .env(CASE, case)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("an isolated panic case has a bounded lifetime")
            .expect("the isolated case runs");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{case}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            effects.push(
                stdout
                    .lines()
                    .find_map(|line| line.split_once("EFFECT ").map(|(_, effect)| effect))
                    .expect("the case reports its effect")
                    .to_owned(),
            );
        }
        assert_eq!(
            effects[0], effects[1],
            "a loud run commits the effect a quiet run commits"
        );
        return Ok(());
    };
    let loud = case == "loud";
    crate::testing::set_loud(loud);
    let reraised = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&reraised);
    let raised = Arc::new(tokio::sync::Notify::new());
    let hook_raised = Arc::clone(&raised);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .payload()
            .downcast_ref::<String>()
            .is_some_and(|message| message == "provider_panicked: provider payload only")
        {
            hook_count.fetch_add(1, Ordering::SeqCst);
            hook_raised.notify_one();
        }
        previous(info);
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("panic-containment")
        .complete(move |_request| {
            let ordinal = counted.fetch_add(1, Ordering::SeqCst);
            async move {
                if ordinal == 0 {
                    panic!("provider payload only");
                }
                Ok(text_response("redriven"))
            }
        })
        .build()
        .into_handle();
    let (core, session) =
        session(&format!("{case}-provider-panic-session"), provider, None).await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session.send(TurnInput::text("panic provider")).output(),
    )
    .await
    .expect("the turn settles")?;
    if loud {
        tokio::time::timeout(std::time::Duration::from_secs(30), raised.notified())
            .await
            .expect("loud propagation follows the committed terminal");
    }
    let effect = panicked_attempt(&output);
    println!("EFFECT {effect:?}");
    assert_eq!(
        effect.0.as_deref(),
        Some("lash:provider_panicked"),
        "{case}: {output:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the panicked call is not redriven"
    );
    assert_eq!(
        reraised.load(Ordering::SeqCst),
        usize::from(loud),
        "only a loud run re-raises the panic"
    );
    core.shutdown().await?;
    Ok(())
}

/// ADR 0054 and FIG-5775: the loud tool path commits its terminal before unwinding.
#[allow(
    clippy::disallowed_methods,
    reason = "isolated processes own panic policy"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_panic_effect_is_identical_before_quiet_return_or_loud_reraise() -> Result<()> {
    const TEST: &str = "tests::panic_containment::tool_panic_effect_is_identical_before_quiet_return_or_loud_reraise";
    const CASE: &str = "LASH_TOOL_PANIC_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let mut effects = Vec::new();
        for mode in ["quiet", "loud"] {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                tokio::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env(CASE, mode)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("bounded isolated case")
            .expect("isolated case");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{mode}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            effects.push(
                stdout
                    .lines()
                    .find_map(|line| {
                        line.split_once("EFFECT ")
                            .map(|(_, effect)| effect.to_owned())
                    })
                    .expect("effect"),
            );
        }
        assert_eq!(effects[0], effects[1]);
        return Ok(());
    };
    let loud = case == "loud";
    crate::testing::set_loud(loud);
    let raised = Arc::new(tokio::sync::Notify::new());
    let hook_raised = Arc::clone(&raised);
    let reraised = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&reraised);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .payload()
            .downcast_ref::<String>()
            .is_some_and(|message| message == "tool_panicked: tool payload only")
        {
            hook_count.fetch_add(1, Ordering::SeqCst);
            hook_raised.notify_one();
        }
        previous(info);
    }));
    let tool = Arc::new(PanicTool {
        construction: true,
        ..Default::default()
    });
    let (core, session) = session(
        "tool-effect-session",
        provider(
            false,
            vec![panic_tool_response(), text_response("redriven")],
        ),
        Some(tool.clone()),
    )
    .await?;
    let input = TurnInput::text("panic tool");
    let first = session
        .send(input.clone())
        .id(crate::TurnId::fixture("tool-panic-turn"))
        .output()
        .await?;
    assert_tool_panic(&first);
    if loud {
        tokio::time::timeout(std::time::Duration::from_secs(30), raised.notified())
            .await
            .expect("loud propagation follows the committed terminal");
    }
    assert_eq!(reraised.load(Ordering::SeqCst), usize::from(loud));
    let replay = session
        .send(input)
        .id(crate::TurnId::fixture("tool-panic-turn"))
        .output()
        .await?;
    assert_eq!(first.result.outcome, replay.result.outcome);
    assert_eq!(
        tool.calls.load(Ordering::SeqCst),
        1,
        "a committed panic never calls the tool again"
    );
    println!("EFFECT {:?}", first.result.outcome);
    core.shutdown().await?;
    Ok(())
}
