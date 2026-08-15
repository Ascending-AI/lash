use super::*;
use crate::plugin::PluginFactory;

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

fn before_tool_factory(
    id: &'static str,
    hook: crate::plugin::BeforeToolCallHook,
) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        id,
        crate::PluginSpec::new().with_before_tool_call(hook),
    ))
}

fn fixed_before_tool_factory(
    id: &'static str,
    directive: crate::PluginDirective,
) -> Arc<dyn PluginFactory> {
    before_tool_factory(
        id,
        Arc::new(move |_ctx| {
            let directive = directive.clone();
            Box::pin(async move { Ok(vec![directive]) })
        }),
    )
}

fn before_tool_plugin_stack(mut factories: Vec<Arc<dyn PluginFactory>>) -> Arc<PluginSession> {
    factories.insert(
        0,
        Arc::new(StaticPluginFactory::new(
            "directive_test_tools",
            crate::PluginSpec::new().with_tool_provider(Arc::new(MockTools)),
        )),
    );
    PluginHost::new(factories)
        .build_session("root", None)
        .expect("plugin session")
}

fn after_tool_factory(
    id: &'static str,
    hook: crate::plugin::AfterToolCallHook,
) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        id,
        crate::PluginSpec::new().with_after_tool_call(hook),
    ))
}

fn fixed_after_tool_factory(
    id: &'static str,
    directives: Vec<crate::PluginDirective>,
) -> Arc<dyn PluginFactory> {
    after_tool_factory(
        id,
        Arc::new(move |_ctx| {
            let directives = directives.clone();
            Box::pin(async move { Ok(directives) })
        }),
    )
}

fn after_tool_plugin_stack(mut factories: Vec<Arc<dyn PluginFactory>>) -> Arc<PluginSession> {
    factories.insert(
        0,
        Arc::new(StaticPluginFactory::new(
            "directive_test_tools",
            crate::PluginSpec::new().with_tool_provider(Arc::new(MockTools)),
        )),
    );
    PluginHost::new(factories)
        .build_session("root", None)
        .expect("plugin session")
}

fn denial(code: &str, message: &str) -> crate::PluginDirective {
    crate::PluginDirective::ShortCircuitTool {
        output: crate::ToolCallOutput::failure(crate::ToolFailure::tool(
            crate::ToolFailureClass::InvalidRequest,
            code,
            message,
        )),
    }
}

fn policy_denial() -> crate::PluginDirective {
    denial("policy_denied", "policy denied the call")
}

fn successful_short_circuit() -> crate::PluginDirective {
    crate::PluginDirective::ShortCircuitTool {
        output: crate::ToolCallOutput::success(json!("allowed")),
    }
}

fn abort_turn() -> crate::PluginDirective {
    crate::PluginDirective::AbortTurn {
        code: "plugin_abort".to_string(),
        message: "plugin aborted the turn".to_string(),
    }
}

async fn dispatch_with_terminal_plugins(
    directives: Vec<(&'static str, crate::PluginDirective)>,
) -> crate::ToolCallOutput {
    let factories = directives
        .into_iter()
        .map(|(id, directive)| fixed_before_tool_factory(id, directive))
        .collect();
    dispatch_tool_call(
        &exact_dispatch_context_with_plugins(before_tool_plugin_stack(factories)),
        "beta".to_string(),
        json!({ "value": "original" }),
    )
    .await
    .record
    .output
}

#[tokio::test]
async fn deny_cannot_be_overridden_by_later_success_on_the_raw_fold() {
    let outcome = super::super::directives::apply_before_tool_directives(
        &dispatch_context(),
        json!({ "value": "original" }),
        vec![
            crate::plugin::PluginOwned {
                plugin_id: "deny".to_string(),
                value: policy_denial(),
            },
            crate::plugin::PluginOwned {
                plugin_id: "allow".to_string(),
                value: successful_short_circuit(),
            },
        ],
    )
    .await;

    let result = outcome.short_circuit.expect("directive must terminate");
    assert!(
        !result.is_success(),
        "a later plugin cannot restore permission"
    );
    assert_eq!(
        result.value_for_projection()["code"],
        json!("policy_denied")
    );
}

#[tokio::test]
async fn multi_plugin_deny_then_allow_keeps_deny() {
    let output = dispatch_with_terminal_plugins(vec![
        ("deny", policy_denial()),
        ("allow", successful_short_circuit()),
    ])
    .await;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
}

#[tokio::test]
async fn multi_plugin_allow_then_deny_keeps_deny() {
    let output = dispatch_with_terminal_plugins(vec![
        ("allow", successful_short_circuit()),
        ("deny", policy_denial()),
    ])
    .await;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
}

#[tokio::test]
async fn multi_plugin_abort_wins_in_either_registration_order() {
    for directives in [
        vec![
            ("abort", abort_turn()),
            ("allow", successful_short_circuit()),
        ],
        vec![
            ("allow", successful_short_circuit()),
            ("abort", abort_turn()),
        ],
    ] {
        let output = dispatch_with_terminal_plugins(directives).await;
        assert!(!output.is_success());
        let projected = output.value_for_projection();
        assert_eq!(projected["code"], json!("tool_error"));
        assert_eq!(projected["message"], json!("plugin aborted the turn"));
    }

    let output =
        dispatch_with_terminal_plugins(vec![("deny", policy_denial()), ("abort", abort_turn())])
            .await;
    assert!(!output.is_success());
    let projected = output.value_for_projection();
    assert_eq!(projected["code"], json!("policy_denied"));
    assert_eq!(projected["message"], json!("policy denied the call"));

    let output =
        dispatch_with_terminal_plugins(vec![("abort", abort_turn()), ("deny", policy_denial())])
            .await;
    assert!(!output.is_success());
    let projected = output.value_for_projection();
    assert_eq!(projected["code"], json!("tool_error"));
    assert_eq!(projected["message"], json!("plugin aborted the turn"));
}

#[tokio::test]
async fn equal_terminal_strength_uses_plugin_id_not_registration_order() {
    for directives in [
        vec![
            ("zulu", denial("zulu_denied", "zulu denied")),
            ("alpha", denial("alpha_denied", "alpha denied")),
        ],
        vec![
            ("alpha", denial("alpha_denied", "alpha denied")),
            ("zulu", denial("zulu_denied", "zulu denied")),
        ],
    ] {
        let output = dispatch_with_terminal_plugins(directives).await;
        assert_eq!(output.value_for_projection()["code"], json!("alpha_denied"));
    }
}

#[tokio::test]
async fn terminal_conflict_is_an_observable_composition_event() {
    let plugins = before_tool_plugin_stack(vec![
        fixed_before_tool_factory("deny", policy_denial()),
        fixed_before_tool_factory("allow", successful_short_circuit()),
    ]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let _ = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" })).await;

    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("composition event receive timed out")
        .expect("composition event channel closed");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "allow");
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom composition event");
    };
    assert_eq!(name, "before_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("deny"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("allow"));

    let trace_events = session_graph.events.lock_recover();
    let [lash_trace::TraceEvent::Custom { name, payload }] = trace_events.as_slice() else {
        panic!("expected one durable composition trace event: {trace_events:?}");
    };
    assert_eq!(name, "plugin.allow.before_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("deny"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("allow"));
    assert_eq!(
        payload["ignored_directive"],
        json!("successful_short_circuit")
    );
}

#[tokio::test]
async fn reinspection_does_not_emit_a_self_conflict() {
    let plugins = before_tool_plugin_stack(vec![
        fixed_before_tool_factory("policy", policy_denial()),
        fixed_before_tool_factory(
            "normalizer",
            crate::PluginDirective::ReplaceToolArgs {
                args: json!({ "value": "normalized" }),
            },
        ),
    ]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
    assert!(events.try_recv().is_err(), "self-conflict runtime event");
    assert!(
        session_graph.events.lock_recover().is_empty(),
        "self-conflict trace event"
    );
}

#[tokio::test]
async fn replacement_is_seen_by_remaining_plugin_hooks() {
    let inspected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let inspector_observations = Arc::clone(&inspected);
    let replacer = fixed_before_tool_factory(
        "replace",
        crate::PluginDirective::ReplaceToolArgs {
            args: json!({ "value": "replaced" }),
        },
    );
    let inspector = before_tool_factory(
        "inspect",
        Arc::new(move |ctx| {
            let inspector_observations = Arc::clone(&inspector_observations);
            Box::pin(async move {
                inspector_observations.lock_recover().push(ctx.args);
                Ok(Vec::new())
            })
        }),
    );
    let context =
        exact_dispatch_context_with_plugins(before_tool_plugin_stack(vec![replacer, inspector]));

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(
        inspected.lock_recover().as_slice(),
        &[json!({ "value": "replaced" })]
    );
    assert_eq!(output.value_for_projection(), json!("replaced"));
}

#[tokio::test]
async fn replacement_is_reinspected_by_earlier_policy_in_either_registration_order() {
    for policy_first in [true, false] {
        let policy = before_tool_factory(
            "policy",
            Arc::new(|ctx| {
                Box::pin(async move {
                    if ctx.args["value"] == json!("forbidden") {
                        Ok(vec![policy_denial()])
                    } else {
                        Ok(Vec::new())
                    }
                })
            }),
        );
        let replacer = fixed_before_tool_factory(
            "replacer",
            crate::PluginDirective::ReplaceToolArgs {
                args: json!({ "value": "forbidden" }),
            },
        );
        let mut factories = if policy_first {
            vec![policy, replacer]
        } else {
            vec![replacer, policy]
        };
        factories.push(fixed_before_tool_factory(
            "allow",
            successful_short_circuit(),
        ));
        let context = exact_dispatch_context_with_plugins(before_tool_plugin_stack(factories));

        let output =
            dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
                .await
                .record
                .output;

        assert!(!output.is_success(), "policy_first={policy_first}");
        assert_eq!(
            output.value_for_projection()["code"],
            json!("policy_denied"),
            "policy_first={policy_first}"
        );
    }
}

#[tokio::test]
async fn replacement_during_bounded_reinspection_is_a_typed_composition_error() {
    let earlier = before_tool_factory(
        "earlier",
        Arc::new(|ctx| {
            Box::pin(async move {
                if ctx.args["value"] == json!("forbidden") {
                    Ok(vec![crate::PluginDirective::ReplaceToolArgs {
                        args: json!({ "value": "second replacement" }),
                    }])
                } else {
                    Ok(Vec::new())
                }
            })
        }),
    );
    let later = fixed_before_tool_factory(
        "later",
        crate::PluginDirective::ReplaceToolArgs {
            args: json!({ "value": "forbidden" }),
        },
    );
    let plugins = before_tool_plugin_stack(vec![earlier, later]);

    let error = plugins
        .before_tool_call(crate::plugin::ToolCallHookContext::new(
            "session".to_string(),
            "beta".to_string(),
            json!({ "value": "original" }),
            beta_tool().manifest().argument_projection,
            crate::TurnContext::default(),
            Arc::new(crate::testing::MockSessionManager::default()),
        ))
        .await
        .expect_err("a reinspection pass cannot replace arguments again");

    let crate::PluginError::BeforeToolCallReplacementConflict {
        replacing_plugin_id,
        repeated_plugin_id,
    } = error
    else {
        panic!("expected typed replacement conflict: {error:?}");
    };
    assert_eq!(replacing_plugin_id, "later");
    assert_eq!(repeated_plugin_id, "earlier");
}

#[tokio::test]
async fn clean_bounded_reinspection_runs_on_replaced_arguments() {
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let earlier_observed = Arc::clone(&observed);
    let earlier = before_tool_factory(
        "earlier",
        Arc::new(move |ctx| {
            let earlier_observed = Arc::clone(&earlier_observed);
            Box::pin(async move {
                earlier_observed.lock_recover().push(ctx.args);
                Ok(Vec::new())
            })
        }),
    );
    let later = fixed_before_tool_factory(
        "later",
        crate::PluginDirective::ReplaceToolArgs {
            args: json!({ "value": "replaced" }),
        },
    );
    let context =
        exact_dispatch_context_with_plugins(before_tool_plugin_stack(vec![earlier, later]));

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(
        observed.lock_recover().as_slice(),
        &[
            json!({ "value": "original" }),
            json!({ "value": "replaced" }),
        ]
    );
    assert_eq!(output.value_for_projection(), json!("replaced"));
}

#[tokio::test]
async fn reinspection_rehonors_terminals_without_reapplying_side_effects() {
    let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let auditor_invocations = Arc::clone(&invocations);
    let auditor = before_tool_factory(
        "auditor",
        Arc::new(move |ctx| {
            let auditor_invocations = Arc::clone(&auditor_invocations);
            Box::pin(async move {
                auditor_invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut directives = vec![crate::PluginDirective::emit_runtime_events(vec![
                    crate::PluginRuntimeEvent::Custom {
                        name: "audit".to_string(),
                        payload: json!({ "value": ctx.args["value"] }),
                    },
                ])];
                if ctx.args["value"] == json!("forbidden") {
                    directives.push(policy_denial());
                }
                Ok(directives)
            })
        }),
    );
    let replacer = fixed_before_tool_factory(
        "replacer",
        crate::PluginDirective::ReplaceToolArgs {
            args: json!({ "value": "forbidden" }),
        },
    );
    let plugins = before_tool_plugin_stack(vec![auditor, replacer]);
    let (event_tx, mut events) = mpsc::channel(8);
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
    let event = events.try_recv().expect("first-pass audit event");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "auditor");
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom audit event");
    };
    assert_eq!(name, "audit");
    assert_eq!(payload["value"], json!("original"));
    assert!(
        events.try_recv().is_err(),
        "reinspection duplicated audit event"
    );
}

#[tokio::test]
async fn two_unconditional_replacers_are_a_typed_composition_error() {
    let plugins = before_tool_plugin_stack(vec![
        fixed_before_tool_factory(
            "normalizer_one",
            crate::PluginDirective::ReplaceToolArgs {
                args: json!({ "value": "one" }),
            },
        ),
        fixed_before_tool_factory(
            "normalizer_two",
            crate::PluginDirective::ReplaceToolArgs {
                args: json!({ "value": "two" }),
            },
        ),
    ]);

    let error = plugins
        .before_tool_call(crate::plugin::ToolCallHookContext::new(
            "session".to_string(),
            "beta".to_string(),
            json!({ "value": "original" }),
            beta_tool().manifest().argument_projection,
            crate::TurnContext::default(),
            Arc::new(crate::testing::MockSessionManager::default()),
        ))
        .await
        .expect_err("two unconditional replacers must fail closed");

    let crate::PluginError::BeforeToolCallReplacementConflict {
        replacing_plugin_id,
        repeated_plugin_id,
    } = error
    else {
        panic!("expected typed replacement conflict: {error:?}");
    };
    assert_eq!(replacing_plugin_id, "normalizer_two");
    assert_eq!(repeated_plugin_id, "normalizer_one");
}

async fn dispatch_with_after_terminal_plugins(
    directives: Vec<(&'static str, crate::PluginDirective)>,
) -> crate::ToolCallOutput {
    let factories = directives
        .into_iter()
        .map(|(id, directive)| fixed_after_tool_factory(id, vec![directive]))
        .collect();
    dispatch_tool_call(
        &exact_dispatch_context_with_plugins(after_tool_plugin_stack(factories)),
        "beta".to_string(),
        json!({ "value": "original" }),
    )
    .await
    .record
    .output
}

fn successful_replacement(value: &str) -> crate::PluginDirective {
    crate::PluginDirective::ShortCircuitTool {
        output: crate::ToolCallOutput::success(json!(value)),
    }
}

#[tokio::test]
async fn after_tool_deny_wins_in_either_registration_order() {
    for directives in [
        vec![
            ("deny", policy_denial()),
            ("allow", successful_replacement("allowed")),
        ],
        vec![
            ("allow", successful_replacement("allowed")),
            ("deny", policy_denial()),
        ],
    ] {
        let output = dispatch_with_after_terminal_plugins(directives).await;
        assert!(!output.is_success());
        assert_eq!(
            output.value_for_projection()["code"],
            json!("policy_denied")
        );
    }
}

#[tokio::test]
async fn after_tool_abort_wins_in_either_registration_order() {
    for directives in [
        vec![
            ("abort", abort_turn()),
            ("allow", successful_replacement("allowed")),
        ],
        vec![
            ("allow", successful_replacement("allowed")),
            ("abort", abort_turn()),
        ],
    ] {
        let output = dispatch_with_after_terminal_plugins(directives).await;
        assert!(!output.is_success());
        assert_eq!(
            output.value_for_projection()["message"],
            json!("plugin aborted the turn")
        );
    }

    let output = dispatch_with_after_terminal_plugins(vec![
        ("deny", policy_denial()),
        ("abort", abort_turn()),
    ])
    .await;
    assert!(!output.is_success());
    let projected = output.value_for_projection();
    assert_eq!(projected["code"], json!("policy_denied"));
    assert_eq!(projected["message"], json!("policy denied the call"));

    let output = dispatch_with_after_terminal_plugins(vec![
        ("abort", abort_turn()),
        ("deny", policy_denial()),
    ])
    .await;
    assert!(!output.is_success());
    let projected = output.value_for_projection();
    assert_eq!(projected["code"], json!("tool_error"));
    assert_eq!(projected["message"], json!("plugin aborted the turn"));
}

#[tokio::test]
async fn after_tool_three_plugins_keep_the_most_restrictive_terminal() {
    let output = dispatch_with_after_terminal_plugins(vec![
        ("first", successful_replacement("first")),
        ("deny", policy_denial()),
        ("abort", abort_turn()),
    ])
    .await;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
}

#[tokio::test]
async fn after_tool_equal_strength_result_replacement_is_first_wins() {
    let first = after_tool_factory(
        "first",
        Arc::new(|ctx| {
            Box::pin(async move {
                if ctx.result.value_for_projection() == json!("original") {
                    Ok(vec![successful_replacement("first")])
                } else {
                    Ok(Vec::new())
                }
            })
        }),
    );
    let second = fixed_after_tool_factory("second", vec![successful_replacement("second")]);
    let plugins = after_tool_plugin_stack(vec![first, second]);
    let context = exact_dispatch_context_with_plugins(plugins);

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(output.value_for_projection(), json!("first"));
}

#[tokio::test]
async fn after_tool_replacement_is_reinspected_by_policy_in_either_registration_order() {
    for policy_first in [true, false] {
        let policy = after_tool_factory(
            "policy",
            Arc::new(|ctx| {
                Box::pin(async move {
                    if ctx
                        .result
                        .value_for_projection()
                        .as_str()
                        .is_some_and(|value| value.contains("SECRET"))
                    {
                        Ok(vec![policy_denial()])
                    } else {
                        Ok(Vec::new())
                    }
                })
            }),
        );
        let injector =
            fixed_after_tool_factory("injector", vec![successful_replacement("SECRET-material")]);
        let factories = if policy_first {
            vec![policy, injector]
        } else {
            vec![injector, policy]
        };
        let context = exact_dispatch_context_with_plugins(after_tool_plugin_stack(factories));

        let output =
            dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
                .await
                .record
                .output;

        assert!(!output.is_success(), "policy_first={policy_first}");
        assert_eq!(
            output.value_for_projection()["code"],
            json!("policy_denied"),
            "policy_first={policy_first}"
        );
    }
}

#[tokio::test]
async fn after_tool_clean_bounded_reinspection_keeps_the_replaced_result() {
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let earlier_observed = Arc::clone(&observed);
    let earlier = after_tool_factory(
        "earlier",
        Arc::new(move |ctx| {
            let earlier_observed = Arc::clone(&earlier_observed);
            Box::pin(async move {
                earlier_observed
                    .lock_recover()
                    .push(ctx.result.value_for_projection());
                Ok(Vec::new())
            })
        }),
    );
    let later = fixed_after_tool_factory("later", vec![successful_replacement("replaced")]);
    let context =
        exact_dispatch_context_with_plugins(after_tool_plugin_stack(vec![earlier, later]));

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(
        observed.lock_recover().as_slice(),
        &[json!("original"), json!("replaced")]
    );
    assert_eq!(output.value_for_projection(), json!("replaced"));
}

#[tokio::test]
async fn after_tool_replacement_during_reinspection_is_a_typed_composition_error() {
    let earlier = after_tool_factory(
        "earlier",
        Arc::new(|ctx| {
            Box::pin(async move {
                if ctx.result.value_for_projection() == json!("replaced") {
                    Ok(vec![successful_replacement("second replacement")])
                } else {
                    Ok(Vec::new())
                }
            })
        }),
    );
    let later = fixed_after_tool_factory("later", vec![successful_replacement("replaced")]);
    let plugins = after_tool_plugin_stack(vec![earlier, later]);

    let error = plugins
        .after_tool_call(crate::plugin::ToolResultHookContext::new(
            "session".to_string(),
            "beta".to_string(),
            json!({ "value": "original" }),
            crate::ToolResult::from_output(crate::ToolCallOutput::success(json!("original"))),
            0,
            crate::TurnContext::default(),
            Arc::new(crate::testing::MockSessionManager::default()),
        ))
        .await
        .expect_err("a reinspection pass cannot replace the result again");

    let crate::PluginError::AfterToolCallReplacementConflict {
        replacing_plugin_id,
        repeated_plugin_id,
    } = error
    else {
        panic!("expected typed replacement conflict: {error:?}");
    };
    assert_eq!(replacing_plugin_id, "later");
    assert_eq!(repeated_plugin_id, "earlier");
}

#[tokio::test]
async fn after_tool_reinspection_does_not_reapply_side_effects() {
    let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let auditor_invocations = Arc::clone(&invocations);
    let auditor = after_tool_factory(
        "auditor",
        Arc::new(move |ctx| {
            let auditor_invocations = Arc::clone(&auditor_invocations);
            Box::pin(async move {
                auditor_invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![crate::PluginDirective::emit_runtime_events(vec![
                    crate::PluginRuntimeEvent::Custom {
                        name: "audit".to_string(),
                        payload: json!({ "value": ctx.result.value_for_projection() }),
                    },
                ])])
            })
        }),
    );
    let replacer = fixed_after_tool_factory("replacer", vec![successful_replacement("replaced")]);
    let plugins = after_tool_plugin_stack(vec![auditor, replacer]);
    let (event_tx, mut events) = mpsc::channel(8);
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(output.value_for_projection(), json!("replaced"));
    let event = events.try_recv().expect("first-pass audit event");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "auditor");
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom audit event");
    };
    assert_eq!(name, "audit");
    assert_eq!(payload["value"], json!("original"));
    assert!(
        events.try_recv().is_err(),
        "reinspection duplicated audit event"
    );
}

#[tokio::test]
async fn after_tool_reinspection_does_not_emit_a_self_conflict() {
    let plugins = after_tool_plugin_stack(vec![
        fixed_after_tool_factory("policy", vec![policy_denial()]),
        fixed_after_tool_factory("replacer", vec![successful_replacement("replaced")]),
    ]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert!(!output.is_success());
    let event = events.try_recv().expect("inter-plugin conflict event");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, .. } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "replacer");
    assert!(events.try_recv().is_err(), "self-conflict runtime event");
    assert_eq!(
        session_graph.events.lock_recover().len(),
        1,
        "self-conflict trace event"
    );
}

#[tokio::test]
async fn after_tool_two_unconditional_replacers_fail_closed() {
    let plugins = after_tool_plugin_stack(vec![
        fixed_after_tool_factory("first", vec![successful_replacement("first")]),
        fixed_after_tool_factory("second", vec![successful_replacement("second")]),
    ]);

    let error = plugins
        .after_tool_call(crate::plugin::ToolResultHookContext::new(
            "session".to_string(),
            "beta".to_string(),
            json!({ "value": "original" }),
            crate::ToolResult::from_output(crate::ToolCallOutput::success(json!("original"))),
            0,
            crate::TurnContext::default(),
            Arc::new(crate::testing::MockSessionManager::default()),
        ))
        .await
        .expect_err("two unconditional result replacers must fail closed");

    let crate::PluginError::AfterToolCallReplacementConflict {
        replacing_plugin_id,
        repeated_plugin_id,
    } = error
    else {
        panic!("expected typed replacement conflict: {error:?}");
    };
    assert_eq!(replacing_plugin_id, "second");
    assert_eq!(repeated_plugin_id, "first");
}

#[tokio::test]
async fn after_tool_same_plugin_terminals_tighten_without_self_conflict() {
    let plugins = after_tool_plugin_stack(vec![fixed_after_tool_factory(
        "policy",
        vec![
            successful_replacement("first"),
            policy_denial(),
            successful_replacement("last"),
        ],
    )]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
    assert!(events.try_recv().is_err(), "self-conflict runtime event");
    assert!(
        session_graph.events.lock_recover().is_empty(),
        "self-conflict trace event"
    );
}

#[tokio::test]
async fn after_tool_terminal_conflict_has_bounded_identity_evidence() {
    let plugins = after_tool_plugin_stack(vec![
        fixed_after_tool_factory("deny", vec![policy_denial()]),
        fixed_after_tool_factory("allow", vec![successful_replacement("allowed")]),
    ]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let _ = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" })).await;

    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("composition event receive timed out")
        .expect("composition event channel closed");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "allow");
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom composition event");
    };
    assert_eq!(name, "after_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("deny"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("allow"));
    assert_eq!(
        payload["ignored_directive"],
        json!("successful_short_circuit")
    );

    let trace_events = session_graph.events.lock_recover();
    let [lash_trace::TraceEvent::Custom { name, payload }] = trace_events.as_slice() else {
        panic!("expected one durable composition trace event: {trace_events:?}");
    };
    assert_eq!(name, "plugin.allow.after_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("deny"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("allow"));
    assert_eq!(
        payload["ignored_directive"],
        json!("successful_short_circuit")
    );
}

#[tokio::test]
async fn displaced_after_tool_replace_args_misuse_emits_conflict_evidence() {
    let plugins = after_tool_plugin_stack(vec![
        fixed_after_tool_factory("policy", vec![policy_denial()]),
        fixed_after_tool_factory(
            "misuser",
            vec![crate::PluginDirective::ReplaceToolArgs {
                args: json!({ "not": "valid after execution" }),
            }],
        ),
    ]);
    let (event_tx, mut events) = mpsc::channel(8);
    let session_graph = Arc::new(RecordingSessionGraph::default());
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;
    context.session_graph = session_graph.clone();

    let output = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" }))
        .await
        .record
        .output;

    assert!(!output.is_success());
    assert_eq!(
        output.value_for_projection()["code"],
        json!("policy_denied")
    );
    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("misuse conflict receive timed out")
        .expect("misuse conflict channel closed");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(plugin_id, "misuser");
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom composition event");
    };
    assert_eq!(name, "after_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("policy"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("misuser"));
    assert_eq!(payload["ignored_directive"], json!("denied_short_circuit"));

    let trace_events = session_graph.events.lock_recover();
    let [lash_trace::TraceEvent::Custom { name, payload }] = trace_events.as_slice() else {
        panic!("expected one durable misuse-conflict event: {trace_events:?}");
    };
    assert_eq!(name, "plugin.misuser.after_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("policy"));
    assert_eq!(payload["ignored_plugin_id"], json!("misuser"));
}
