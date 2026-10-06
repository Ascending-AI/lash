//! The tool hook composition (ADR 0128): transforms chain once in recorded
//! order, every check inspects one immutable value, and checks reduce by
//! strength, then plugin id, then callback key, in any registration order.
use super::*;
use crate::plugin::{BeforeToolDecision, PluginFactory, PluginSpec};
use lash_core_execution::hook_key;

fn plugin(id: &'static str, spec: PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial(id),
        spec,
    ))
}

fn fixed_check(decision: BeforeToolDecision) -> crate::plugin::ToolArgsCheckHook {
    Arc::new(move |_| {
        let decision = decision.clone();
        Box::pin(async move { Ok(decision) })
    })
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
