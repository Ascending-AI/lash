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
            plan: Box::default(),
            call_id: lash_core::ToolCallId::fixture("call"),
            tool_id: lash_core::ToolId::new("tool:fixture"),
            tool_name: "fixture".into(),
            render: None,
            args: serde_json::Value::Null,
            output: Box::new(lash_core::ToolCallOutput::success("output")),
        },
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
fn a_direct_completion_retries_an_unbound_llm_profile_instead_of_recording_it() {
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
            request: {
                let mut request = Box::new(lash_core::LlmRequestSpec {
                    instructions: None,
                    model: lash_sansio::llm_profile::LlmProfileConfig::new(
                        lash_sansio::llm_profile::RecordedLlmProfile::mint(
                            lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                            lash_sansio::llm_profile::LlmProfileMetadata::builder(
                                "kimi-k3".to_string(),
                            )
                            .context_window_tokens(128_000)
                            .capability(lash_core::LlmProfileCapability::default())
                            .extra_body(Default::default())
                            .request_defaults(Default::default())
                            .build()
                            .expect("valid profile"),
                        ),
                    )
                    .with_reasoning(Default::default()),
                    messages: Vec::new(),
                    tools: Arc::new(Vec::new()),
                    tool_choice: Default::default(),
                    attachment_acceptance: Default::default(),
                    generation: lash_core::GenerationOptions::default(),
                    scope: lash_core::LlmRequestScope::new(
                        lash_core::SessionId::fixture("direct-session".to_string()),
                        "direct-session:frame:test".to_string(),
                        "direct-session:request:test".to_string(),
                    ),
                    output_spec: None,
                });
                request.model.model = lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::new("kimi-k3@tensorx"),
                    request.model.metadata().clone(),
                );
                request
            },
            usage_source: "compaction".into(),
        },
    })
    .expect("direct route");
    assert!(matches!(
        route,
        execution::RestateEffectExecution::JournaledRun {
            engine_faults: EngineFaults::Retried,
            ..
        }
    ));
    let fault = RuntimeEffectControllerError::llm_profile_unavailable(
        &lash_core::LlmProfileKey::new("kimi-k3@tensorx"),
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
fn a_profile_bind_fault_stays_typed_across_the_engine_and_plugin_boundaries() {
    let key = lash_core::LlmProfileKey::new("fast\"@worker");
    let fault = RuntimeEffectControllerError::llm_profile_unavailable(
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
    assert_eq!(park.profile_key(), Some(&key));

    let plugin = PluginError::RuntimeEffectController(fault.clone());
    assert!(plugin.is_retryable());
    assert!(!plugin.is_terminal());
    assert_eq!(plugin.attempt_failure_text(), failure);
    let runtime = plugin.into_turn_failure(RuntimeErrorCode::Plugin);
    assert_eq!(runtime.code, RuntimeErrorCode::LlmProfileUnavailable);
    assert_eq!(runtime.profile_key(), Some(&key));
    assert!(runtime.is_retryable());
    assert!(!runtime.is_terminal());
    assert_eq!(runtime.attempt_failure_text(), failure);

    for message in [
        format!("Handler failed with retryable error: {failure}"),
        "llm_profile_unavailable: model fast is unavailable".to_string(),
        r#"failed {"lash.error":{"code":"llm_profile_unavailable","message":"unbound"}}"#
            .to_string(),
        r#"failed {"lash.error":{"code":"llm_profile_unavailable"}}"#.to_string(),
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

#[test]
fn plugin_transition_is_journaled_with_attempt_faults_retried() {
    let scope = lash_core::ExecutionScope::turn("transition-owner", "run");
    let address = lash_core::EffectAddress::new(scope, "plugin-transition").unwrap();
    let request = lash_core::plugin::PluginTransitionRequest {
        id: lash_core::plugin::PluginTransitionId(address.clone()),
        owner: lash_core::RuntimeOwner::Session("transition-owner".into()),
        base: lash_core::plugin::PluginTransitionBase::Session {
            head: lash_core::store::SessionHeadRef {
                generation: 0,
                revision: 0,
                leaf: None,
                checkpoint: None,
            },
        },
        target: Default::default(),
    };
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            address,
            lash_core::RuntimeAttribution::for_session("transition-owner"),
            "transition",
        ),
        RuntimeEffectCommand::TransitionPlugins {
            request: Box::new(request),
        },
    );
    let canonical = envelope.stable_hash().unwrap();
    let execution::RestateEffectExecution::JournaledRun {
        envelope,
        engine_faults: EngineFaults::Retried,
    } = execution::restate_effect_execution(envelope).unwrap()
    else {
        panic!("transition must record its result and retry attempt faults");
    };
    assert_eq!(envelope.stable_hash().unwrap(), canonical);
    let fault = RuntimeEffectControllerError::new(
        RuntimeErrorCode::PluginSessionManager,
        "store unavailable",
    )
    .retryable_uncommitted_derivation();
    assert!(
        fault
            .journal_disposition(lash_core::RuntimeEffectKind::TransitionPlugins)
            .is_retryable_derivation()
    );
}
