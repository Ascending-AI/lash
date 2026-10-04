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
        context.close_opener_groups().await.unwrap();
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
    definition().with_retry_policy(crate::ToolRetryPolicy::safe(2, 30_000, 30_000))
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
            crate::ToolOutcome::retryable_failure(
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
                    context.close_opener_groups().await.unwrap();
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
