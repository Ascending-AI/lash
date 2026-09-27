use super::*;

#[test]
fn restate_trace_projection_uses_shared_parent_precedence_and_scoped_nodes() {
    let parent_address = lash_core::EffectAddress::new(
        ExecutionScope::process(lash_core::ProcessId::fixture("restate-parent-process")),
        "shared-replay-key",
    )
    .expect("valid Restate causal address");
    let invocation = RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::turn("restate-session", "restate-turn"),
            "restate-child-key",
        )
        .expect("valid Restate child address"),
        lash_core::RuntimeAttribution::for_turn("restate-session", "restate-turn", 4, 2),
        "restate-child",
    )
    .with_caused_by(Some(lash_core::CausalRef::Effect {
        address: parent_address.clone(),
    }));

    let caused = trace_context_for_runtime_effect_invocation(
        lash_trace::TraceContext::default(),
        &invocation,
    );
    assert_eq!(
        caused.parent_graph_node_id.as_deref(),
        Some(parent_address.graph_key().as_str())
    );

    let explicit = lash_trace::TraceContext {
        parent_graph_node_id: Some("host:explicit-parent".to_string()),
        run_id: Some("restate-host-run".to_string()),
        ..Default::default()
    };
    let explicit = trace_context_for_runtime_effect_invocation(explicit.clone(), &invocation);
    assert_eq!(
        explicit.parent_graph_node_id.as_deref(),
        Some("host:explicit-parent")
    );
    assert_eq!(explicit.run_id.as_deref(), Some("restate-host-run"));
}
