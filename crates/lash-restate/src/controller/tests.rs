use super::*;

#[test]
fn recorded_renderer_refusal_retries_the_uncommitted_presentation() {
    let invocation = RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::turn("render-session", "render-turn"),
            "present-result",
        )
        .expect("effect address"),
        lash_core::RuntimeAttribution::for_turn("render-session", "render-turn", 0, 0),
        "present-result",
    );
    let route = execution::restate_effect_execution(RuntimeEffectEnvelope {
        invocation,
        command: RuntimeEffectCommand::PresentToolResult {
            call_id: lash_core::ToolCallId::fixture("call"),
            tool_id: lash_core::ToolId::new("tool:fixture"),
            tool_name: "fixture".into(),
            render: None,
            args: serde_json::Value::Null,
            output: Box::new(lash_core::ToolCallOutput::success("output")),
        },
        group: None,
    })
    .expect("presentation route");
    assert!(matches!(
        route,
        execution::RestateEffectExecution::JournaledRun {
            engine_faults: EngineFaults::Retried,
            ..
        }
    ));
    let refusal = RuntimeEffectControllerError::new(
        RuntimeErrorCode::RecordedRendererUnavailable,
        "renderer absent",
    )
    .retryable_uncommitted_derivation();
    assert!(
        refusal
            .journal_disposition(lash_core::RuntimeEffectKind::PresentToolResult)
            .is_retryable_derivation()
    );
}

/// FIG-4404: a direct completion runs in the fault class of a model call, so
/// a recorded model its body cannot bind ends the attempt and is never
/// journaled as the completion's result.
#[test]
fn a_direct_completion_retries_an_unbound_model_instead_of_recording_it() {
    let invocation = RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::turn("direct-session", "direct-turn"),
            "direct-completion",
        )
        .expect("effect address"),
        lash_core::RuntimeAttribution::for_turn("direct-session", "direct-turn", 0, 0),
        "direct-completion",
    );
    let route = execution::restate_effect_execution(RuntimeEffectEnvelope {
        invocation,
        command: RuntimeEffectCommand::Direct {
            model_key: lash_core::ModelKey::new("kimi-k3@tensorx"),
            request: Box::new(lash_core::LlmRequestSpec {
                instructions: None,
                model: "kimi-k3".to_string(),
                messages: Vec::new(),
                tools: Arc::new(Vec::new()),
                tool_choice: Default::default(),
                attachment_acceptance: Default::default(),
                model_variant: Default::default(),
                model_capability: lash_core::ModelCapability::default(),
                extra_body: Default::default(),
                request_defaults: Default::default(),
                generation: lash_core::GenerationOptions::default(),
                scope: lash_core::LlmRequestScope::new(
                    "direct-session".to_string(),
                    "direct-session:frame:test".to_string(),
                    "direct-session:request:test".to_string(),
                ),
                output_spec: None,
            }),
            usage_source: "compaction".into(),
        },
        group: None,
    })
    .expect("direct route");
    assert!(matches!(
        route,
        execution::RestateEffectExecution::JournaledRun {
            engine_faults: EngineFaults::Retried,
            ..
        }
    ));
    let fault = RuntimeEffectControllerError::model_unavailable(&lash_core::ModelUnavailable::new(
        lash_core::ModelKey::new("kimi-k3@tensorx"),
        lash_core::ModelUnavailableReason::UnknownKey,
    ));
    for kind in [
        lash_core::RuntimeEffectKind::Direct,
        lash_core::RuntimeEffectKind::LlmCall,
    ] {
        assert!(fault.journal_disposition(kind).is_retryable_derivation());
    }
}

#[test]
fn a_model_bind_fault_survives_the_restate_terminal_and_plugin_boundaries() {
    let key = lash_core::ModelKey::new("fast\"@worker");
    let fault = RuntimeEffectControllerError::model_unavailable(&lash_core::ModelUnavailable::new(
        key.clone(),
        lash_core::ModelUnavailableReason::UnknownKey,
    ));
    let terminal = TerminalError::new(format!(
        "Handler failed with retryable error: {}",
        fault.attempt_failure_text()
    ));
    let bridged = RuntimeEffectControllerError::from(RestateEffectError::Terminal {
        effect: "child-llm-call".into(),
        terminal,
    });
    assert_eq!(bridged.code, RuntimeErrorCode::ModelUnavailable);
    assert_eq!(bridged.cause, fault.cause);
    assert_eq!(bridged.model_key(), Some(&key));
    assert!(bridged.message.contains("child-llm-call"));
    assert!(!bridged.journaled);
    let plugin = PluginError::RuntimeEffectController(bridged);
    assert!(plugin.is_retryable());
    assert!(!plugin.is_terminal());
    let runtime = plugin.into_turn_failure(RuntimeErrorCode::Plugin);
    assert_eq!(runtime.code, RuntimeErrorCode::ModelUnavailable);
    assert_eq!(runtime.model_key(), Some(&key));
    assert!(runtime.is_retryable());
    assert!(!runtime.is_terminal());

    for message in [
        "model_unavailable: model fast is unavailable",
        r#"failed {"fault":"lash.model_unavailable","model_key":null}"#,
        r#"failed {"fault":"lash.model_unavailable","model_key":"fast""#,
        r#"failed {"fault":"lash.unknown","model_key":"fast"}"#,
    ] {
        let error = RestateEffectError::Terminal {
            effect: "other-effect".into(),
            terminal: TerminalError::new(message),
        };
        let diagnostic = error.to_string();
        let bridged = RuntimeEffectControllerError::from(error);
        assert_eq!(bridged.code, RuntimeErrorCode::EngineEffectController);
        assert_eq!(bridged.message, diagnostic);
        assert!(bridged.cause.is_none());
    }
}

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
