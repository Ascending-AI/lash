use crate::ProcessId;
use crate::SessionId;
use serde_json::json;

use super::super::model::{
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessIdentity, ProcessInput,
    ProcessListFilter, ProcessListMode, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessStatus, WaitKind, WaitState,
};

#[test]
fn process_execution_env_identity_golden_corpus() {
    let mut plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
    plugin_config.insert("a:b", serde_json::json!({"enabled": true}));
    let plugin_config = crate::AdmittedPluginConfig::new(plugin_config, 3);
    let policy = crate::SessionPolicy {
        charge_safety: Default::default(),
        model: Some(
            crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
                crate::LlmProfileKey::new("rich-key"),
                crate::LlmProfileMetadata::builder("model:rich")
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
        ),
        attachment_acceptance: Default::default(),
        session_id: Some(SessionId::from("session")),
        autonomous: true,
        turn_budget: crate::TurnBudget::bounded(1),
        max_tool_calls: crate::MaxToolCalls::new(1024),
        no_progress_budget: Default::default(),
        generation: crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(1024),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
            stop_sequences: Vec::new(),
            parallel_tool_calls: None,
            projection_provenance: Default::default(),
        },
    };
    let specs = [
        ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
        ProcessExecutionEnvSpec::new(plugin_config, policy),
    ];
    let actual = specs.map(|spec| {
        let bytes = spec.to_store_bytes().expect("encode golden env");
        (
            String::from_utf8(bytes).expect("env bytes are JSON"),
            spec.stable_ref()
                .expect("derive golden env ref")
                .to_string(),
        )
    });
    assert_eq!(
        actual,
        [
            (
                "{\"plugin_config\":{\"revision\":0,\"config\":{}},\"policy\":{\"session_id\":null,\"autonomous\":false,\"turn_budget\":\"unbounded\",\"max_tool_calls\":1024}}".to_string(),
                "process-env:v6:blake3:359aa9b669b4a7fb3a7a471a622a7d316cda72d3c5f9adbb8f48738103447383".to_string(),
            ),
            (
                r#"{"plugin_config":{"revision":3,"config":{"protocol":"protocol","namespaces":{"a:b":{"format_version":1,"value":{"enabled":true}}}}},"policy":{"model":{"model":{"key":"rich-key","metadata":{"wire_model":"model:rich","limits":{"context_window_tokens":8192,"output_token_capacity":2048},"capability":{"instruction_role":"developer","native_mid_conversation_system":true,"cache_control":"anthropic","stream_termination":"eof_tolerated","sampling":"pinned","reasoning":{"efforts":["low","high"],"encoding":{"budget":{"high":1024,"low":256}},"disable":true,"mandatory":true}}}},"reasoning":{"effort":"high"}},"session_id":"session","autonomous":true,"turn_budget":{"bounded":1},"max_tool_calls":1024,"generation":{"output_token_cap":1024,"temperature":0.25,"seed":-7}}}"#.to_string(),
                "process-env:v6:blake3:941fc994f65ac26fe5da636106bfd9082c3d40e3e654a0f20934481e3e3245c7".to_string(),
            ),
        ]
    );
}

fn process_value(component: &str, pos: usize, name: &str) -> serde_json::Value {
    json!({
        "component": component,
        "pos": pos,
        "name": name,
    })
}

fn engine_entry(
    process_id: &ProcessId,
    definition: serde_json::Value,
    process_name: &str,
    status: ProcessStatus,
) -> ProcessRecord {
    let mut record = ProcessRecord::from_registration(
        ProcessRegistration::new(
            ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: json!({
                    "definition": definition.clone(),
                    "label": process_name,
                }),
            },
            ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
        .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
            ProcessIdentity::for_definition(
                crate::ProcessDefinitionRef::unclaimed("test-engine", definition),
                Some(process_name),
            ),
        ))
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:test:{process_id}"
        )))),
        process_id.clone(),
    );
    record.lifecycle = crate::ProcessLifecycleState::fixture(status);
    record
}

#[test]
fn process_list_filter_matches_status_sets_and_the_non_waiting_complement() {
    let process_id = process_value("target", 0, "target");
    let mut waiting_entry = engine_entry(
        &crate::process_id_for_test("waiting"),
        process_id.clone(),
        "target",
        ProcessStatus::Waiting,
    );
    waiting_entry.lifecycle = crate::ProcessLifecycleState::Waiting {
        wait: WaitState {
            since_ms: 42,
            kind: WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:waiting:signal.ready:1".to_string(),
                ordinal: 1,
            },
        },
        park: None,
    };
    let idle_entry = engine_entry(
        &crate::process_id_for_test("idle"),
        process_id,
        "target",
        ProcessStatus::Running,
    );
    let waiting_filter = ProcessListFilter::decode(&json!({ "status": {"in": ["waiting"]} }))
        .expect("decode waiting filter");
    let idle_filter =
        ProcessListFilter::decode(&json!({ "status": {"in": ["running", "completed", "failed", "cancelled", "abandoned", "caller_departed"]} })).expect("decode idle filter");

    assert_eq!(waiting_filter.list_mode(), ProcessListMode::Live);
    assert!(waiting_filter.matches_record(&waiting_entry));
    assert!(!waiting_filter.matches_record(&idle_entry));
    assert!(!idle_filter.matches_record(&waiting_entry));
    assert!(idle_filter.matches_record(&idle_entry));
    assert!(
        ProcessListFilter::decode(&json!({ "waiting": "yes" }))
            .expect_err("invalid waiting filter")
            .contains("unknown filter")
    );
}
