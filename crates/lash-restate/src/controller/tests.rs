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
    let fault = RuntimeEffectControllerError::model_unavailable(
        &lash_core::ModelKey::new("kimi-k3@tensorx"),
        "the recorded model cannot be bound on this worker",
    );
    for kind in [
        lash_core::RuntimeEffectKind::Direct,
        lash_core::RuntimeEffectKind::LlmCall,
    ] {
        assert!(fault.journal_disposition(kind).is_retryable_derivation());
    }
}

/// FIG-4608, FIG-4631: a model bind fault stays typed across the engine and
/// the plugin boundary. The engine keeps only the failed attempt's text, so
/// the fault's record rides it to the park of the exhausted retries; the
/// plugin and runtime conversions keep the code, the key and the retry
/// class. A Restate terminal is a journaled completion and is never read
/// back as the retried fault, whatever its text carries.
#[test]
fn a_model_bind_fault_stays_typed_across_the_engine_and_plugin_boundaries() {
    let key = lash_core::ModelKey::new("fast\"@worker");
    let fault = RuntimeEffectControllerError::model_unavailable(
        &key,
        "the recorded model cannot be bound on this worker",
    );
    for kind in [
        lash_core::RuntimeEffectKind::Direct,
        lash_core::RuntimeEffectKind::LlmCall,
    ] {
        assert!(
            fault.journal_disposition(kind).is_retryable_derivation(),
            "the fault is never the recorded result of {}",
            kind.as_str()
        );
    }
    let failure = fault.attempt_failure_text();
    let park = lash_core::store::ParkReason::engine_retry_exhausted(
        8,
        Some("500".to_string()),
        format!("[500] Handler failed with retryable error: {failure}"),
    );
    assert_eq!(park.model_key(), Some(&key));

    let plugin = PluginError::RuntimeEffectController(fault.clone());
    assert!(plugin.is_retryable());
    assert!(!plugin.is_terminal());
    assert_eq!(plugin.attempt_failure_text(), failure);
    let runtime = plugin.into_turn_failure(RuntimeErrorCode::Plugin);
    assert_eq!(runtime.code, RuntimeErrorCode::ModelUnavailable);
    assert_eq!(runtime.model_key(), Some(&key));
    assert!(runtime.is_retryable());
    assert!(!runtime.is_terminal());
    assert_eq!(runtime.attempt_failure_text(), failure);

    for message in [
        format!("Handler failed with retryable error: {failure}"),
        "model_unavailable: model fast is unavailable".to_string(),
        r#"failed {"lash.error":{"code":"model_unavailable","message":"unbound"}}"#.to_string(),
        r#"failed {"lash.error":{"code":"model_unavailable"}}"#.to_string(),
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
        assert!(
            !PluginError::RuntimeEffectController(bridged).is_retryable(),
            "a Restate terminal is never retried: {diagnostic}"
        );
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

/// FIG-4649: a typed refusal a handler's terminal error carries has one
/// class, whichever service the controller called. An object whose `_compat`
/// record refuses this build, and a stored value whose stamp it does not
/// read, are terminal with their typed cause on the effect-group path, the
/// journaled-effect path, the durable-wait path and the process-command path
/// alike.
#[test]
fn a_typed_terminal_refusal_has_one_class_on_every_controller_path() {
    let refusal = lash_core_store::compat::CompatRefusal::Unstamped {
        component: "effect-group index".to_string(),
        writing_release: None,
    };
    let stamp = crate::object_state::stored_format_error(
        "effect group",
        "group",
        Some(u64::from(crate::effect_group::EFFECT_GROUP_STATE_FORMAT_VERSION) + 1),
        &crate::effect_group::EFFECT_GROUP_STATE_FORMATS,
    );
    type MakeTerminal = Box<dyn Fn() -> TerminalError>;
    let terminals: [(&str, MakeTerminal); 2] = [
        (
            "object _compat refusal",
            Box::new({
                let refusal = refusal.clone();
                move || crate::wire::incompatible(refusal.clone())
            }),
        ),
        (
            "stored value stamp refusal",
            Box::new({
                let stamp = stamp.clone();
                move || TerminalError::new(stamp.to_record())
            }),
        ),
    ];
    for (name, terminal) in terminals {
        // The engine prefixes a handler's message with its own words.
        let prefixed =
            || TerminalError::new(format!("[500] handler failed: {}", terminal().message()));
        let paths = [
            (
                "effect group",
                effect_group_engine_error("EffectGroupIndex/open", terminal()),
            ),
            (
                "journaled effect",
                RuntimeEffectControllerError::from(RestateEffectError::Terminal {
                    effect: "effect".into(),
                    terminal: terminal(),
                }),
            ),
            (
                "durable wait",
                crate::wire::lash_terminal(&terminal(), RuntimeErrorCode::EngineEffectController),
            ),
            (
                "process command",
                process_command::process_command_journal_error("await control", terminal()),
            ),
            (
                "prefixed by the engine",
                crate::wire::lash_terminal(&prefixed(), RuntimeErrorCode::EngineEffectController),
            ),
        ];
        for (path, error) in paths {
            assert_eq!(
                error.code,
                RuntimeErrorCode::EngineObjectStateFormatUnsupported,
                "{name} on the {path} path: {error:?}"
            );
            assert!(
                error.is_terminal() && !error.code.is_retryable(),
                "{name} on the {path} path: {error:?}"
            );
            let runtime = error.into_runtime_error();
            assert!(
                runtime.is_terminal() && !runtime.is_retryable(),
                "{name} on the {path} path: {runtime:?}"
            );
        }
    }
    let typed = crate::wire::lash_terminal(
        &crate::wire::incompatible(refusal.clone()),
        RuntimeErrorCode::EngineEffectController,
    );
    assert_eq!(typed.compat_refusal(), Some(&refusal));
    let replayed: RuntimeEffectControllerError =
        serde_json::from_value(serde_json::to_value(&typed).expect("encode")).expect("decode");
    assert_eq!(replayed.compat_refusal(), Some(&refusal));

    // A terminal with no typed refusal keeps each path's own code.
    let untyped = || TerminalError::new("the service is not registered");
    assert_eq!(
        crate::wire::lash_terminal(&untyped(), RuntimeErrorCode::EngineEffectController).code,
        RuntimeErrorCode::EngineEffectController
    );
    assert_eq!(
        effect_group_engine_error("EffectGroupIndex/open", untyped()).code,
        RuntimeErrorCode::RuntimeEffectGroupShape
    );
}

/// FIG-4649: a recorded step journals a plugin operation's failure exactly
/// when the failure is terminal. Every store error is asked: a fault of the
/// substrate, or any other fact of the attempt, ends the attempt with nothing
/// recorded.
#[test]
fn a_recorded_step_journals_a_store_error_only_when_it_is_terminal() {
    let mut journaled_faults = Vec::new();
    for index in 0..lash_core::StoreError::samples_for_testing().len() {
        let sample = || lash_core::StoreError::samples_for_testing().swap_remove(index);
        let name = sample().variant_name();
        let transient = sample().is_transient();
        let terminal = sample().runtime_error().is_terminal();
        let step = crate::process::journal_or_retry::<()>(Err(PluginError::from(sample())));
        match step {
            Ok(Err(recorded)) => {
                if transient || !terminal {
                    journaled_faults.push(format!("{name} journaled as {recorded:?}"));
                }
            }
            Err(_) => {
                if terminal {
                    journaled_faults.push(format!("{name} is terminal and was retried"));
                }
            }
            Ok(Ok(())) => unreachable!("the step failed"),
        }
    }
    assert!(
        journaled_faults.is_empty(),
        "{} disagreements:\n{}",
        journaled_faults.len(),
        journaled_faults.join("\n")
    );
}
