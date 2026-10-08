//! The tool hook composition (ADR 0128): transforms chain once in recorded
//! order, every check inspects one immutable value, and checks reduce by
//! strength, then plugin id, then callback key, in any registration order.
use super::*;
use crate::plugin::{
    AfterToolContributions, AfterToolDecision, BeforeToolDecision, CachedToolSuccess,
    PluginFactory, PluginSpec, ToolHookOccurrence, ToolResultCandidate,
};
use crate::tool_dispatch::{
    ToolPreparationOutcome, finalize_tool_result_with_execution_context,
    prepare_tool_call_with_context,
};
use lash_core_execution::hook_key;
use lash_sansio::sync::MutexExt as _;

/// `beta` returns its `value` argument and counts its executions. With
/// `prepare`, the provider's preparation appends `-prepared` to the value.
#[derive(Clone, Default)]
struct CountingTools {
    executions: Arc<AtomicUsize>,
    prepare: bool,
}

#[async_trait::async_trait]
impl ToolProvider for CountingTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![named_beta_tool("beta")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        contract_from(vec![named_beta_tool("beta")], name)
    }

    async fn prepare_tool_call(
        &self,
        call: crate::ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, ToolOutcome> {
        let mut pending = call.pending;
        if self.prepare {
            let value = pending.args["value"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            pending.args = json!({ "value": format!("{value}-prepared") });
        }
        Ok(crate::PreparedToolCall::identity(call.tool_id, pending))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(call.args["value"].clone()).into()
    }
}

fn contract_from(
    definitions: Vec<crate::ToolDefinition>,
    name: &str,
) -> Option<Arc<crate::ToolContract>> {
    definitions
        .into_iter()
        .find(|tool| tool.name() == name)
        .map(|tool| Arc::new(tool.contract()))
}

fn plugin(id: &'static str, spec: PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial(id),
        spec,
    ))
}

fn session(tools: &CountingTools, mut plugins: Vec<Arc<dyn PluginFactory>>) -> Arc<PluginSession> {
    plugins.insert(
        0,
        plugin(
            "composition_tools",
            PluginSpec::new().with_tool_provider(Arc::new(tools.clone())),
        ),
    );
    crate::support::plugin_host(plugins)
        .build_session(PluginSessionRequest::creation(
            "root",
            crate::plugin::SessionAuthorityContext::ambient_fixture(),
        ))
        .expect("plugin session")
}

/// Dispatch `beta` with `value` under `plugins`: the call's attempt runs in
/// place, as a round member's does.
async fn dispatch(plugins: Arc<PluginSession>, value: &str) -> crate::ToolCallOutput {
    let context = refusing_dispatch_context(plugins).await;
    dispatch_tool_call(&context, "beta".to_string(), json!({ "value": value }))
        .await
        .record
        .output
}

/// An argument transform that appends `suffix` and counts its invocations.
fn appender(suffix: &'static str, calls: Arc<AtomicUsize>) -> crate::plugin::ToolArgsTransformHook {
    Arc::new(move |input| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let value = input.current["value"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            Ok(json!({ "value": format!("{value}{suffix}") }))
        })
    })
}

fn fixed_check(decision: BeforeToolDecision) -> crate::plugin::ToolArgsCheckHook {
    Arc::new(move |_| {
        let decision = decision.clone();
        Box::pin(async move { Ok(decision) })
    })
}

fn denial(code: &str) -> crate::ToolFailure {
    crate::ToolFailure::tool(crate::ToolFailureClass::PermissionDenied, code, code)
}

fn abort() -> BeforeToolDecision {
    BeforeToolDecision::AbortRun(crate::plugin::PluginAbort::new("stop", "the run stops"))
}

fn cached(value: &str) -> BeforeToolDecision {
    BeforeToolDecision::Cached(CachedToolSuccess::new(crate::ToolValue::untrusted_json(
        json!(value),
    )))
}

/// Every order of `items`.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for index in 0..items.len() {
        let mut rest = items.to_vec();
        let first = rest.remove(index);
        for mut tail in permutations(&rest) {
            tail.insert(0, first.clone());
            all.push(tail);
        }
    }
    all
}

/// Law 2: a check registered before a normalizer still inspects the final
/// arguments and denies the dangerous replacement, in either order.
#[tokio::test]
async fn a_check_sees_the_final_arguments_in_either_registration_order() {
    for check_first in [true, false] {
        let tools = CountingTools::default();
        let policy = plugin(
            "policy",
            PluginSpec::new().with_tool_args_check(
                hook_key!("forbid-normalized"),
                Arc::new(|input| {
                    let dangerous = input.prepared.args()["value"] == json!("x-norm");
                    let original = input.original_args["value"].clone();
                    Box::pin(async move {
                        assert_eq!(original, json!("x"), "the model's arguments stay readable");
                        Ok(if dangerous {
                            BeforeToolDecision::Deny(denial("normalized_value_forbidden"))
                        } else {
                            BeforeToolDecision::Allow
                        })
                    })
                }),
            ),
        );
        let normalizer = plugin(
            "normalizer",
            PluginSpec::new().with_tool_args_transform(
                hook_key!("normalize"),
                appender("-norm", Arc::new(AtomicUsize::new(0))),
            ),
        );
        let plugins = if check_first {
            vec![policy, normalizer]
        } else {
            vec![normalizer, policy]
        };
        let output = dispatch(session(&tools, plugins), "x").await;

        assert_eq!(
            output.value_for_projection()["code"],
            json!("normalized_value_forbidden"),
            "check first: {check_first}"
        );
        assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    }
}

/// Law 4: a cache registered before a normalizer hits on the normalized
/// call, the body runs zero times, and the cached value still passes
/// through the result phase as a `Cached` occurrence.
#[tokio::test]
async fn a_cache_before_a_normalizer_hits_on_the_normalized_call() {
    let tools = CountingTools::default();
    let occurrences = Arc::new(std::sync::Mutex::new(Vec::new()));
    let witness = Arc::clone(&occurrences);
    let cache = plugin(
        "cache",
        PluginSpec::new()
            .with_tool_args_check(
                hook_key!("lookup"),
                Arc::new(|input| {
                    let hit = input.prepared.args()["value"] == json!("x-norm");
                    Box::pin(async move {
                        Ok(if hit {
                            cached("cached")
                        } else {
                            BeforeToolDecision::Allow
                        })
                    })
                }),
            )
            .with_tool_result_check(
                hook_key!("observe"),
                Arc::new(move |input| {
                    witness
                        .lock_recover()
                        .push((input.occurrence, input.final_result.outcome.clone()));
                    Box::pin(async { Ok(AfterToolContributions::default()) })
                }),
            ),
    );
    let normalizer = plugin(
        "normalizer",
        PluginSpec::new().with_tool_args_transform(
            hook_key!("normalize"),
            appender("-norm", Arc::new(AtomicUsize::new(0))),
        ),
    );
    let output = dispatch(session(&tools, vec![cache, normalizer]), "x").await;

    assert_eq!(output.value_for_projection(), json!("cached"));
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    let occurrences = occurrences.lock_recover();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].0, ToolHookOccurrence::Cached);
}

/// Law 4: a denial or an abort beats a cache hit in every registration
/// order, and the body never runs.
#[tokio::test]
async fn a_denial_or_abort_beats_a_cache_in_every_order() {
    for restrictive in [BeforeToolDecision::Deny(denial("denied")), abort()] {
        for order in permutations(&["cache", "guard"]) {
            let tools = CountingTools::default();
            let plugins = order
                .iter()
                .map(|id| {
                    let decision = if *id == "cache" {
                        cached("cached")
                    } else {
                        restrictive.clone()
                    };
                    plugin(
                        id,
                        PluginSpec::new()
                            .with_tool_args_check(hook_key!("check"), fixed_check(decision)),
                    )
                })
                .collect();
            let output = dispatch(session(&tools, plugins), "x").await;
            assert!(!output.is_success(), "{order:?}");
            assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
        }
    }
}

/// Law 5: fixed replies reduce to the same control, result and evidence in
/// every registration order, including abort and deny in both orders and
/// two callbacks of one plugin.
#[tokio::test]
async fn fixed_replies_reduce_identically_in_every_order() {
    let replies: Vec<(&'static str, BeforeToolDecision)> = vec![
        ("allow", BeforeToolDecision::Allow),
        ("cache", cached("cached")),
        ("deny", BeforeToolDecision::Deny(denial("denied"))),
        ("abort", abort()),
    ];
    let mut outputs = Vec::new();
    for order in permutations(&replies) {
        let tools = CountingTools::default();
        let plugins = order
            .into_iter()
            .map(|(id, decision)| {
                plugin(
                    id,
                    PluginSpec::new()
                        .with_tool_args_check(hook_key!("check"), fixed_check(decision)),
                )
            })
            .collect();
        outputs.push(dispatch(session(&tools, plugins), "x").await);
    }
    let first = outputs[0].clone();
    assert!(outputs.iter().all(|output| *output == first));
    let Some(crate::ToolControl::AbortRun { code, message }) = &first.control else {
        panic!("the abort selects the Run control: {first:?}");
    };
    assert_eq!(code.namespaced(), "abort:stop");
    assert_eq!(message, "the run stops");
    assert_eq!(first.value_for_projection()["code"], json!("stop"));

    // Two denials of one plugin reduce by callback key, in either order.
    for keys in permutations(&["b-second", "a-first"]) {
        let tools = CountingTools::default();
        let mut spec = PluginSpec::new();
        for key in keys {
            let decision = BeforeToolDecision::Deny(denial(key));
            spec = spec.with_tool_args_check(
                crate::plugin::HookKey::new(key).expect("valid key"),
                fixed_check(decision),
            );
        }
        let output = dispatch(session(&tools, vec![plugin("policy", spec)]), "x").await;
        assert_eq!(output.value_for_projection()["code"], json!("a-first"));
    }
}

/// A check whose callback fails denies the call; it never allows, and the
/// other checks still run on the same call.
#[tokio::test]
async fn a_failing_check_denies_and_the_others_still_run() {
    let tools = CountingTools::default();
    let later = Arc::new(AtomicUsize::new(0));
    let witness = Arc::clone(&later);
    let output = dispatch(
        session(
            &tools,
            vec![
                plugin(
                    "broken",
                    PluginSpec::new().with_tool_args_check(
                        hook_key!("check"),
                        Arc::new(|_| {
                            Box::pin(async { Err(crate::PluginError::Invoke("broken".into())) })
                        }),
                    ),
                ),
                plugin(
                    "later",
                    PluginSpec::new().with_tool_args_check(
                        hook_key!("check"),
                        Arc::new(move |_| {
                            witness.fetch_add(1, Ordering::SeqCst);
                            Box::pin(async { Ok(BeforeToolDecision::Allow) })
                        }),
                    ),
                ),
            ],
        ),
        "x",
    )
    .await;

    assert_eq!(
        output.value_for_projection()["code"],
        json!("tool_args_check_failed")
    );
    assert_eq!(later.load(Ordering::SeqCst), 1);
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
}

/// A transform's failure fails the call: there is no valid value to go on.
#[tokio::test]
async fn a_failing_transform_fails_the_call() {
    let tools = CountingTools::default();
    let output = dispatch(
        session(
            &tools,
            vec![plugin(
                "broken",
                PluginSpec::new().with_tool_args_transform(
                    hook_key!("normalize"),
                    Arc::new(|_| {
                        Box::pin(async { Err(crate::PluginError::Invoke("broken".into())) })
                    }),
                ),
            )],
        ),
        "x",
    )
    .await;

    assert_eq!(
        output.value_for_projection()["code"],
        json!("tool_args_transform_failed")
    );
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
}

/// Keys name callbacks: a duplicate key in one plugin and seam, and a
/// response that names an unregistered stream-finished key, fail
/// registration.
#[test]
fn keyed_registrations_refuse_duplicates_and_unpaired_stream_state() {
    let check = fixed_check(BeforeToolDecision::Allow);
    let duplicate = plugin(
        "policy",
        PluginSpec::new()
            .with_tool_args_check(hook_key!("same"), Arc::clone(&check))
            .with_tool_args_check(hook_key!("same"), check),
    );
    let error = crate::support::plugin_host(vec![duplicate])
        .build_session(PluginSessionRequest::creation(
            "root",
            crate::plugin::SessionAuthorityContext::ambient_fixture(),
        ))
        .err()
        .expect("a duplicate key is refused");
    assert!(
        error
            .to_string()
            .contains("duplicate hook key `tool_args_check:same`")
    );

    let unpaired = plugin(
        "mask",
        PluginSpec::new().with_assistant_response(
            hook_key!("splice"),
            Some(hook_key!("missing")),
            Arc::new(|ctx| {
                Box::pin(async move {
                    Ok(crate::plugin::AssistantResponseTransform {
                        response: ctx.response,
                        events: Vec::new(),
                    })
                })
            }),
        ),
    );
    let error = crate::support::plugin_host(vec![unpaired])
        .build_session(PluginSessionRequest::creation(
            "root",
            crate::plugin::SessionAuthorityContext::ambient_fixture(),
        ))
        .err()
        .expect("an unpaired stream state is refused");
    assert!(
        error
            .to_string()
            .contains("assistant_stream_finished:missing")
    );
}

/// FIG-4922, Q5.7/8: a result check without a recorded decision cannot
/// publish state before settlement: a cached success and a Deferred
/// completion have no recorded body to own the state their checks propose,
/// so the result fails typed, no reducer runs, the plugin state is unchanged,
/// and a later preparation that misses the cache has nothing to apply.
#[tokio::test]
async fn result_checks_without_a_decision_record_publish_no_state_before_settlement() {
    for occurrence in [
        ToolHookOccurrence::Cached,
        ToolHookOccurrence::DeferredCompletion {
            attempt: crate::tool_run::AttemptOrdinal::FIRST,
        },
    ] {
        for verdict in [
            AfterToolDecision::Allow,
            AfterToolDecision::Deny(denial("denied")),
        ] {
            let tools = CountingTools::default();
            let reducers = Arc::new(AtomicUsize::new(0));
            let reductions = Arc::clone(&reducers);
            let admitted = Arc::new(std::sync::Mutex::new(None));
            let read_view = Arc::clone(&admitted);
            let hit = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let cache_hit = Arc::clone(&hit);
            let plugins = session(
                &tools,
                vec![plugin(
                    "cache",
                    PluginSpec::new()
                        .with_tool_args_check(
                            hook_key!("lookup"),
                            Arc::new(move |input| {
                                *read_view.lock_recover() = Some(input.prepared.clone());
                                let hit = cache_hit.load(Ordering::SeqCst);
                                Box::pin(async move {
                                    Ok(if hit && occurrence == ToolHookOccurrence::Cached {
                                        cached("cached")
                                    } else {
                                        BeforeToolDecision::Allow
                                    })
                                })
                            }),
                        )
                        .with_tool_result_check(
                            hook_key!("cache-store"),
                            Arc::new(move |_| {
                                let verdict = verdict.clone();
                                Box::pin(async move {
                                    Ok(AfterToolContributions {
                                        verdict,
                                        state: crate::StateCommands::new().apply(
                                            "value",
                                            "store",
                                            json!("cached"),
                                        ),
                                        ..Default::default()
                                    })
                                })
                            }),
                        )
                        .with_state_reducer(
                            "store",
                            Arc::new(move |input| {
                                reductions.fetch_add(1, Ordering::SeqCst);
                                Ok(Some(input.input.clone()))
                            }),
                        ),
                )],
            );
            let context = refusing_dispatch_context(Arc::clone(&plugins)).await;
            let before = serde_json::to_value(plugins.export_state()).unwrap();
            let pending = crate::sansio::PendingToolCall {
                call_id: crate::ToolCallId::fixture("c1"),
                provider_call_id: None,
                tool_name: "beta".into(),
                args: json!({"value": "x"}),
                replay: None,
            };
            let retry = pending.clone();
            let output = if occurrence == ToolHookOccurrence::Cached {
                let ToolPreparationOutcome::Completed(outcome) =
                    prepare_tool_call_with_context(&context, pending).await
                else {
                    panic!("the cache completes preparation");
                };
                outcome.record.output
            } else {
                assert!(matches!(
                    prepare_tool_call_with_context(&context, pending).await,
                    ToolPreparationOutcome::Prepared(_)
                ));
                let prepared = admitted.lock_recover().clone().unwrap();
                finalize_tool_result_with_execution_context(
                    &context,
                    &prepared,
                    occurrence,
                    ToolOutcome::ok(json!("resolved")),
                )
                .await
                .into_done_output()
                .unwrap()
            };
            // Stop before the caller's settlement. No durable decision
            // exists yet, even if a result check allowed this candidate.
            assert_eq!(
                serde_json::to_value(plugins.export_state()).unwrap(),
                before,
                "{occurrence:?} published commands before its decision was recorded"
            );
            assert!(
                !output.is_success(),
                "command-bearing checks require a decision record"
            );
            let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
                panic!("unrecorded commands fail the result");
            };
            assert_eq!(failure.code, "tool_result_check_state_unrecorded");
            assert_eq!(failure.source, crate::ToolFailureSource::Plugin);
            assert_eq!(
                failure.cause.as_deref(),
                Some(&crate::ToolFailureCause::PluginStateUnrecorded {
                    plugin: "cache".into(),
                })
            );
            let restored: crate::ToolFailure =
                serde_json::from_value(serde_json::to_value(&failure).unwrap()).unwrap();
            assert_eq!(
                restored.cause, failure.cause,
                "the host receives a typed refusal"
            );
            assert_eq!(reducers.load(Ordering::SeqCst), 0);
            assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
            hit.store(false, Ordering::SeqCst);
            assert!(
                matches!(
                    prepare_tool_call_with_context(&context, retry).await,
                    ToolPreparationOutcome::Prepared(_)
                ),
                "a repeated preparation that misses the cache leaves no command to replay"
            );
            assert_eq!(
                serde_json::to_value(plugins.export_state()).unwrap(),
                before,
                "a preparation that misses the cache applies no left-over command"
            );
            assert_eq!(reducers.load(Ordering::SeqCst), 0);
            drop(context);
        }
    }
}

/// Law 1: two unconditional argument transforms compose in registration
/// order, each invoked once; the body runs once on their result.
#[tokio::test]
async fn argument_transforms_chain_once_in_registration_order() {
    let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let tools = CountingTools::default();
    let output = dispatch(
        session(
            &tools,
            vec![
                plugin(
                    "one",
                    PluginSpec::new()
                        .with_tool_args_transform(hook_key!("append"), appender("-a", a.clone())),
                ),
                plugin(
                    "two",
                    PluginSpec::new()
                        .with_tool_args_transform(hook_key!("append"), appender("-b", b.clone())),
                ),
            ],
        ),
        "x",
    )
    .await;

    assert_eq!(output.value_for_projection(), json!("x-a-b"));
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert_eq!(b.load(Ordering::SeqCst), 1);
    assert_eq!(tools.executions.load(Ordering::SeqCst), 1);
}

/// Law 3: the provider's preparation runs before the checks, so a check
/// inspects the arguments the body executes with.
#[tokio::test]
async fn checks_inspect_the_provider_prepared_call() {
    let tools = CountingTools {
        prepare: true,
        ..CountingTools::default()
    };
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let witness = Arc::clone(&seen);
    let output = dispatch(
        session(
            &tools,
            vec![plugin(
                "policy",
                PluginSpec::new().with_tool_args_check(
                    hook_key!("witness"),
                    Arc::new(move |input| {
                        witness.lock_recover().push(input.prepared.args().clone());
                        Box::pin(async { Ok(BeforeToolDecision::Allow) })
                    }),
                ),
            )],
        ),
        "x",
    )
    .await;

    assert_eq!(
        seen.lock_recover().as_slice(),
        &[json!({ "value": "x-prepared" })]
    );
    assert_eq!(output.value_for_projection(), json!("x-prepared"));
}

/// Law 5: a reduction that displaces terminal replies publishes the winner
/// and every displaced terminal, attributed by plugin and callback key.
#[tokio::test]
async fn displaced_terminals_are_published_as_composition_evidence() {
    #[derive(Default)]
    struct RecordingSessionGraph {
        events: std::sync::Mutex<Vec<lash_trace::TraceEvent>>,
    }

    #[async_trait::async_trait]
    impl crate::plugin::SessionGraphService for RecordingSessionGraph {
        async fn emit_trace_event(
            &self,
            _context: lash_trace::TraceContext,
            event: lash_trace::TraceEvent,
        ) -> Result<(), crate::PluginError> {
            self.events.lock_recover().push(event);
            Ok(())
        }
    }

    let tools = CountingTools::default();
    let plugins = session(
        &tools,
        vec![
            plugin(
                "zeta",
                PluginSpec::new()
                    .with_tool_args_check(hook_key!("check"), fixed_check(cached("hit"))),
            ),
            plugin(
                "alpha",
                PluginSpec::new().with_tool_args_check(
                    hook_key!("check"),
                    fixed_check(BeforeToolDecision::Deny(denial("denied"))),
                ),
            ),
        ],
    );
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = refusing_dispatch_context(plugins).await;
    context.observer = crate::testing::ChannelObservationSink::new(Some(event_tx), None);
    context.session_graph = session_graph.clone();

    let _ = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "x" })).await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("composition event receive timed out")
        .expect("composition event channel closed");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "alpha");
    let crate::PluginRuntimeEvent::ToolCheckConflict(conflict) = event else {
        panic!("expected tool check conflict event");
    };
    let reply = |plugin: &str, verdict| crate::ToolCheckReply {
        plugin_id: plugin.to_string(),
        callback: "tool_args_check:check".to_string(),
        verdict,
    };
    let expected = crate::ToolCheckConflict {
        phase: crate::ToolCheckPhase::ToolArgsCheck,
        winner: reply("alpha", crate::ToolCheckVerdictKind::Deny),
        displaced: vec![reply("zeta", crate::ToolCheckVerdictKind::Cached)],
    };
    assert_eq!(conflict, expected);
    {
        let trace_events = session_graph.events.lock_recover();
        let [
            lash_trace::TraceEvent::ToolCheckConflict {
                plugin_id,
                conflict,
            },
        ] = trace_events.as_slice()
        else {
            panic!("expected one durable composition trace event: {trace_events:?}");
        };
        assert_eq!(plugin_id, "alpha");
        assert_eq!(conflict, &expected);
    }
    drop(context);
}

/// Law 1 on the result side: result transforms chain once in order, and an
/// after-check registered before them inspects the final result.
#[tokio::test]
async fn result_transforms_chain_and_checks_see_the_final_result() {
    fn suffix(tag: &'static str) -> crate::plugin::ToolResultTransformHook {
        Arc::new(move |input| {
            Box::pin(async move {
                let crate::ToolCallOutcome::Success(value) = &input.current.outcome else {
                    return Ok(input.current);
                };
                let text = value
                    .to_json_value()
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(ToolResultCandidate {
                    outcome: crate::ToolCallOutcome::Success(crate::ToolValue::untrusted_json(
                        json!(format!("{text}{tag}")),
                    )),
                    ..input.current
                })
            })
        })
    }
    let tools = CountingTools::default();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let witness = Arc::clone(&seen);
    let output = dispatch(
        session(
            &tools,
            vec![
                plugin(
                    "audit",
                    PluginSpec::new().with_tool_result_check(
                        hook_key!("witness"),
                        Arc::new(move |input| {
                            witness.lock_recover().push((
                                input.original.outcome.clone(),
                                input.final_result.outcome.clone(),
                            ));
                            Box::pin(async { Ok(AfterToolContributions::default()) })
                        }),
                    ),
                ),
                plugin(
                    "one",
                    PluginSpec::new().with_tool_result_transform(hook_key!("tag"), suffix("+1")),
                ),
                plugin(
                    "two",
                    PluginSpec::new().with_tool_result_transform(hook_key!("tag"), suffix("+2")),
                ),
            ],
        ),
        "x",
    )
    .await;

    assert_eq!(output.value_for_projection(), json!("x+1+2"));
    let seen = seen.lock_recover();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].1,
        crate::ToolCallOutcome::Success(crate::ToolValue::untrusted_json(json!("x+1+2")))
    );
}

/// An after-check can only fail the call or stop the Run, never replace the
/// result: a Deny fails the call with its failure, an AbortRun also carries
/// the Run control, and their declared messages still apply.
#[tokio::test]
async fn after_checks_fail_or_abort_but_never_replace() {
    for (verdict, aborts) in [
        (AfterToolDecision::Deny(denial("after_denied")), false),
        (
            AfterToolDecision::AbortRun(crate::plugin::PluginAbort::new("after_stop", "stop")),
            true,
        ),
    ] {
        let tools = CountingTools::default();
        let output = dispatch(
            session(
                &tools,
                vec![plugin(
                    "audit",
                    PluginSpec::new().with_tool_result_check(
                        hook_key!("verdict"),
                        Arc::new(move |_| {
                            let verdict = verdict.clone();
                            Box::pin(async move { Ok(AfterToolContributions::from(verdict)) })
                        }),
                    ),
                )],
            ),
            "x",
        )
        .await;

        assert!(!output.is_success());
        assert_eq!(tools.executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            matches!(output.control, Some(crate::ToolControl::AbortRun { .. })),
            aborts
        );
    }
}
