//! L19 Q5: cancelling unrecorded A preserves durable B, including overlapping keys.

use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use lash_core_store::session_state::SessionPluginStateSource as _;
use lash_sansio::sync::MutexExt as _;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// Delay only the live notification: reads and writes retain the real durable
// authority. An accepted gate must arbitrate A even if its watcher is late.
struct DelayedWatch(Arc<dyn crate::EffectHost>);

#[async_trait::async_trait]
impl crate::AwaitEventResolver for DelayedWatch {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.0.await_event_authority_binding_id()
    }
    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.0.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.0.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.0.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        _: &crate::AwaitEventKey,
        _: CancellationToken,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl crate::EffectHost for DelayedWatch {
    fn turn_control_binding_id(&self) -> String {
        self.0.turn_control_binding_id()
    }
    fn scoped<'run>(
        &'run self,
        admitted: crate::AdmittedScope,
    ) -> Result<crate::ScopedEffectController<'run>, crate::RuntimeError> {
        self.0.scoped(admitted)
    }
    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }
    async fn journal_replay(
        &self,
        identity: &crate::runtime::EffectJournalIdentity,
    ) -> Result<crate::JournalReplay, crate::RuntimeError> {
        self.0.journal_replay(identity).await
    }
}

const PLUGIN: &str = "q5";

struct Tools {
    entered: tokio::sync::mpsc::UnboundedSender<CancellationToken>,
    release: Arc<tokio::sync::Notify>,
}

fn definition() -> crate::ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    crate::ToolDefinition::raw(
        "q5:append",
        "append",
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({"type":"string"}),
    )
    .unwrap()
    .with_tool_binding(lash_sansio::ToolBinding::new([PLUGIN], "append"))
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
        if symbol == "A" {
            self.entered
                .send(
                    call.context
                        .cancellation_token()
                        .cloned()
                        .unwrap_or_default(),
                )
                .unwrap();
            self.release.notified().await;
        }
        // Even a body that returns success after its stop cannot publish A.
        crate::ToolAttemptOutcome::done_without_intents(
            crate::ToolOutcomeDone::ok(json!(symbol)).with_state(
                crate::StateCommands::new().apply(
                    call.args["key"].as_str().unwrap(),
                    "append",
                    json!(symbol),
                ),
            ),
        )
    }
}

async fn cancelled_sibling(disjoint: bool) {
    cancelled_sibling_with_delivery(disjoint, true).await;
}

async fn cancelled_sibling_with_delivery(disjoint: bool, await_delivery: bool) {
    let double =
        crate::support::kernel_double(0x4936, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let scope = crate::AdmittedScope::turn("q5-session", "test-turn");
    let handler = double.open_handler(scope).await.unwrap();
    let host = backend.effect_host();
    let resolver = host.await_event_resolver();
    let control = Arc::new(
        crate::runtime::turn_control::ActiveTurnControl::new(
            resolver,
            crate::TurnAddress::new("q5-session", "test-turn"),
        )
        .await
        .unwrap(),
    );
    let (entered, mut body) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let reductions = Arc::new(Mutex::new(Vec::new()));
    let reduced = reductions.clone();
    let factory = crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial(PLUGIN),
        crate::PluginSpec::new()
            .with_state_reducer(
                "append",
                Arc::new(move |input| {
                    reduced.lock_recover().push(input.input.clone());
                    Ok(Some(json!(format!(
                        "{}{}",
                        input
                            .current
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                        input.input.as_str().unwrap()
                    ))))
                }),
            )
            .with_tool_provider(Arc::new(Tools {
                entered,
                release: release.clone(),
            })),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(factory));
    let cancel_host: Arc<dyn crate::EffectHost> = if await_delivery {
        backend.effect_host()
    } else {
        Arc::new(DelayedWatch(backend.effect_host()))
    };
    let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
        .session_id("q5-session")
        .borrowed_effect_controller(handler.scoped())
        .plugin_factories(factories)
        .build()
        .into_runtime()
        .with_recorded_turn_cancel(
            false,
            control.clone(),
            cancel_host,
            CancellationToken::new(),
        );
    let plugins = context.dispatch().plugins.clone();
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new(PLUGIN, crate::plugin::BehaviorRevision::ONE),
        definition(),
    );
    let calls = [
        ("A", if disjoint { "a" } else { "value" }),
        ("B", if disjoint { "b" } else { "value" }),
    ]
    .into_iter()
    .map(|(symbol, key)| {
        crate::session::ToolInvocation::new(
            crate::ToolCallId::fixture(symbol),
            crate::ToolId::new("q5:append"),
            json!({"symbol":symbol,"key":key}),
        )
        .with_execution_grant(grant.clone())
    })
    .collect();
    let run = context.drive_tool_run(None, |context| async move {
        let replies = context.call_tool_batch(calls).await;
        assert!(!context.has_nested_effect_error());
        context.close_tool_run().await.unwrap();
        replies
    });
    let cancel = async {
        let stop = tokio::time::timeout(Duration::from_secs(2), body.recv())
            .await
            .expect("A enters its native inline body")
            .unwrap();
        // Read the double's independent journal, rather than a reducer or
        // presentation callback: B's final decision must already be durable.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let durable = double.server().invocations().iter().any(|view| {
                    double.server().journal(&view.id).unwrap().iter().any(|entry| {
                        let Some(Ok(bytes)) = entry.run_completion() else { return false; };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false; };
                        value.get("record").and_then(|record| serde_json::from_value::<crate::tool_run::RunRecord>(record.clone()).ok()).is_some_and(|record| record.events.iter().any(|event| matches!(event, crate::tool_run::RunEvent::Decided { call_id, decision: crate::tool_run::CallDecision::Final { .. }, .. } if *call_id == crate::ToolCallId::fixture("B"))))
                    })
                });
                if durable { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("B's final decision is durable while A is unrecorded");
        control
            .request_local_stop(resolver, crate::TurnCancelMode::Immediate, None)
            .await
            .unwrap();
        let observed = if await_delivery {
            tokio::time::timeout(Duration::from_secs(1), stop.cancelled())
                .await
                .is_ok()
        } else {
            // The durable gate acceptance precedes the body ACK; local watch
            // delivery must not decide whether that result may publish state.
            true
        };
        // Release also on the unfixed side, so the red measures leaked state
        // instead of leaving a live body or handler behind.
        release.notify_one();
        observed
    };
    let (replies, observed) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
            .await
            .unwrap();
    let replies = replies.unwrap();
    let state = plugins.capture_plugin_state().unwrap();
    drop(context);
    handler.close().await.unwrap();
    assert_eq!(reductions.lock_recover().as_slice(), [json!("B")]);
    let namespace = &state.plugins[PLUGIN];
    assert_eq!(
        namespace.values[if disjoint { "b" } else { "value" }],
        json!("B")
    );
    assert!(!namespace.values.contains_key("a"));
    assert_eq!(namespace.publication.receipts.len(), 1);
    assert!(observed, "native A never observed its durable cancellation");
    assert!(!replies.replies[0].output.is_success());
    assert!(replies.replies[1].output.is_success());
}

#[tokio::test]
async fn l19_native_inline_cancel_retains_only_the_same_key_durable_sibling() {
    cancelled_sibling(false).await;
}

#[tokio::test]
async fn l19_native_inline_cancel_retains_only_the_disjoint_key_durable_sibling() {
    cancelled_sibling(true).await;
}

struct RetryingTools(Arc<Mutex<Vec<(String, u32)>>>);

fn retry_definition() -> crate::ToolDefinition {
    definition().with_execution_policy(crate::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
        30_000,
        30_000,
    ))
}

#[async_trait::async_trait]
impl crate::ToolProvider for RetryingTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![retry_definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(retry_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.0.lock_recover().push((
            call.args["symbol"].as_str().unwrap().into(),
            call.context.attempt_number(),
        ));
        if call.context.attempt_number() == 1 {
            crate::ToolOutcome::failure_with_delay(
                crate::ToolFailureClass::External,
                "retry-first",
                "reported first-attempt failure",
                Some(30_000),
            )
            .into()
        } else {
            crate::ToolOutcome::ok(json!("retried")).into()
        }
    }
}

/// L03: the durable cancel gate, not a handler's live flag, owns the retry
/// decision after the original owner dies with its backoff still pending.
#[tokio::test]
async fn l03_accepted_cancel_survives_crash_before_retry_wake_without_second_bodies() {
    let double =
        crate::support::kernel_double(0x493207, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let session = crate::SessionId::fixture("retry-cancel-session");
    let catalog = backend.session_store_factory();
    crate::SessionCatalogStore::admit_session(
        catalog.as_ref(),
        &crate::testing::store_fixtures::root_session_request(&session),
    )
    .await
    .unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let backend = backend.clone();
        let server = double.server().clone();
        let bodies = bodies.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let server = server.clone();
            let bodies = bodies.clone();
            Box::pin(async move {
                let host = backend.effect_host();
                let control = Arc::new(
                    crate::runtime::turn_control::ActiveTurnControl::new(
                        host.await_event_resolver(),
                        crate::TurnAddress::new("retry-cancel-session", "retry-cancel-turn"),
                    )
                    .await
                    .unwrap(),
                );
                let factory = crate::plugin::StaticPluginFactory::new(
                    crate::plugin::PluginDeclaration::initial(PLUGIN),
                    crate::PluginSpec::new().with_tool_provider(Arc::new(RetryingTools(bodies))),
                );
                let mut factories = crate::testing::test_standard_protocol_factories();
                factories.push(Arc::new(factory));
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id("retry-cancel-session")
                    .borrowed_effect_controller(scoped)
                    .plugin_factories(factories)
                    .build()
                    .into_runtime()
                    .with_recorded_turn_cancel(
                        false,
                        control,
                        host.clone(),
                        CancellationToken::new(),
                    );
                let grant = crate::ToolExecutionGrant::from_definition(
                    crate::plugin::PluginRevision::new(
                        PLUGIN,
                        crate::plugin::BehaviorRevision::ONE,
                    ),
                    retry_definition(),
                );
                let calls = ["A", "B"]
                    .into_iter()
                    .map(|symbol| {
                        crate::session::ToolInvocation::new(
                            crate::ToolCallId::fixture(symbol),
                            crate::ToolId::new("q5:append"),
                            json!({"symbol":symbol}),
                        )
                        .with_execution_grant(grant.clone())
                    })
                    .collect();
                let drive = context.drive_tool_run(None, |context| async move {
                    let replies = context.call_tool_batch(calls).await;
                    context.close_tool_run().await.unwrap();
                    replies
                });
                let cancel_or_wake = async {
                    let wake_at = loop {
                        let sleeps = server
                            .timers()
                            .into_iter()
                            .filter(|timer| timer.kind == "sleep")
                            .collect::<Vec<_>>();
                        if sleeps.len() == 2 {
                            break sleeps.iter().map(|timer| timer.fire_at_ms).max().unwrap();
                        }
                        tokio::task::yield_now().await;
                    };
                    if crash {
                        let driver = crate::TurnWorkDriver::for_catalog(
                            host,
                            backend.session_store_factory(),
                        );
                        let receipt = driver
                            .request_cancel(crate::TurnCancelRequest::new(
                                crate::TurnAddress::new(
                                    "retry-cancel-session",
                                    "retry-cancel-turn",
                                ),
                                "cancel-before-owner-loss",
                                None,
                            ))
                            .await
                            .unwrap();
                        assert!(matches!(
                            receipt.outcome,
                            crate::TurnCancelOutcome::Requested(_)
                        ));
                        panic!(
                            "owner dies after accepted cancel with both durable retries pending"
                        );
                    }
                    server.advance_to(wake_at);
                };
                let (replies, ()) = tokio::join!(drive, cancel_or_wake);
                let replies = replies.unwrap();
                assert_eq!(replies.replies.len(), 2);
                assert!(replies.replies.iter().all(|reply| matches!(
                    reply.output.outcome,
                    crate::ToolCallOutcome::Cancelled(_)
                )));
                assert!(!context.has_nested_effect_error());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        double.run_crashed_then_redriven(
            crate::AdmittedScope::turn("retry-cancel-session", "retry-cancel-turn"),
            attempt(true),
            attempt(false),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let mut actual = bodies.lock_recover().clone();
    actual.sort();
    assert_eq!(
        actual,
        [("A".into(), 1), ("B".into(), 1)],
        "accepted cancellation started a retry body after cold recovery"
    );
    let events = double
        .server()
        .invocations()
        .into_iter()
        .flat_map(|invocation| {
            double
                .server()
                .journal(&invocation.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| entry.run_completion().and_then(Result::ok))
                .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .filter_map(|value| value.get("record").cloned())
                .filter_map(|record| {
                    serde_json::from_value::<crate::tool_run::RunRecord>(record).ok()
                })
                .flat_map(|record| record.events)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                crate::tool_run::RunEvent::Decided {
                    decision: crate::tool_run::CallDecision::Cancelled,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        crate::tool_run::RunEvent::RetryScheduled { .. }
            | crate::tool_run::RunEvent::DeclarationsIssued { .. }
    )));
}

#[tokio::test]
async fn l03_native_cancel_accepted_before_inline_ack_discards_the_unrecorded_sibling() {
    cancelled_sibling_with_delivery(false, false).await;
}

/// L03: a cancelled inline loser whose X was not acknowledged must inherit
/// the accepted stop on cold replay, while its durable winner stays final.
#[tokio::test]
async fn l03_native_cancel_replays_before_the_inline_loser_ack() {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateOutcome, ToolAggregateRequest,
    };
    use lash_restate_test::{CrashCount, CrashPoint, CrashRule};

    let double =
        crate::support::kernel_double(0x496603, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    double
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some(
                lash_restate_test::JournalStepKind::RunAttempt.journal_name(&format!(
                    "lash:run:{}:attempt:1",
                    crate::ToolCallId::fixture("A")
                )),
            ),
        }));
    let crashes = CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    let cold_prefix = Arc::new(Mutex::new(None));
    let (entered, mut stops) = tokio::sync::mpsc::unbounded_channel();
    let reductions = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let server = double.server().clone();
        let crashes = crashes.clone();
        let cold_prefix = cold_prefix.clone();
        let backend = backend.clone();
        let reductions = reductions.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let entered = entered.clone();
            let reductions = reductions.clone();
            let server = server.clone();
            let crashes = crashes.clone();
            let cold_prefix = cold_prefix.clone();
            Box::pin(async move {
                if crashes.get() > 0 && cold_prefix.lock_recover().is_none() {
                    let entries = server
                        .invocations()
                        .iter()
                        .flat_map(|view| server.journal(&view.id).unwrap_or_default())
                        .collect::<Vec<_>>();
                    let cancel_durable = entries.iter().filter_map(|entry| entry.run_completion().and_then(Result::ok))
                        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .filter_map(|value| value.get("record").cloned())
                        .filter_map(|record| serde_json::from_value::<crate::tool_run::RunRecord>(record).ok())
                        .any(|record| record.events.iter().any(|event| matches!(event,
                            crate::tool_run::RunEvent::CancelDischarged { call_id } if *call_id == crate::ToolCallId::fixture("A")
                        )));
                    let loser_durable = entries
                        .iter()
                        .filter_map(|entry| entry.run_completion().and_then(Result::ok))
                        .filter_map(|bytes| {
                            serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                        })
                        .any(|value| {
                            value.get("call_id") == Some(&json!(crate::ToolCallId::fixture("A")))
                        });
                    *cold_prefix.lock_recover() = Some((cancel_durable, loser_durable));
                }
                let reduced = reductions.clone();
                let factory = crate::plugin::StaticPluginFactory::new(
                    crate::plugin::PluginDeclaration::initial(PLUGIN),
                    crate::PluginSpec::new()
                        .with_state_reducer(
                            "append",
                            Arc::new(move |input| {
                                reduced.lock_recover().push(input.input.clone());
                                Ok(Some(input.input.clone()))
                            }),
                        )
                        .with_tool_provider(Arc::new(Tools {
                            entered,
                            // The loser never answers: Closing must stop its native body.
                            release: Arc::new(tokio::sync::Notify::new()),
                        })),
                );
                let mut factories = crate::testing::test_standard_protocol_factories();
                factories.push(Arc::new(factory));
                let parent_stop = CancellationToken::new();
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id("inline-close-session")
                    .borrowed_effect_controller(scoped)
                    .plugin_factories(factories)
                    .build()
                    .into_runtime()
                    .with_cancellation_token(parent_stop.clone());
                let grant = crate::ToolExecutionGrant::from_definition(
                    crate::plugin::PluginRevision::new(
                        PLUGIN,
                        crate::plugin::BehaviorRevision::ONE,
                    ),
                    definition(),
                );
                let leaves = ["A", "B"]
                    .into_iter()
                    .map(|symbol| {
                        ToolAggregateLeaf::Tool(
                            crate::session::ToolInvocation::new(
                                crate::ToolCallId::fixture(symbol),
                                crate::ToolId::new("q5:append"),
                                json!({"symbol":symbol,"key":"value"}),
                            )
                            .with_execution_grant(grant.clone()),
                        )
                    })
                    .collect();
                context
                    .drive_tool_run(None, |context| async move {
                        let outcome = context
                            .call_tool_aggregate(ToolAggregateRequest {
                                leaves,
                                consumer: ToolAggregateConsumer::Race,
                                settled_value_after: None,
                                command: crate::CommandReplayKey::new("inline-close-race"),
                            })
                            .await;
                        assert!(matches!(
                            outcome,
                            ToolAggregateOutcome::Selected { leaf: 1, .. }
                        ));
                        context.close_tool_run().await.unwrap();
                    })
                    .await
                    .unwrap();
                assert!(
                    !parent_stop.is_cancelled(),
                    "a loser's stop must not cancel its siblings"
                );
                assert!(!context.has_nested_effect_error());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        double.run_in_handler(
            crate::AdmittedScope::turn("inline-close-session", "inline-close-turn"),
            attempt,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        *cold_prefix.lock_recover(),
        Some((true, false)),
        "cold replay starts after accepted cancel and before the loser's X ACK"
    );
    assert_eq!(
        crashes.get(),
        1,
        "the owner crashes before the loser's X becomes durable"
    );
    let mut observed = Vec::new();
    while let Ok(stop) = stops.try_recv() {
        observed.push(stop);
    }
    assert_eq!(
        observed.len(),
        1,
        "the accepted cancel prevents the cold owner from invoking the loser again"
    );
    assert!(observed.iter().all(CancellationToken::is_cancelled));
    assert_eq!(reductions.lock_recover().as_slice(), [json!("B")]);
    let events = double
        .server()
        .invocations()
        .into_iter()
        .flat_map(|invocation| {
            double
                .server()
                .journal(&invocation.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| entry.run_completion().and_then(Result::ok))
                .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .filter_map(|value| value.get("record").cloned())
                .filter_map(|record| {
                    serde_json::from_value::<crate::tool_run::RunRecord>(record).ok()
                })
                .flat_map(|record| record.events)
        })
        .collect::<Vec<_>>();
    assert_eq!(events.iter().filter(|event| matches!(event,
        crate::tool_run::RunEvent::CancelDischarged { call_id } if *call_id == crate::ToolCallId::fixture("A")
    )).count(), 1, "the cold owner reuses the accepted cancellation");
    assert!(events.iter().any(|event| matches!(event,
        crate::tool_run::RunEvent::Decided { call_id, decision: crate::tool_run::CallDecision::Cancelled, .. }
        if *call_id == crate::ToolCallId::fixture("A")
    )));
}

/// The recorded X completions of `symbol`, with their canonical material.
fn attempt_material(server: &lash_restate_test::RestateTestServer, symbol: &str) -> String {
    server
        .invocations()
        .iter()
        .flat_map(|view| server.journal(&view.id).unwrap_or_default())
        .filter_map(|entry| entry.run_completion().and_then(Result::ok))
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter(|value| value.get("call_id") == Some(&json!(crate::ToolCallId::fixture(symbol))))
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_events(server: &lash_restate_test::RestateTestServer) -> Vec<crate::tool_run::RunEvent> {
    server
        .invocations()
        .into_iter()
        .flat_map(|invocation| server.journal(&invocation.id).unwrap_or_default())
        .filter_map(|entry| entry.run_completion().and_then(Result::ok))
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter_map(|value| value.get("record").cloned())
        .filter_map(|record| serde_json::from_value::<crate::tool_run::RunRecord>(record).ok())
        .flat_map(|record| record.events)
        .collect()
}

/// A race loser A that either watches its stop and finishes its own work
/// after it, or never answers; winner B answers once A's body is live.
struct LoserTools {
    cooperative: bool,
    entered: Arc<tokio::sync::Notify>,
    settled: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for LoserTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.args["symbol"] == "B" {
            self.entered.notified().await;
            return crate::ToolOutcome::ok(json!("B")).into();
        }
        let stop = call.context.cancellation_token().cloned().unwrap();
        self.entered.notify_one();
        if !self.cooperative {
            std::future::pending::<()>().await;
        }
        stop.cancelled().await;
        // Work the body still owes after its stop, as a nested owned run's
        // settlement does: dropping the body here loses it.
        tokio::task::yield_now().await;
        self.settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        crate::ToolOutcome::cancelled("A observed its stop").into()
    }
}

async fn closing_race(cooperative: bool) -> (lash_restate_test::RestateTestBackend, bool) {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateOutcome, ToolAggregateRequest,
    };
    let double =
        crate::support::kernel_double(0x4979, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let scope = crate::AdmittedScope::turn("closing-session", "closing-turn");
    let handler = double.open_handler(scope).await.unwrap();
    let host = backend.effect_host();
    let control = Arc::new(
        crate::runtime::turn_control::ActiveTurnControl::new(
            host.await_event_resolver(),
            crate::TurnAddress::new("closing-session", "closing-turn"),
        )
        .await
        .unwrap(),
    );
    let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial(PLUGIN),
        crate::PluginSpec::new().with_tool_provider(Arc::new(LoserTools {
            cooperative,
            entered: Arc::new(tokio::sync::Notify::new()),
            settled: settled.clone(),
        })),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(factory));
    // The turn has a live gate that never stops: only Closing stops A.
    let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
        .session_id("closing-session")
        .borrowed_effect_controller(handler.scoped())
        .plugin_factories(factories)
        .build()
        .into_runtime()
        .with_recorded_turn_cancel(false, control, host, CancellationToken::new());
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new(PLUGIN, crate::plugin::BehaviorRevision::ONE),
        definition(),
    );
    let leaves = ["A", "B"]
        .into_iter()
        .map(|symbol| {
            ToolAggregateLeaf::Tool(
                crate::session::ToolInvocation::new(
                    crate::ToolCallId::fixture(symbol),
                    crate::ToolId::new("q5:append"),
                    json!({"symbol":symbol,"key":"value"}),
                )
                .with_execution_grant(grant.clone()),
            )
        })
        .collect();
    tokio::time::timeout(
        Duration::from_secs(5),
        context.drive_tool_run(None, |context| async move {
            let outcome = context
                .call_tool_aggregate(ToolAggregateRequest {
                    leaves,
                    consumer: ToolAggregateConsumer::Race,
                    settled_value_after: None,
                    command: crate::CommandReplayKey::new("closing-race"),
                })
                .await;
            assert!(matches!(
                outcome,
                ToolAggregateOutcome::Selected { leaf: 1, .. }
            ));
            context.close_tool_run().await.unwrap();
        }),
    )
    .await
    .expect("Closing settles the race loser")
    .unwrap();
    assert!(!context.has_nested_effect_error());
    drop(context);
    handler.close().await.unwrap();
    let events = run_events(double.server());
    assert!(events.iter().any(|event| matches!(event,
        crate::tool_run::RunEvent::Decided { call_id, decision: crate::tool_run::CallDecision::Cancelled, .. }
        if *call_id == crate::ToolCallId::fixture("A")
    )));
    (double, settled.load(std::sync::atomic::Ordering::SeqCst))
}

/// L06: Closing stops a loser cooperatively. Its body observes the stop,
/// finishes the work it owes and records its own cancellation, attributed
/// to the Run's Closing rather than to a turn that never stopped.
#[tokio::test]
async fn l06_closing_lets_a_cooperative_loser_record_its_own_cancellation() {
    let (double, settled) = closing_race(true).await;
    assert!(settled, "Closing dropped the loser's body after its stop");
    let recorded = attempt_material(double.server(), "A");
    assert!(
        recorded.contains("A observed its stop"),
        "X records the body's own cancellation: {recorded}"
    );
    assert!(recorded.contains("run_closing"), "{recorded}");
    assert!(!recorded.contains("turn_stopped"), "{recorded}");
}

/// L06: a loser that never answers its stop is dropped only after the
/// bounded grace, and its runtime cancellation names Closing as its cause.
#[tokio::test]
async fn l06_closing_records_its_own_cause_for_a_loser_it_drops() {
    let (double, settled) = closing_race(false).await;
    assert!(!settled);
    let recorded = attempt_material(double.server(), "A");
    assert!(
        recorded.contains("the inline attempt stopped before its body completed"),
        "{recorded}"
    );
    assert!(recorded.contains("run_closing"), "{recorded}");
    assert!(!recorded.contains("turn_stopped"), "{recorded}");
}

/// A fails retryably on its first attempt; B answers once A's backoff is a
/// durable SDK sleep.
struct BackoffTools {
    bodies: Arc<Mutex<Vec<(String, u32)>>>,
    server: lash_restate_test::RestateTestServer,
}

#[async_trait::async_trait]
impl crate::ToolProvider for BackoffTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![retry_definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(retry_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let symbol = call.args["symbol"].as_str().unwrap().to_owned();
        self.bodies
            .lock_recover()
            .push((symbol.clone(), call.context.attempt_number()));
        if symbol == "B" {
            while !self
                .server
                .timers()
                .iter()
                .any(|timer| timer.kind == "sleep")
            {
                tokio::task::yield_now().await;
            }
            return crate::ToolOutcome::ok(json!("B")).into();
        }
        crate::ToolOutcome::failure_with_delay(
            crate::ToolFailureClass::External,
            "retry-first",
            "reported first-attempt failure",
            Some(30_000),
        )
        .into()
    }
}

/// L17: Closing cuts a losing call's retry backoff at once and decides it
/// cancelled; it never waits out the sleep or starts the next attempt.
#[tokio::test]
async fn l17_closing_cuts_a_loser_in_retry_backoff_promptly() {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateOutcome, ToolAggregateRequest,
    };
    let double =
        crate::support::kernel_double(0x497903, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let scope = crate::AdmittedScope::turn("backoff-session", "backoff-turn");
    let handler = double.open_handler(scope).await.unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let factory = crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial(PLUGIN),
        crate::PluginSpec::new().with_tool_provider(Arc::new(BackoffTools {
            bodies: bodies.clone(),
            server: double.server().clone(),
        })),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(factory));
    let parent_stop = CancellationToken::new();
    let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
        .session_id("backoff-session")
        .borrowed_effect_controller(handler.scoped())
        .plugin_factories(factories)
        .build()
        .into_runtime()
        .with_cancellation_token(parent_stop.clone());
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new(PLUGIN, crate::plugin::BehaviorRevision::ONE),
        retry_definition(),
    );
    let leaves = ["A", "B"]
        .into_iter()
        .map(|symbol| {
            ToolAggregateLeaf::Tool(
                crate::session::ToolInvocation::new(
                    crate::ToolCallId::fixture(symbol),
                    crate::ToolId::new("q5:append"),
                    json!({"symbol":symbol}),
                )
                .with_execution_grant(grant.clone()),
            )
        })
        .collect();
    tokio::time::timeout(
        Duration::from_secs(5),
        context.drive_tool_run(None, |context| async move {
            let outcome = context
                .call_tool_aggregate(ToolAggregateRequest {
                    leaves,
                    consumer: ToolAggregateConsumer::Race,
                    settled_value_after: None,
                    command: crate::CommandReplayKey::new("backoff-race"),
                })
                .await;
            assert!(matches!(
                outcome,
                ToolAggregateOutcome::Selected { leaf: 1, .. }
            ));
            context.close_tool_run().await.unwrap();
        }),
    )
    .await
    .expect("Closing does not wait out the loser's 30-second backoff")
    .unwrap();
    assert!(!parent_stop.is_cancelled());
    assert!(!context.has_nested_effect_error());
    drop(context);
    handler.close().await.unwrap();
    let mut actual = bodies.lock_recover().clone();
    actual.sort();
    assert_eq!(actual, [("A".into(), 1), ("B".into(), 1)]);
    let events = run_events(double.server());
    assert!(events.iter().any(|event| matches!(event,
        crate::tool_run::RunEvent::Decided { call_id, decision: crate::tool_run::CallDecision::Cancelled, .. }
        if *call_id == crate::ToolCallId::fixture("A")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, crate::tool_run::RunEvent::RetryScheduled { .. }))
    );
}
