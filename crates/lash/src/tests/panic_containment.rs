//! A host's panic at a tool or provider seam is a typed failure of the
//! turn that met it, and the session runs its next turn, through a host's
//! `send()` on the core's node over SQLite memory stores (FIG-5307; the
//! runtime laws FIG-5190 deleted with the engine double). Containment is
//! quiet unless a harness turns it loud, and nothing in this binary does.

use super::*;

use crate::support::TurnInput;
use lash_core::llm::types::LlmOutputPart;
use std::collections::VecDeque;

/// The fixture echo tool, whose body panics.
struct PanicTool;

#[async_trait]
impl ToolProvider for PanicTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        lash_core::testing::FixtureTools.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        lash_core::testing::FixtureTools.resolve_contract(name)
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        panic!("tool payload only")
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_panic_is_recorded_and_the_session_runs_its_next_turn() -> Result<()> {
    let (core, session) = session(
        "tool-panic-session",
        provider(
            false,
            vec![
                LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "panic-call".to_owned(),
                        tool_name: lash_core::testing::FIXTURE_ECHO_TOOL.to_owned(),
                        input_json: r#"{"value":"boom"}"#.to_owned(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                },
                text_response("turn recovered"),
                text_response("next turn works"),
            ],
        ),
        Some(Arc::new(PanicTool)),
    )
    .await?;

    let first = session
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;
    let lash_core::ToolCallOutcome::Failure(failure) = &first.result.tool_calls[0].output.outcome
    else {
        panic!(
            "a tool panic is recorded as the call's failure: {:?}",
            first.result.tool_calls
        )
    };
    assert_eq!(failure.class, lash_core::ToolFailureClass::Internal);
    assert_eq!(failure.code, "tool_panicked");
    assert_eq!(failure.message, "tool payload only");
    assert_eq!(failure.suggested_delay_ms, None);
    assert_eq!(first.assistant_message(), Some("turn recovered"));

    let next = session.send(TurnInput::text("continue")).output().await?;
    assert_eq!(next.assistant_message(), Some("next turn works"));
    core.shutdown().await?;
    Ok(())
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
#[ignore = "FIG-5349: a loud panic kills the node's turn task before its typed attempt commits, so the node redrives the call"]
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
                    .args(["--exact", TEST, "--include-ignored", "--nocapture"])
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
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .payload()
            .downcast_ref::<String>()
            .is_some_and(|message| message == "provider_panicked: provider payload only")
        {
            hook_count.fetch_add(1, Ordering::SeqCst);
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
