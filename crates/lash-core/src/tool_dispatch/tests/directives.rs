use super::*;
use crate::plugin::PluginFactory;

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
        vec![("abort", abort_turn()), ("deny", policy_denial())],
        vec![("deny", policy_denial()), ("abort", abort_turn())],
    ] {
        let output = dispatch_with_terminal_plugins(directives).await;
        assert!(!output.is_success());
        let projected = output.value_for_projection();
        assert_eq!(projected["code"], json!("tool_error"));
        assert_eq!(projected["message"], json!("plugin aborted the turn"));
    }
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
    let mut context = exact_dispatch_context_with_plugins(plugins);
    context.event_tx = event_tx;

    let _ = dispatch_tool_call(&context, "beta".to_string(), json!({ "value": "original" })).await;

    let event = events.recv().await.expect("composition event");
    let crate::SessionStreamEvent::PluginEvent { plugin_id, event } = event else {
        panic!("expected plugin runtime event");
    };
    assert_eq!(
        plugin_id,
        crate::session_model::PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID
    );
    let crate::PluginRuntimeEvent::Custom { name, payload } = event else {
        panic!("expected custom composition event");
    };
    assert_eq!(name, "before_tool_call.directive_conflict");
    assert_eq!(payload["winner_plugin_id"], json!("deny"));
    assert_eq!(payload["winner_directive"], json!("denied_short_circuit"));
    assert_eq!(payload["ignored_plugin_id"], json!("allow"));
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
