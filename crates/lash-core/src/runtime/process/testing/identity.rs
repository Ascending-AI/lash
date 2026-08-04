use serde_json::json;

use super::super::model::{
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessIdentity, ProcessInput,
    ProcessListFilter, ProcessListMode, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessStatus, RecoveryDisposition, WaitKind, WaitState,
};

#[test]
fn process_execution_env_identity_golden_corpus() {
    let mut plugin_options = crate::PluginOptions::default();
    plugin_options
        .plugins
        .insert("a:b".to_string(), serde_json::json!({"enabled": true}));
    let policy = crate::SessionPolicy {
        model: crate::ModelSpec::from_token_limits(
            "model:rich",
            crate::ReasoningSelection::Effort("high".to_string()),
            8192,
            Some(2048),
        )
        .expect("valid rich model limits")
        .with_capability(crate::ModelCapability {
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
        }),
        provider_id: "provider".to_string(),
        session_id: Some("session".to_string()),
        autonomous: true,
        max_turns: Some(0),
        prompt: crate::PromptLayer::with_template(crate::PromptTemplate::new(vec![])),
        generation: crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(1024),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
            seed: Some(-7),
        },
    };
    let specs = [
        ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::default(),
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
                "{\"plugin_options\":{},\"policy\":{\"model\":{\"id\":\"\",\"variant\":\"provider_default\",\"limits\":{\"context_window_tokens\":1}},\"provider_id\":\"\",\"session_id\":null,\"autonomous\":false,\"max_turns\":null}}".to_string(),
                "process-env:v2:sha256:02f6585f92aa774919cc2d3b51f1853eb4ff3d9b25d441936d7742ed58f8ba7e".to_string(),
            ),
            (
                "{\"plugin_options\":{\"plugins\":{\"a:b\":{\"enabled\":true}}},\"policy\":{\"model\":{\"id\":\"model:rich\",\"variant\":{\"effort\":\"high\"},\"limits\":{\"context_window_tokens\":8192,\"output_token_capacity\":2048},\"capability\":{\"reasoning\":{\"efforts\":[\"low\",\"high\"],\"default_effort\":\"low\",\"aliases\":{\"max\":\"high\"},\"encoding\":{\"budget\":{\"high\":1024,\"low\":256}},\"disable\":\"toggle_false\",\"mandatory\":true},\"cache_control\":\"anthropic\",\"stream_termination\":\"eof_tolerated\",\"sampling\":\"pinned\"}},\"provider_id\":\"provider\",\"session_id\":\"session\",\"autonomous\":true,\"max_turns\":0,\"prompt\":{\"template\":{\"sections\":[]}},\"generation\":{\"output_token_cap\":1024,\"temperature\":0.25,\"seed\":-7}}}".to_string(),
                "process-env:v2:sha256:a1f7bed1cd486042c887b946c15a8bcec8bfcc21b88a14f18698c76c2098a5db".to_string(),
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
    process_id: &str,
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
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        )
        .with_identity(
            ProcessIdentity::new("test-engine")
                .with_label(Some(process_name))
                .with_definition(Some(definition)),
        )
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:test:{process_id}"
        )))),
    );
    record.status = status;
    record
}

#[test]
fn process_list_filter_matches_waiting_facet() {
    let process_ref = process_value("target", 0, "target");
    let mut waiting_entry = engine_entry(
        "waiting",
        process_ref.clone(),
        "target",
        ProcessStatus::Running,
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
    let idle_entry = engine_entry("idle", process_ref, "target", ProcessStatus::Running);
    let waiting_filter =
        ProcessListFilter::decode(&json!({ "waiting": true })).expect("decode waiting filter");
    let idle_filter =
        ProcessListFilter::decode(&json!({ "waiting": false })).expect("decode idle filter");

    assert_eq!(waiting_filter.list_mode(), ProcessListMode::Live);
    assert!(waiting_filter.matches_record(&waiting_entry));
    assert!(!waiting_filter.matches_record(&idle_entry));
    assert!(!idle_filter.matches_record(&waiting_entry));
    assert!(idle_filter.matches_record(&idle_entry));
    assert!(
        ProcessListFilter::decode(&json!({ "waiting": "yes" }))
            .expect_err("invalid waiting filter")
            .contains("must be a boolean")
    );
}
