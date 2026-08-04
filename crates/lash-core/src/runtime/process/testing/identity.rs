use serde_json::json;

use super::super::model::{
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessIdentity, ProcessInput,
    ProcessListFilter, ProcessListMode, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessStatus, RecoveryDisposition, WaitKind, WaitState,
};

#[test]
fn process_execution_env_identity_golden_corpus() {
    let spec = ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::default(),
    );
    let bytes = spec.to_store_bytes().expect("encode golden env");
    let actual = (
        String::from_utf8(bytes).expect("env bytes are JSON"),
        spec.stable_ref()
            .expect("derive golden env ref")
            .to_string(),
    );
    assert_eq!(
        actual,
        (
            "{\"plugin_options\":{},\"policy\":{\"model\":{\"id\":\"\",\"variant\":\"provider_default\",\"limits\":{\"context_window_tokens\":1}},\"provider_id\":\"\",\"session_id\":null,\"autonomous\":false,\"max_turns\":null}}".to_string(),
            "process-env:v2:sha256:02f6585f92aa774919cc2d3b51f1853eb4ff3d9b25d441936d7742ed58f8ba7e".to_string(),
        )
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
