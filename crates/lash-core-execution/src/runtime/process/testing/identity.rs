use crate::ProcessId;
use serde_json::json;

use super::super::model::{
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessIdentity, ProcessInput,
    ProcessListFilter, ProcessListMode, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessStatus, WaitKind, WaitState,
};

fn process_execution_env_identity_corpus() -> [(String, String); 2] {
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

#[test]
fn process_execution_env_identity_golden_corpus() {
    let expected: [(String, String); 2] =
        serde_json::from_str(include_str!("fixtures/process_execution_env_identity.json"))
            .expect("generated identity corpus");
    assert_eq!(process_execution_env_identity_corpus(), expected);
}

#[test]
#[ignore = "prints the corpus for scripts/generate-process-env-identity-golden.py"]
fn regenerate_process_execution_env_identity_golden_corpus() {
    println!(
        "PROCESS_ENV_IDENTITY_GOLDEN {}",
        serde_json::to_string(&process_execution_env_identity_corpus()).expect("encode corpus")
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
