const CORPUS: &str =
    "crates/lash-core-execution/src/runtime/process/testing/fixtures/process_env_identity.json";

fn process_env_identity_corpus() -> [(String, String); 2] {
    let model = crate::ProcessModel::new(
        crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
            crate::LlmProfileKey::new("rich-key"),
            crate::LlmProfileMetadata::builder("model:rich")
                .cache_retention(crate::provider::CacheRetention::Short)
                .context_window_tokens(8192)
                .output_token_capacity(2048)
                .build()
                .expect("valid rich model limits")
                .with_capability(crate::LlmProfileCapability {
                    instruction_role: crate::InstructionRole::Developer,
                    native_mid_conversation_system: true,
                    google_dialect: Default::default(),
                    reasoning: Some(crate::ReasoningCapability {
                        efforts: vec!["low".to_string(), "high".to_string()],
                        encoding: crate::ReasoningEncoding::Budget(
                            std::collections::BTreeMap::from([
                                ("low".to_string(), 256),
                                ("high".to_string(), 1024),
                            ]),
                        ),
                        disable: true,
                        mandatory: true,
                    }),
                    cache_control: Some(crate::CacheControlDialect::Anthropic),
                    stream_termination: Some(crate::StreamTermination::EofTolerated),
                    sampling: crate::SamplingCapability::Pinned,
                    reasoning_retention: Default::default(),
                }),
        ))
        .with_reasoning(crate::ReasoningSelection::Effort("high".to_string())),
        crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(1024),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
            stop_sequences: Vec::new(),
            parallel_tool_calls: None,
            projection_provenance: Default::default(),
        },
        Default::default(),
    );
    const CONFIGURED: crate::ProcessConfigKey<serde_json::Value> =
        crate::ProcessConfigKey::new("configured", crate::FormatVersion::ONE);
    let rich = crate::ProcessEnv::builder(crate::MaxToolCalls::new(1024))
        .model(model)
        .component(CONFIGURED, &serde_json::json!({"enabled": true}))
        .component_with_sessions(
            crate::ProcessConfigKey::<()>::new("reader", crate::FormatVersion::ONE),
            &(),
            [crate::SessionId::from("granted-session")],
        )
        .build()
        .expect("the rich environment builds");
    let specs = [
        lash_execution_contracts::testing::process_env_fixture(),
        rich,
    ];
    specs.map(|spec| {
        let bytes = spec.to_store_bytes().expect("encode golden env");
        (
            String::from_utf8(bytes).expect("env bytes are JSON"),
            spec.stable_ref()
                .expect("derive golden env ref")
                .to_string(),
        )
    })
}

/// A process environment's stored bytes and reference are its identity: a
/// change to either is a format change (FIG-6034 §8).
#[test]
fn process_env_identity_golden_corpus() {
    let expected: [(String, String); 2] =
        serde_json::from_str(include_str!("fixtures/process_env_identity.json"))
            .expect("generated identity corpus");
    assert_eq!(process_env_identity_corpus(), expected);
}

#[test]
#[ignore = "regenerates crates/lash-core-execution/src/runtime/process/testing/fixtures/process_env_identity.json"]
#[expect(
    clippy::disallowed_methods,
    reason = "the regenerator reads its switch and the workspace root, and writes the corpus"
)]
fn regenerate_process_env_identity_golden_corpus() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root = std::env::var("BUILD_WORKSPACE_DIRECTORY").expect("the workspace root");
    let corpus =
        serde_json::to_string_pretty(&process_env_identity_corpus()).expect("the corpus encodes");
    std::fs::write(std::path::Path::new(&root).join(CORPUS), corpus + "\n")
        .expect("the corpus is written");
}
