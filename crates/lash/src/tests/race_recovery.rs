//! ADR 0099 W5 on the product path: a race's loser is recovered after its
//! opener's worker dies (FIG-3397).
//!
//! The first cell races a held loser against a quick winner and ends; the
//! aggregate is consumed and the cell's result is journaled, so the losing
//! child is live under its opener and no cell will ever reopen its group. The
//! second cell parks on a gate, and the parent kills the worker there — a
//! real process death, not a dropped future. A fresh worker resumes the same
//! queued run: both cells replay from the journal up to the gate, and the only
//! thing that can finish the loser is the opener recovering its live group
//! when it starts. A dead worker is not a closed opener, so the loser must run
//! to its final record and realize its declared intent before the turn ends —
//! the case ADR 0099 Endpoint B-prime would have abandoned.
//!
//! The same run witnesses the resumed turn's incorporation: both leaves
//! enqueue a checkpoint message through an after-tool hook, which reaches the
//! transcript only through the opener's incorporation of its settlement. The
//! winner's rank was incorporated by the first cell before the crash, and the
//! resumed turn serves that cell from the journal, so its end must not
//! incorporate the rank a second time — the transcript carries each leaf's
//! message exactly once (ADR 0099 §6, §13).

use super::*;

const SESSION: &str = "race-recovery";
const INTENT_PROCESS: &str = "race-recovery-intent-target";
const INTENT_EVENT: &str = "race.recovery.loser";

/// The one leaf tool: `race.step({ id, hold, intent })`. A held step blocks in
/// the worker that is about to be killed and runs straight through in the one
/// that recovers it.
struct RaceTools {
    crash: bool,
    /// Raised when the loser runs in the recovering worker. The recovered
    /// turn's gate waits on it, so the opener is still live — and its end has
    /// not closed the group — when recovery drives the loser.
    loser_ran: Arc<tokio::sync::Notify>,
}

fn step_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:race_step",
        "race_step",
        "One race leaf whose settlement the test controls.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "hold": { "type": "boolean" },
                "intent": { "type": "boolean" },
                "message": { "type": "boolean" }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["race"], "step"))
}

#[async_trait]
impl ToolProvider for RaceTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![step_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "race_step").then(|| Arc::new(step_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        use std::io::Write as _;
        let id = call.args["id"].as_str().unwrap_or_default().to_string();
        let hold = call.args["hold"].as_bool().unwrap_or(false);
        let intent = call.args["intent"].as_bool().unwrap_or(false);
        if self.crash && hold {
            if id == "gate" {
                println!("crash_ready");
                std::io::stdout().flush().expect("flush the kill signal");
            }
            // Held until the parent kills this worker.
            std::future::pending::<()>().await;
        }
        if !self.crash && id == "loser" {
            self.loser_ran.notify_one();
        }
        if !self.crash && id == "gate" {
            // Bounded, so a recovery that never happens fails the case on its
            // assertion rather than hanging it.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.loser_ran.notified(),
            )
            .await;
        }
        let value = serde_json::json!({ "id": id });
        if !intent {
            return lash_core::ToolOutcome::ok(value).into();
        }
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(value),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::EmitProcessEvent(
                lash_core::EmitProcessEventIntent {
                    session_id: SessionId::from(SESSION),
                    process_id: lash_sansio::ProcessId::from(INTENT_PROCESS),
                    event_type: INTENT_EVENT.to_string(),
                    payload: serde_json::json!({ "id": id }),
                },
            )]),
        )
    }
}

fn cells() -> Vec<String> {
    vec![
        typescript_block(
            // Binds no global: a resumed turn rebuilds its cells from the
            // journal, and this case is about the loser, not the bindings.
            r#"console.log((await Promise.race([
  race.step({ id: "loser", hold: true, intent: true, message: true }),
  race.step({ id: "winner", message: true })
])).id);"#,
        ),
        typescript_block(
            r#"await race.step({ id: "gate", hold: true });
finish("done");"#,
        ),
    ]
}

/// The checkpoint message a leaf's settlement carries.
fn incorporated_message(id: &str) -> String {
    format!("race step {id} incorporated")
}

/// Enqueues [`incorporated_message`] after every step that asks for one. The
/// hook runs inside the leaf's own dispatch, so the message rides the leaf's
/// settlement and lands only when its opener incorporates that settlement.
fn message_plugin() -> Arc<dyn PluginFactory> {
    let hook: lash_core::plugin::AfterToolCallHook = Arc::new(|context| {
        Box::pin(async move {
            if context.tool_name != "race_step"
                || !context.args["message"].as_bool().unwrap_or(false)
            {
                return Ok(Vec::new());
            }
            let id = context.args["id"].as_str().unwrap_or_default();
            Ok(vec![
                lash_core::facade_support::AfterToolCallPluginDirective::EnqueueMessages(
                    lash_core::facade_support::EnqueueMessagesDirective {
                        messages: vec![lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            incorporated_message(id),
                        )],
                    },
                ),
            ])
        })
    });
    Arc::new(StaticPluginFactory::new(
        "race-recovery-messages",
        lash_core::facade_support::PluginSpec::new().with_after_tool_call(hook),
    ))
}

async fn register_intent_target(registry: &dyn ProcessRegistry) {
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                INTENT_PROCESS,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types(vec![lash_core::ProcessEventType {
                name: INTENT_EVENT.to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from(SESSION)],
        )
        .await
        .expect("register the intent target process");
}

/// The worker a parent runs twice: once to crash at the gate, once to recover.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess entry point for race recovery"]
#[expect(
    clippy::disallowed_methods,
    reason = "cold-recovery harness owns worker processes and durable test files"
)]
async fn race_recovery_worker() -> Result<()> {
    let Ok(action) = std::env::var("LASH_RACE_RECOVERY_ACTION") else {
        return Ok(());
    };
    let directory =
        std::path::PathBuf::from(std::env::var("LASH_RACE_RECOVERY_DIRECTORY").unwrap());
    let crash = action == "crash";
    // The recovering worker starts well after the dead worker's leases lapsed,
    // so the store hands the run over rather than reporting it contended.
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(if crash {
        1_800_000_000_000
    } else {
        1_800_000_600_000
    }));
    // The recovering worker replays both cells' model calls from the journal,
    // so any model call it makes live is a new step, which finishes.
    let scripted = Arc::new(TokioMutex::new(if crash {
        VecDeque::from(cells())
    } else {
        VecDeque::new()
    }));
    let provider = crate::testing::TestProvider::builder()
        .kind("race-recovery")
        .complete(move |_| {
            let scripted = Arc::clone(&scripted);
            async move {
                // A loser's checkpoint message delivered at completion reopens
                // the turn for one more model step, which finishes again.
                let text = scripted
                    .lock()
                    .await
                    .pop_front()
                    .unwrap_or_else(|| typescript_block(r#"finish("done");"#));
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle();
    // The durable execution-environment store is the backend's own:
    // the loser's retained request names the environment its dead worker
    // published, and a recovered child never invents one (ADR 0099 §3).
    let backend: lash_core::Backend = Arc::new(
        lash_sqlite_store::SqliteBackend::open_with_options_and_clock(
            directory.join("sessions"),
            lash_sqlite_store::SqliteBackendOptions::default(),
            Arc::clone(&clock),
        )
        .await
        .unwrap(),
    )
    .into();
    let registry = backend.process_registry();
    register_intent_target(registry.as_ref()).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(RaceTools {
            crash,
            loser_ran: Arc::new(tokio::sync::Notify::new()),
        }));
    let core = core
        .plugin(message_plugin())
        .build(crate::testing::runtime_lease_owner())?;
    let session = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match core.session(SESSION).open().await {
                Ok(session) => break session,
                Err(error)
                    if !crash
                        && format!("{error:?}").contains("Contended")
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(error) => return Err(error),
            }
        }
    };
    if crash {
        session
            .durable()
            .enqueue(TurnInput::text("race, then wait"))
            .id("race-input")
            .send()
            .await?;
        session.queued_turn().run().await?;
        panic!("the parent must kill the worker parked at the gate");
    }
    let report = session
        .queued_turn()
        .run()
        .await?
        .ran()
        .expect("the recovered worker resumes the pending run");
    assert_eq!(
        report.final_value(),
        Some(&serde_json::json!("done")),
        "the recovered turn finishes"
    );
    let recovered = registry
        .recent_events(&lash_sansio::ProcessId::from(INTENT_PROCESS), 16)
        .await
        .expect("read the intent target")
        .into_iter()
        .filter(|event| event.event_type == INTENT_EVENT)
        .filter_map(|event| event.payload["id"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        recovered,
        vec!["loser".to_string()],
        "the loser was recovered under its live opener and realized its intent"
    );
    let transcript = session
        .observe()
        .current_observation()
        .read_view
        .messages()
        .iter()
        .map(crate::turn::message_text)
        .collect::<Vec<_>>();
    for id in ["winner", "loser"] {
        assert_eq!(
            transcript
                .iter()
                .filter(|text| text.contains(&incorporated_message(id)))
                .count(),
            1,
            "the {id}'s settlement was incorporated exactly once across the crash: {transcript:#?}"
        );
    }
    println!("recovered");
    Ok(())
}

/// W5: the worker dies after its race was consumed and its cell journaled;
/// the resumed opener recovers the loser rather than abandoning it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_race_loser_is_recovered_after_its_openers_worker_dies() {
    crash_and_recover().await;
}

#[expect(
    clippy::disallowed_methods,
    reason = "cold-recovery harness owns worker processes and durable test files"
)]
async fn crash_and_recover() {
    use tokio::io::AsyncBufReadExt as _;
    let directory = tempfile::tempdir().unwrap();
    let command = |action: &str| {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "--ignored",
                "tests::race_recovery::race_recovery_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASH_RACE_RECOVERY_ACTION", action)
            .env("LASH_RACE_RECOVERY_DIRECTORY", directory.path())
            .kill_on_drop(true);
        command
    };
    let mut crashed = command("crash")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(crashed.stdout.take().unwrap()).lines();
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("the worker reaches the gate");
            if line.ends_with("crash_ready") {
                break;
            }
        }
    })
    .await
    .expect("the worker reaches the gate in time");
    crashed.kill().await.unwrap();
    assert!(!crashed.wait().await.unwrap().success());
    let recovered = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        command("recover").output(),
    )
    .await
    .expect("the recovering worker finishes in time")
    .unwrap();
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    assert!(
        recovered.status.success() && stdout.contains("recovered"),
        "stdout: {stdout}; stderr: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
}
