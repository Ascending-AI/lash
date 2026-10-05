//! L06/C04: a race loser's body keeps progressing while the program waits
//! on a non-Run effect.

use lash_sansio::sync::MutexExt as _;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PLUGIN: &str = "l06";
/// Beyond the double's one-second auto-advance horizon, inside its inactivity
/// timeout, so only the test's explicit advance fires the sleep.
const SLEEP_MS: u64 = 30_000;

#[derive(Default)]
struct Observed {
    /// Whether the loser reached its midpoint while the program slept.
    progressed: Option<bool>,
    /// Whether the program's sleep was still pending at that moment.
    sleep_pending: Option<bool>,
}

struct Tools {
    entries: Arc<Mutex<Vec<String>>>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    midpoint: tokio::sync::mpsc::UnboundedSender<()>,
    gate: Arc<tokio::sync::Notify>,
    /// Whether the loser waits for the test's gate before its midpoint.
    gated: bool,
    /// Whether the loser then stays unfinished.
    park: bool,
}

fn definition() -> crate::ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    crate::ToolDefinition::raw(
        "l06:run",
        "run",
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({"type":"string"}),
    )
    .unwrap()
    .with_tool_binding(lash_sansio::ToolBinding::new([PLUGIN], "run"))
}

#[async_trait::async_trait]
impl crate::ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let symbol = call.args["symbol"].as_str().unwrap();
        self.entries.lock_recover().push(symbol.to_owned());
        if symbol == "L" {
            let _ = self.entered.send(());
            if self.gated {
                self.gate.notified().await;
            }
            let _ = self.midpoint.send(());
            if self.park {
                std::future::pending::<()>().await;
            }
        }
        crate::ToolOutcome::ok(json!(symbol)).into()
    }
}

/// Poll the double's timers until the program's durable sleep exists and
/// return when it fires. No retry exists in this test; "sleep" names only
/// the program's timer.
async fn sleep_timer_fire_at(
    server: &lash_restate_test::RestateTestServer,
) -> Result<u64, tokio::time::error::Elapsed> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(timer) = server.timers().iter().find(|timer| timer.kind == "sleep") {
                break timer.fire_at_ms;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

async fn loser_beside_sleep(crash: bool) {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateOutcome, ToolAggregateRequest,
    };

    let seed = if crash { 0x497801 } else { 0x4978 };
    let double =
        crate::support::kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let entries = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::new(Mutex::new(Observed::default()));
    let crashes = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    // The fresh owner waits on the test's gate; a redriven attempt must not,
    // the gate fired once in the owner that died.
    let attempt = |redrive: bool| -> lash_restate_test::HandlerAttempt {
        let backend = backend.clone();
        let server = double.server().clone();
        let entries = entries.clone();
        let observed = observed.clone();
        let crashes = crashes.clone();
        let gate = gate.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let server = server.clone();
            let entries = entries.clone();
            let observed = observed.clone();
            let crashes = crashes.clone();
            let gate = gate.clone();
            Box::pin(async move {
                let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
                let (midpoint, mut midpoint_rx) = tokio::sync::mpsc::unbounded_channel();
                let factory = crate::plugin::StaticPluginFactory::new(
                    crate::plugin::PluginDeclaration::initial(PLUGIN),
                    crate::PluginSpec::new().with_tool_provider(Arc::new(Tools {
                        entries: entries.clone(),
                        entered,
                        midpoint,
                        gate: gate.clone(),
                        gated: !redrive,
                        park: crash && !redrive,
                    })),
                );
                let mut factories = crate::testing::test_standard_protocol_factories();
                factories.push(Arc::new(factory));
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id("loser-progress-session")
                    .borrowed_effect_controller(scoped)
                    .plugin_factories(factories)
                    .build()
                    .into_runtime();
                let grant = crate::ToolExecutionGrant::from_definition(
                    crate::plugin::PluginRevision::new(
                        PLUGIN,
                        crate::plugin::BehaviorRevision::ONE,
                    ),
                    definition(),
                );
                let leaves = ["L", "W"]
                    .into_iter()
                    .map(|symbol| {
                        ToolAggregateLeaf::Tool(
                            crate::session::ToolInvocation::new(
                                crate::ToolCallId::fixture(symbol),
                                crate::ToolId::new("l06:run"),
                                json!({"symbol": symbol}),
                            )
                            .with_execution_grant(grant.clone()),
                        )
                    })
                    .collect();
                let drive = context.drive_tool_run(None, |context| async move {
                    let outcome = context
                        .call_tool_aggregate(ToolAggregateRequest {
                            leaves,
                            consumer: ToolAggregateConsumer::Race,
                            settled_value_after: None,
                            command: crate::CommandReplayKey::new("loser-progress-race"),
                        })
                        .await;
                    assert!(matches!(
                        outcome,
                        ToolAggregateOutcome::Selected { leaf: 1, .. }
                    ));
                    context
                        .sleep_command(
                            &crate::CommandReplayKey::new("loser-progress-sleep"),
                            crate::SleepSpec::For {
                                duration_ms: SLEEP_MS,
                            },
                        )
                        .await
                        .unwrap();
                    context.close_opener_groups().await.unwrap();
                });
                let observe = async {
                    if !redrive {
                        let entered =
                            tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
                                .await
                                .map(|entry| entry.is_some())
                                .unwrap_or(false);
                        let sleep_at = sleep_timer_fire_at(&server).await.ok();
                        gate.notify_one();
                        let progressed = entered
                            && tokio::time::timeout(Duration::from_secs(2), midpoint_rx.recv())
                                .await
                                .map(|entry| entry.is_some())
                                .unwrap_or(false);
                        let sleep_pending =
                            server.timers().iter().any(|timer| timer.kind == "sleep");
                        *observed.lock_recover() = Observed {
                            progressed: Some(progressed),
                            sleep_pending: Some(sleep_pending),
                        };
                        if crash {
                            crashes.fetch_add(1, Ordering::SeqCst);
                            panic!(
                                "the owner dies while loser L is unfinished and the program sleeps"
                            );
                        }
                        if let Some(sleep_at) = sleep_at {
                            server.advance_to(sleep_at);
                        }
                        return;
                    }
                    // The redriven program re-registers its sleep; advance it.
                    let sleep_at = sleep_timer_fire_at(&server)
                        .await
                        .expect("the redriven program re-registers its sleep");
                    server.advance_to(sleep_at);
                };
                let (result, ()) = tokio::join!(drive, observe);
                result.unwrap();
                assert!(!context.has_nested_effect_error());
            })
        })
    };
    if crash {
        tokio::time::timeout(
            Duration::from_secs(10),
            double.run_crashed_then_redriven(
                crate::AdmittedScope::turn("loser-progress-session", "test-turn"),
                attempt(false),
                attempt(true),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            double.run_in_handler(
                crate::AdmittedScope::turn("loser-progress-session", "test-turn"),
                attempt(false),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
    let observed = observed.lock_recover();
    assert_eq!(
        observed.progressed,
        Some(true),
        "L06/C04: a race loser's body progresses while the program waits on its sleep"
    );
    assert_eq!(
        observed.sleep_pending,
        Some(true),
        "the loser's midpoint preceded the program's sleep"
    );
    let entries = entries.lock_recover();
    assert_eq!(
        entries.iter().filter(|symbol| *symbol == "W").count(),
        1,
        "the winner's body ran exactly once"
    );
    let expected_losers = if crash { 2 } else { 1 };
    assert_eq!(
        entries.iter().filter(|symbol| *symbol == "L").count(),
        expected_losers,
        "the loser's body ran once, plus once more redelivered after the crash"
    );
    if crash {
        assert_eq!(
            crashes.load(Ordering::SeqCst),
            1,
            "exactly one owner died and its replay completed"
        );
    }
}

#[tokio::test]
async fn l06_race_loser_body_progresses_while_the_program_sleeps() {
    loser_beside_sleep(false).await;
}

#[tokio::test]
async fn l06_cold_replay_with_a_progressing_loser_never_awaits_its_unfinished_x() {
    loser_beside_sleep(true).await;
}
