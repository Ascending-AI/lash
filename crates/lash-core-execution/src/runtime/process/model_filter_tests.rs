use crate::ProcessId;
use serde_json::json;

use super::model::{
    ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessListFilter, ProcessListMode,
    ProcessOriginator, ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessStatus,
    SessionScope,
};

fn record(process_id: &ProcessId, label: &str, created_at_ms: u64) -> ProcessRecord {
    let mut record = ProcessRecord::from_registration(
        ProcessRegistration::new(
            ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: json!({}),
            },
            ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
        .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
            ProcessIdentity::labelled("test-engine", Some(label)),
        ))
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:test:{process_id}"
        )))),
        process_id.clone(),
    );
    record.created_at_ms = created_at_ms;
    record
}

#[test]
fn process_identity_keeps_typed_engine_definitions_in_durable_encodings() {
    for signature in [
        crate::ProcessSignature::Unknown,
        crate::ProcessSignature::known(json!({"result": "string"})),
    ] {
        let reference = crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "retained"}),
            signature.clone(),
        );
        let identity = ProcessIdentity::for_definition(reference, Some("retained"));
        let stored = serde_json::to_value(&identity).expect("encode engine identity");
        assert_eq!(
            stored,
            json!({
                "kind": "test-engine",
                "label": "retained",
                "definition": {
                    "engine_kind": "test-engine",
                    "definition": {"program": "retained"},
                    "signature": signature,
                },
            }),
            "durable identity must keep the admitted engine value and signature"
        );
        let decoded: ProcessIdentity =
            serde_json::from_value(stored).expect("decode typed engine identity");
        assert_eq!(decoded, identity);
        let packed = rmp_serde::to_vec_named(&identity).expect("pack engine identity");
        let unpacked: ProcessIdentity =
            rmp_serde::from_slice(&packed).expect("unpack engine identity");
        assert_eq!(unpacked, identity);
    }

    for malformed in [
        json!({"kind": "test-engine", "definition": {"program": "retained"}}),
        json!({"kind": "test-engine", "unexpected": true}),
    ] {
        assert!(serde_json::from_value::<ProcessIdentity>(malformed).is_err());
    }
}

#[test]
fn retained_host_start_requires_the_same_persisted_engine_definition() {
    let reference = crate::ProcessDefinitionRef::new(
        "test-engine",
        json!({"program": "retained"}),
        crate::ProcessSignature::known(json!({"result": "string"})),
    );
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: json!({"program": "retained"}),
        },
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
    .with_start_key(Some(crate::StartKey::for_host("retained-engine-identity")))
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new("retained-engine-env")))
    .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::for_definition(reference, Some("retained")),
    ));
    let retained = ProcessRecord::from_registration(
        registration.clone(),
        crate::process_id_for_test("retained-engine-identity"),
    );
    let retained: ProcessRecord =
        serde_json::from_value(serde_json::to_value(retained).expect("persist retained process"))
            .expect("reopen retained process");
    super::validation::check_retained_start(&registration, &retained, None)
        .expect("the same host start returns the persisted process");

    for changed in [
        crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "different"}),
            crate::ProcessSignature::known(json!({"result": "string"})),
        ),
        crate::ProcessDefinitionRef::new(
            "test-engine",
            json!({"program": "retained"}),
            crate::ProcessSignature::known(json!({"result": "number"})),
        ),
    ] {
        let conflicting = registration.clone().with_admitted_identity(
            crate::AdmittedProcessIdentity::for_testing(ProcessIdentity::for_definition(
                changed,
                Some("retained"),
            )),
        );
        assert!(matches!(
            super::validation::check_retained_start(&conflicting, &retained, None),
            Err(crate::PluginError::StartKeyConflict { start_key })
                if registration.start_key.as_ref() == Some(&start_key)
        ));
    }
}

#[test]
fn process_identity_keeps_immutable_ids_separate_from_engine_definitions() {
    let id = crate::ProcessDefinitionId::from_sha256_digest([7; 32]);
    let mut identity = ProcessIdentity::new("test-engine");
    identity.definition_id = Some(id.clone());
    assert_eq!(
        serde_json::to_value(&identity).expect("encode id-only identity"),
        json!({"kind": "test-engine", "definition_id": id.to_tagged_json()})
    );
    identity.definition = Some(crate::ProcessDefinitionRef::unclaimed(
        "test-engine",
        json!({"program": "realized"}),
    ));
    let restored: ProcessIdentity = serde_json::from_value(
        serde_json::to_value(&identity).expect("encode realized definition identity"),
    )
    .expect("decode realized definition identity");
    assert_eq!(
        restored, identity,
        "the immutable id and admission facts both persist"
    );
}

#[test]
fn process_originator_host_scope_is_serde_compatible() {
    let old_host: ProcessOriginator =
        serde_json::from_value(json!({ "type": "host" })).expect("old host originator");
    assert_eq!(old_host, ProcessOriginator::host());
    assert_eq!(old_host.id(), "host");

    let scoped = ProcessOriginator::host_scoped("automation-a");
    assert_eq!(scoped.id(), "host:automation-a");
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
    let target_ref = crate::ProcessDefinitionId::from_sha256_digest([1; 32]);
    let other_ref = crate::ProcessDefinitionId::from_sha256_digest([2; 32]);
    let filter = ProcessListFilter::decode(&json!({
        "definition_id": target_ref.to_tagged_json(),
        "status": {"in": ["completed"]}
    }))
    .expect("decode filter");

    let mut matching = record(&crate::process_id_for_test("matching"), "target", 100);
    matching.identity.definition_id = Some(target_ref);
    matching.status = ProcessStatus::Completed;
    matching.outcome = Some(crate::ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::success(json!(true)),
    ));
    let mut wrong_definition = record(
        &crate::process_id_for_test("wrong-definition"),
        "other",
        100,
    );
    wrong_definition.identity.definition_id = Some(other_ref);
    wrong_definition.status = matching.status;

    assert_eq!(filter.list_mode(), ProcessListMode::All);
    assert!(filter.matches_record(&matching));
    assert!(!filter.matches_record(&wrong_definition));
}

#[test]
fn process_list_filter_matches_enriched_facets() {
    let mut matching = record(&crate::process_id_for_test("matching"), "target", 100);
    matching.provenance = ProcessProvenance::session(SessionScope::new("origin-session"))
        .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence-target".to_string(),
            subscription_id: Some("subscription-target".to_string()),
            subscription_incarnation: None,
            subscription_revision: None,
        }));
    let mut wrong_subscription = record(
        &crate::process_id_for_test("wrong-subscription"),
        "target",
        100,
    );
    wrong_subscription.provenance = ProcessProvenance::session(SessionScope::new("origin-session"))
        .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence-target".to_string(),
            subscription_id: Some("subscription-other".to_string()),
            subscription_incarnation: None,
            subscription_revision: None,
        }));
    let mut missing_subscription = record(
        &crate::process_id_for_test("missing-subscription"),
        "target",
        100,
    );
    missing_subscription.provenance = ProcessProvenance::session(SessionScope::new(
        "origin-session",
    ))
    .with_caused_by(Some(crate::CausalRef::TriggerOccurrence {
        occurrence_id: "occurrence-target".to_string(),
        subscription_id: None,
        subscription_incarnation: None,
        subscription_revision: None,
    }));
    let wrong = record(&crate::process_id_for_test("wrong"), "other", 200);

    let filter = ProcessListFilter::decode(&json!({
        "originator": {"type": "session", "session_id": "origin-session"},
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

#[test]
fn process_list_filter_keeps_live_rows_and_bounds_retired_rows() {
    let filter = ProcessListFilter::decode(&json!({
        "status": "any",
        "retired_since_ms": 100
    }))
    .expect("decode recently retired filter");

    let mut old_live = record(&crate::process_id_for_test("old-live"), "live", 1);
    old_live.updated_at_ms = 1;
    let mut fresh_terminal = record(&crate::process_id_for_test("fresh-terminal"), "fresh", 1);
    fresh_terminal.status = ProcessStatus::Completed;
    fresh_terminal.updated_at_ms = 100;
    let mut old_terminal = record(&crate::process_id_for_test("old-terminal"), "old", 1);
    old_terminal.status = ProcessStatus::Completed;
    old_terminal.updated_at_ms = 99;
    let mut old_caller_departed = record(
        &crate::process_id_for_test("old-caller-departed"),
        "departed",
        1,
    );
    old_caller_departed.status = ProcessStatus::CallerDeparted;
    old_caller_departed.updated_at_ms = 99;

    assert!(filter.matches_record(&old_live));
    assert!(filter.matches_record(&fresh_terminal));
    assert!(!filter.matches_record(&old_terminal));
    assert!(!filter.matches_record(&old_caller_departed));
}
