//! The tool hook composition (ADR 0128): transforms chain once in recorded
//! order, every check inspects one immutable value, and checks reduce by
//! strength, then plugin id, then callback key, in any registration order.
use super::*;
use crate::plugin::{
    AfterToolContributions, BeforeToolDecision, CachedToolSuccess, PluginFactory, PluginSpec,
    ToolHookOccurrence,
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
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugin session")
}

/// Dispatch `beta` with `value` under `plugins`, with no effects: each law
/// here ends before the body could run.
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
        .build_session(PluginSessionRequest::creation("root", Default::default()))
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
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .err()
        .expect("an unpaired stream state is refused");
    assert!(
        error
            .to_string()
            .contains("assistant_stream_finished:missing")
    );
}
