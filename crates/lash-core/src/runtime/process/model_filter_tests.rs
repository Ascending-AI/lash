use serde_json::json;

use super::model::{
    ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessListFilter, ProcessListMode,
    ProcessOriginator, ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessStatus,
    RecoveryDisposition, SessionScope,
};

fn record(process_id: &str, label: &str, created_at_ms: u64) -> ProcessRecord {
    let mut record = ProcessRecord::from_registration(
        ProcessRegistration::new(
            process_id,
            ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: json!({}),
            },
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        )
        .with_identity(ProcessIdentity::new("test-engine").with_label(Some(label)))
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:test:{process_id}"
        )))),
    );
    record.created_at_ms = created_at_ms;
    record
}

#[test]
fn process_originator_host_scope_is_serde_compatible() {
    let old_host: ProcessOriginator =
        serde_json::from_value(json!({ "type": "host" })).expect("old host originator");
    assert_eq!(old_host, ProcessOriginator::host());
    assert_eq!(old_host.scope_id(), "host");

    let scoped = ProcessOriginator::host_scoped("automation-a");
    assert_eq!(scoped.scope_id(), "host:automation-a");
    assert_eq!(
        serde_json::to_value(&scoped).expect("scoped host json"),
        json!({ "type": "host", "scope": "automation-a" })
    );

    let round_tripped: ProcessOriginator =
        serde_json::from_value(json!({ "type": "host", "scope": "automation-a" }))
            .expect("scoped host originator");
    assert_eq!(round_tripped, scoped);
}

#[test]
fn process_list_filter_matches_definition_and_status() {
    let target_ref = json!({ "component": "target", "pos": 0, "name": "target" });
    let other_ref = json!({ "component": "other", "pos": 1, "name": "other" });
    let filter = ProcessListFilter::decode(&json!({
        "definition": target_ref,
        "status": "completed"
    }))
    .expect("decode filter");

    let mut matching = record("matching", "target", 100);
    matching.identity.definition = Some(target_ref);
    matching.status = ProcessStatus::Completed;
    matching.outcome = Some(crate::ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::success(json!(true)),
    ));
    let mut wrong_definition = record("wrong-definition", "other", 100);
    wrong_definition.identity.definition = Some(other_ref);
    wrong_definition.status = matching.status;

    assert_eq!(filter.list_mode(), ProcessListMode::All);
    assert!(filter.matches_record(&matching));
    assert!(!filter.matches_record(&wrong_definition));
}

#[test]
fn process_list_filter_matches_enriched_facets() {
    let mut matching = record("matching", "target", 100);
    matching.provenance = ProcessProvenance::session(SessionScope::new("origin-session"))
        .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence-target".to_string(),
            subscription_id: Some("subscription-target".to_string()),
            subscription_incarnation: None,
            subscription_revision: None,
        }));
    let mut wrong_subscription = record("wrong-subscription", "target", 100);
    wrong_subscription.provenance = ProcessProvenance::session(SessionScope::new("origin-session"))
        .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence-target".to_string(),
            subscription_id: Some("subscription-other".to_string()),
            subscription_incarnation: None,
            subscription_revision: None,
        }));
    let mut missing_subscription = record("missing-subscription", "target", 100);
    missing_subscription.provenance = ProcessProvenance::session(SessionScope::new(
        "origin-session",
    ))
    .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
        occurrence_id: "occurrence-target".to_string(),
        subscription_id: None,
        subscription_incarnation: None,
        subscription_revision: None,
    }));
    let wrong = record("wrong", "other", 200);

    let filter = ProcessListFilter::decode(&json!({
        "originator_scope_id": "session:origin-session",
        "identity_kind": "test-engine",
        "identity_label": "target",
        "caused_by_occurrence_id": "occurrence-target",
        "created_at_start_ms": 100,
        "created_at_end_ms": 101
    }))
    .expect("decode enriched filter");
    assert!(filter.matches_record(&matching));
    assert!(!filter.matches_record(&wrong));

    let subscription_filter = ProcessListFilter::decode(&json!({
        "caused_by_subscription_id": "subscription-target"
    }))
    .expect("decode subscription filter");
    assert!(subscription_filter.matches_record(&matching));
    assert!(!subscription_filter.matches_record(&wrong_subscription));
    assert!(!subscription_filter.matches_record(&missing_subscription));
    assert!(!subscription_filter.matches_record(&wrong));
    assert!(
        ProcessListFilter::decode(&json!({ "identity_kind": true }))
            .expect_err("invalid identity kind")
            .contains("must be a string")
    );
    assert!(
        ProcessListFilter::decode(&json!({ "created_at_start_ms": "old" }))
            .expect_err("invalid created-at start")
            .contains("must be an integer")
    );
}
