//! `resolve_generation_policy`: every host setting is sent or refused before
//! any I/O, and the receipt joins resolution with adapter emission (ADR 0121).
use super::*;

fn open_wire() -> GenerationWire {
    GenerationWire {
        label: "Test Wire",
        output_token_cap: OutputCapWire::Optional,
        temperature: true,
        seed: true,
        stop_sequences: true,
        parallel_tool_calls: true,
        thinking_summary: ThinkingSummaryWire::Always,
        active_thinking_pins_sampling: false,
    }
}

fn generation_request(generation: GenerationOptions) -> LlmRequest {
    LlmRequest {
        generation,
        ..empty_request()
    }
}

fn refusal_code(error: &LlmTransportError) -> Option<TurnFailureCode> {
    error.code.as_ref().and_then(FailureCode::turn_code)
}

#[test]
fn generation_policy_prefers_request_then_provider_and_invents_no_cap() {
    let provider_options = ProviderOptions {
        max_output_tokens: Some(8_192),
        cache_retention: CacheRetention::Long,
        expose_thinking: true,
        ..ProviderOptions::default()
    };
    let unset = resolve_generation_policy(
        &empty_request(),
        &ProviderOptions::default(),
        "test",
        &open_wire(),
    )
    .expect("nothing to refuse");
    assert_eq!(unset.max_output_tokens, None, "lash invents no cap");
    assert_eq!(unset.cache_retention, CacheRetention::Short);
    assert!(!unset.expose_thinking);
    assert_eq!(unset.temperature, None);
    assert_eq!(unset.seed, None);
    assert_eq!(unset.parallel_tool_calls, None);
    assert_eq!(unset.reasoning, None);

    let provider_limited =
        resolve_generation_policy(&empty_request(), &provider_options, "test", &open_wire())
            .expect("provider cap");
    assert_eq!(provider_limited.max_output_tokens, Some(8_192));
    assert_eq!(provider_limited.cache_retention, CacheRetention::Long);
    assert!(provider_limited.expose_thinking);

    let request_limited = resolve_generation_policy(
        &generation_request(GenerationOptions {
            output_token_cap: NonZeroUsize::new(2_048),
            temperature: Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
            parallel_tool_calls: Some(false),
            ..GenerationOptions::default()
        }),
        &provider_options,
        "test",
        &open_wire(),
    )
    .expect("request cap");
    assert_eq!(request_limited.max_output_tokens, Some(2_048));
    assert_eq!(
        request_limited.temperature.map(|value| value.get()),
        Some(0.25)
    );
    assert_eq!(request_limited.seed, Some(-7));
    assert_eq!(request_limited.parallel_tool_calls, Some(false));
}

#[test]
fn generation_policy_refuses_every_setting_the_wire_cannot_carry() {
    struct Case {
        name: &'static str,
        generation: GenerationOptions,
        options: ProviderOptions,
        wire: GenerationWire,
        code: TurnFailureCode,
    }
    let temperature = || Some(NonNegativeFiniteF64::new(0.5).expect("finite"));
    let cases = [
        Case {
            name: "cap on a wire without one",
            generation: GenerationOptions {
                output_token_cap: NonZeroUsize::new(1_024),
                ..GenerationOptions::default()
            },
            options: ProviderOptions::default(),
            wire: GenerationWire {
                output_token_cap: OutputCapWire::Unsupported,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
        Case {
            name: "provider cap on a wire without one",
            generation: GenerationOptions::default(),
            options: ProviderOptions {
                max_output_tokens: Some(1_024),
                ..ProviderOptions::default()
            },
            wire: GenerationWire {
                output_token_cap: OutputCapWire::Unsupported,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
        Case {
            name: "no cap on a wire that requires one",
            generation: GenerationOptions::default(),
            options: ProviderOptions::default(),
            wire: GenerationWire {
                output_token_cap: OutputCapWire::Required,
                ..open_wire()
            },
            code: TurnFailureCode::OutputTokenCapRequired,
        },
        Case {
            name: "temperature on a wire without one",
            generation: GenerationOptions {
                temperature: temperature(),
                ..GenerationOptions::default()
            },
            options: ProviderOptions::default(),
            wire: GenerationWire {
                temperature: false,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
        Case {
            name: "seed on a wire without one",
            generation: GenerationOptions {
                seed: Some(1),
                ..GenerationOptions::default()
            },
            options: ProviderOptions::default(),
            wire: GenerationWire {
                seed: false,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
        Case {
            name: "stop sequences on a wire without them",
            generation: GenerationOptions {
                stop_sequences: vec!["END".to_string()],
                ..GenerationOptions::default()
            },
            options: ProviderOptions::default(),
            wire: GenerationWire {
                stop_sequences: false,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
        Case {
            name: "parallel tool calls on a wire without them",
            generation: GenerationOptions {
                parallel_tool_calls: Some(true),
                ..GenerationOptions::default()
            },
            options: ProviderOptions::default(),
            wire: GenerationWire {
                parallel_tool_calls: false,
                ..open_wire()
            },
            code: TurnFailureCode::UnsupportedGenerationOption,
        },
    ];
    for case in cases {
        let error = resolve_generation_policy(
            &generation_request(case.generation),
            &case.options,
            "test",
            &case.wire,
        )
        .expect_err(case.name);
        assert_eq!(refusal_code(&error), Some(case.code), "{}", case.name);
        assert_eq!(
            error.retry_verdict,
            TransportRetryVerdict::Forbidden,
            "{}",
            case.name
        );
    }
}

#[test]
fn pinned_sampling_refuses_a_set_temperature_on_every_wire() {
    let mut pinned = generation_request(GenerationOptions {
        temperature: Some(NonNegativeFiniteF64::new(0.2).expect("finite")),
        ..GenerationOptions::default()
    });
    pinned.model_capability.sampling = crate::provider::SamplingCapability::Pinned;
    for pins in [false, true] {
        let wire = GenerationWire {
            active_thinking_pins_sampling: pins,
            ..open_wire()
        };
        let error = resolve_generation_policy(&pinned, &ProviderOptions::default(), "test", &wire)
            .expect_err("pinned model");
        assert_eq!(
            refusal_code(&error),
            Some(TurnFailureCode::UnsupportedGenerationOption)
        );
    }

    // Active thinking pins sampling only where the wire says so, and only
    // for an effort or budget, not for reasoning turned off.
    let mut thinking = generation_request(GenerationOptions {
        temperature: Some(NonNegativeFiniteF64::new(0.2).expect("finite")),
        ..GenerationOptions::default()
    });
    thinking.model_capability.reasoning = Some(crate::provider::ReasoningCapability {
        efforts: vec!["high".to_string()],
        disable: true,
        ..crate::provider::ReasoningCapability::default()
    });
    let pinning_wire = GenerationWire {
        active_thinking_pins_sampling: true,
        ..open_wire()
    };
    thinking.model_variant = ReasoningSelection::Effort("high".to_string());
    let error = resolve_generation_policy(
        &thinking,
        &ProviderOptions::default(),
        "test",
        &pinning_wire,
    )
    .expect_err("active thinking pins sampling");
    assert_eq!(
        refusal_code(&error),
        Some(TurnFailureCode::UnsupportedGenerationOption)
    );
    resolve_generation_policy(&thinking, &ProviderOptions::default(), "test", &open_wire())
        .expect("a wire whose thinking does not pin sampling sends the temperature");
    thinking.model_variant = ReasoningSelection::Disabled;
    let off = resolve_generation_policy(
        &thinking,
        &ProviderOptions::default(),
        "test",
        &pinning_wire,
    )
    .expect("reasoning off does not pin sampling");
    assert_eq!(off.reasoning, Some(ReasoningIntent::Off));
}

#[test]
fn expose_thinking_is_local_visibility_and_a_wire_flag_only_where_one_exists() {
    let options = ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    };
    for (summary, expected) in [
        (ThinkingSummaryWire::NoField, false),
        (ThinkingSummaryWire::Always, true),
        (ThinkingSummaryWire::WithActiveThinking, false),
    ] {
        let wire = GenerationWire {
            thinking_summary: summary,
            ..open_wire()
        };
        let policy = resolve_generation_policy(&empty_request(), &options, "test", &wire)
            .expect("expose_thinking is never refused");
        assert!(policy.expose_thinking);
        assert_eq!(policy.request_thinking_summary, expected, "{summary:?}");
        let receipt = policy.receipt(
            &empty_request(),
            &GenerationEmission {
                thinking_summary: expected,
                ..GenerationEmission::default()
            },
        );
        assert_eq!(
            receipt.thinking_summary,
            if expected {
                GenerationOptionOutcome::Applied
            } else {
                GenerationOptionOutcome::NotRequested
            }
        );
        assert_eq!(
            receipt.thinking_visibility,
            GenerationOptionOutcome::Applied
        );
        assert!(receipt.fully_honored());
    }
}

#[test]
fn generation_policy_refuses_an_invalid_reasoning_selection() {
    let mut request = empty_request();
    request.model_variant = ReasoningSelection::Effort("high".to_string());
    let error =
        resolve_generation_policy(&request, &ProviderOptions::default(), "test", &open_wire())
            .expect_err("no reasoning capability");
    assert_eq!(
        refusal_code(&error),
        Some(TurnFailureCode::EffortNotConfigurable)
    );
}

#[test]
fn receipt_joins_requested_settings_with_adapter_emission() {
    let mut request = generation_request(GenerationOptions {
        temperature: Some(NonNegativeFiniteF64::new(0.2).expect("finite")),
        parallel_tool_calls: Some(true),
        ..GenerationOptions::default()
    });
    request.model_capability.reasoning = Some(crate::provider::ReasoningCapability {
        efforts: vec!["high".to_string()],
        ..crate::provider::ReasoningCapability::default()
    });
    request.model_variant = ReasoningSelection::Effort("high".to_string());
    let options = ProviderOptions {
        max_output_tokens: Some(4_096),
        expose_thinking: true,
        ..ProviderOptions::default()
    };
    let policy =
        resolve_generation_policy(&request, &options, "test", &open_wire()).expect("resolved");
    let receipt = policy.receipt(
        &request,
        &GenerationEmission {
            output_token_cap: true,
            temperature: true,
            parallel_tool_calls: true,
            reasoning: true,
            thinking_summary: true,
            ..GenerationEmission::default()
        },
    );
    assert_eq!(
        receipt,
        crate::GenerationReceipt {
            output_token_cap: GenerationOptionOutcome::Applied,
            temperature: GenerationOptionOutcome::Applied,
            seed: GenerationOptionOutcome::NotRequested,
            stop_sequences: GenerationOptionOutcome::NotRequested,
            cache: GenerationOptionOutcome::NotRequested,
            reasoning: GenerationOptionOutcome::Applied,
            parallel_tool_calls: GenerationOptionOutcome::Applied,
            thinking_summary: GenerationOptionOutcome::Applied,
            thinking_visibility: GenerationOptionOutcome::Applied,
        }
    );
    assert!(receipt.fully_honored());
    // An adapter that failed to emit what resolution accepted is reported
    // as an omission, never as sent.
    let defective = policy.receipt(&request, &GenerationEmission::default());
    assert_eq!(
        defective.reasoning,
        GenerationOptionOutcome::OmittedUnsupported
    );
    assert!(!defective.nothing_omitted());
}
