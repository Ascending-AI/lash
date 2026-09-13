use crate::ProcessId;
use crate::SessionId;
use serde_json::json;

use super::super::model::{
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessIdentity, ProcessIncarnation,
    ProcessInput, ProcessListFilter, ProcessListMode, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessStatus, RecoveryContract, WaitKind, WaitState,
};

#[test]
fn process_execution_env_identity_golden_corpus() {
    let mut plugin_options = crate::PluginOptions::default();
    plugin_options
        .plugins
        .insert("a:b".to_string(), serde_json::json!({"enabled": true}));
    let policy = crate::SessionPolicy {
        charge_safety: Default::default(),
        model: crate::ModelSpec::builder("model:rich")
            .variant(crate::ReasoningSelection::Effort("high".to_string()))
            .context_window_tokens(8192)
            .output_token_capacity(2048)
            .build()
            .expect("valid rich model limits")
            .with_capability(crate::ModelCapability {
                instruction_role: crate::InstructionRole::Developer,
                native_mid_conversation_system: true,
                attachment_acceptance: Default::default(),
                google_dialect: Default::default(),
                reasoning: Some(crate::ReasoningCapability {
                    efforts: vec!["low".to_string(), "high".to_string()],
                    default_effort: Some("low".to_string()),
                    aliases: std::collections::BTreeMap::from([(
                        "max".to_string(),
                        "high".to_string(),
                    )]),
                    encoding: crate::ReasoningEncoding::Budget(std::collections::BTreeMap::from([
                        ("low".to_string(), 256),
                        ("high".to_string(), 1024),
                    ])),
                    disable: Some(crate::ReasoningDisableEncoding::ToggleFalse),
                    mandatory: true,
                }),
                cache_control: Some(crate::CacheControlDialect::Anthropic),
                stream_termination: Some(crate::StreamTermination::EofTolerated),
                sampling: crate::SamplingCapability::Pinned,
                reasoning_retention: Default::default(),
            }),
        provider_id: "provider".to_string(),
        session_id: Some(SessionId::from("session")),
        autonomous: true,
        turn_budget: crate::TurnBudget::bounded(1),
        no_progress_budget: Default::default(),
        prompt: crate::PromptLayer::with_template(crate::PromptTemplate::new(vec![])),
        generation: crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(1024),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
            stop_sequences: Vec::new(),
            projection_provenance: Default::default(),
        },
    };
    let specs = [
        ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        ProcessExecutionEnvSpec::new(plugin_options, policy),
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
                "{\"plugin_options\":{},\"policy\":{\"model\":{\"id\":\"\",\"variant\":\"provider_default\",\"limits\":{\"context_window_tokens\":1}},\"provider_id\":\"\",\"session_id\":null,\"autonomous\":false,\"turn_budget\":\"unbounded\"}}".to_string(),
                "process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738".to_string(),
            ),
            (
                r#"{"plugin_options":{"plugins":{"a:b":{"enabled":true}}},"policy":{"model":{"id":"model:rich","variant":{"effort":"high"},"limits":{"context_window_tokens":8192,"output_token_capacity":2048},"capability":{"instruction_role":"developer","native_mid_conversation_system":true,"cache_control":"anthropic","stream_termination":"eof_tolerated","sampling":"pinned","reasoning":{"efforts":["low","high"],"default_effort":"low","aliases":{"max":"high"},"encoding":{"budget":{"high":1024,"low":256}},"disable":"toggle_false","mandatory":true}}},"provider_id":"provider","session_id":"session","autonomous":true,"turn_budget":{"bounded":1},"prompt":{"template":{"sections":[]}},"generation":{"output_token_cap":1024,"temperature":0.25,"seed":-7}}}"#.to_string(),
                "process-env:v6:blake3:7c2a6b64d1b7e20fb517db8720f073d40bfa41dfe99d160268cd3571f5e5dbac".to_string(),
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
            process_id,
            ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: json!({
                    "definition": definition.clone(),
                    "label": process_name,
                }),
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_identity(
            ProcessIdentity::new("test-engine")
                .with_label(Some(process_name))
                .with_definition(Some(definition)),
        )
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:test:{process_id}"
        )))),
        ProcessIncarnation::from_registration_sequence(1),
    );
    record.status = status;
    record
}

#[test]
fn process_list_filter_matches_status_sets_and_the_non_waiting_complement() {
    let process_ref = process_value("target", 0, "target");
    let mut waiting_entry = engine_entry(
        &ProcessId::from("waiting"),
        process_ref.clone(),
        "target",
        ProcessStatus::Waiting,
    );
    waiting_entry.wait = Some(WaitState {
        since_ms: 42,
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:waiting:signal.ready:1".to_string(),
            ordinal: 1,
        },
    });
    let idle_entry = engine_entry(
        &ProcessId::from("idle"),
        process_ref,
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

#[tokio::test]
async fn runtime_feedback_process_environment_refuses_prior_family() {
    use super::super::model::InMemoryProcessExecutionEnvStore;
    use crate::{
        ArtifactOwner, ProcessExecutionEnvStore, load_process_execution_env,
        publish_process_execution_env,
    };
    let store = InMemoryProcessExecutionEnvStore::new();
    let mut policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.model = crate::ModelSpec::builder("model")
        .context_window_tokens(100)
        .build()
        .unwrap()
        .with_capability(crate::ModelCapability {
            instruction_role: crate::InstructionRole::Developer,
            native_mid_conversation_system: true,
            ..Default::default()
        });
    let spec = ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy);
    let bytes = spec.to_store_bytes().unwrap();
    for (prefix, domain) in [
        ("process-env:v4:blake3:", "lash-process-env/v4"),
        ("process-env:v5:blake3:", "lash-process-env/v5"),
    ] {
        let old = ProcessExecutionEnvRef::new(format!(
            "{prefix}{}",
            crate::stable_hash::blake3_hex(domain, &bytes)
        ));
        assert!(
            store
                .publish_process_execution_env(&ArtifactOwner::host("version-test"), &old, &bytes)
                .await
                .unwrap_err()
                .to_string()
                .contains("do not match")
        );
        store.insert_raw_for_testing(old.clone(), bytes.clone());
        assert!(
            load_process_execution_env(&store, &old)
                .await
                .unwrap_err()
                .to_string()
                .contains("recreate")
        );
    }
    let current =
        publish_process_execution_env(&store, &ArtifactOwner::host("version-test"), &spec)
            .await
            .unwrap();
    assert!(current.as_str().starts_with("process-env:v6:blake3:"));
    assert_eq!(
        load_process_execution_env(&store, &current).await.unwrap(),
        spec
    );
}
