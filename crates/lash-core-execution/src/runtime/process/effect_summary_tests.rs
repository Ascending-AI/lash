use super::*;

fn occurrence(node: &str, occurrence: u64) -> ProcessEffectOccurrence {
    ProcessEffectOccurrence::new(
        lash_sansio::WorkflowOccurrence::fixture(node, occurrence),
        "fixture.operation",
        ProcessEffectOutcomeClass::Success,
        None,
        format!("effect:{node}:{occurrence}"),
        crate::FleetFormat::current(),
    )
}

#[test]
fn the_append_payload_is_the_serde_encoding_and_round_trips() {
    let outcome = ProcessEffectOccurrence::new(
        lash_sansio::WorkflowOccurrence::fixture("node", 3),
        "fixture.disable",
        ProcessEffectOutcomeClass::Failure,
        Some(lash_sansio::FailureCode::lash(
            lash_sansio::TurnFailureCode::from_wire("fixture_conflict"),
        )),
        "effect:node:3",
        crate::FleetFormat::current(),
    );
    let request = outcome.append_request();
    assert_eq!(
        request.fact.payload(),
        serde_json::to_value(&outcome).unwrap()
    );
    assert_eq!(request.fact.payload()["code"], "lash:fixture_conflict");
    assert_eq!(
        request.replay.as_ref().map(|replay| replay.key.as_str()),
        Some("effect:node:3")
    );
    assert_eq!(
        ProcessEffectOccurrence::decode(request.fact.payload(), crate::FleetFormat::current())
            .unwrap(),
        outcome
    );
}

#[test]
fn only_failed_effects_can_carry_failure_codes() {
    let fleet = crate::FleetFormat::current();
    let schema = effect_outcome_payload_schema();
    for class in [
        ProcessEffectOutcomeClass::Success,
        ProcessEffectOutcomeClass::Failure,
        ProcessEffectOutcomeClass::Cancelled,
    ] {
        for code in [
            None,
            Some(lash_sansio::FailureCode::lash(
                lash_sansio::TurnFailureCode::from_wire("fixture_conflict"),
            )),
        ] {
            let allowed = class == ProcessEffectOutcomeClass::Failure || code.is_none();
            let outcome = ProcessEffectOccurrence::new(
                lash_sansio::WorkflowOccurrence::fixture("node", 1),
                "fixture.disable",
                class,
                code,
                "effect:node:1",
                fleet,
            );
            let request = outcome.append_request();
            let mut report = ProcessEffectReport::default();
            let result = report.fold_event(&request.fact, fleet);
            if allowed {
                result.expect("a valid class-and-code pair must fold");
                assert_eq!(report.node("node").unwrap().occurrences, vec![outcome]);
                schema.validate(&request.fact.payload()).unwrap();
            } else {
                assert!(
                    matches!(result, Err(ProcessEffectReportError::InvalidPayload(_))),
                    "{class:?} must refuse a failure code: {result:?}"
                );
                assert_eq!(
                    report.nodes().len(),
                    0,
                    "an invalid event changes no report"
                );
                assert!(
                    serde_json::from_value::<ProcessEffectOccurrence>(request.fact.payload())
                        .is_err(),
                    "direct deserialization must enforce the same rule"
                );
                assert!(schema.validate(&request.fact.payload()).is_err());
            }
        }
    }
}

#[test]
fn decode_refuses_other_versions_unknown_fields_and_a_zero_occurrence() {
    let mut payload = occurrence("node", 1).append_request().fact.payload();
    payload["vocabulary_version"] = serde_json::json!(0);
    assert!(matches!(
        ProcessEffectOccurrence::decode(payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::UnsupportedVocabularyVersion { actual: 0, .. })
    ));

    let mut payload = occurrence("node", 1).append_request().fact.payload();
    payload["unknown"] = serde_json::json!(true);
    assert!(matches!(
        ProcessEffectOccurrence::decode(payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::InvalidPayload(_))
    ));

    // A site's occurrences count from 1, with no ceiling of their own: the
    // per-node cap is the writer's count of what it recorded.
    let mut payload = occurrence("node", 1).append_request().fact.payload();
    payload["at"]["occurrence"] = serde_json::json!(0);
    assert!(matches!(
        ProcessEffectOccurrence::decode(payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::InvalidPayload(_))
    ));
    let beyond = PROCESS_EFFECT_OCCURRENCE_CAP + 1;
    assert_eq!(
        ProcessEffectOccurrence::decode(
            occurrence("node", beyond).append_request().fact.payload(),
            crate::FleetFormat::current()
        )
        .unwrap()
        .at
        .occurrence
        .get(),
        beyond
    );
}

#[test]
fn omission_records_are_strict() {
    let empty = ProcessEffectOmissions::new(BTreeMap::new(), crate::FleetFormat::current());
    assert!(matches!(
        ProcessEffectOmissions::decode(
            empty.append_request("omissions").fact.payload(),
            crate::FleetFormat::current()
        ),
        Err(ProcessEffectReportError::EmptyOmissions)
    ));
    let mut counts = ProcessEffectOmittedCounts::default();
    counts.record(ProcessEffectOutcomeClass::Failure);
    let mut omissions = ProcessEffectOmissions::new(
        BTreeMap::from([("node".to_string(), counts)]),
        crate::FleetFormat::current(),
    );
    for cap in 0..=PROCESS_EFFECT_OCCURRENCE_CAP {
        omissions.occurrence_cap = cap;
        let decoded = ProcessEffectOmissions::decode(
            omissions.append_request("omissions").fact.payload(),
            crate::FleetFormat::current(),
        )
        .unwrap();
        assert_eq!(decoded.occurrence_cap, cap);
    }
    omissions.occurrence_cap = PROCESS_EFFECT_OCCURRENCE_CAP + 1;
    assert!(matches!(
        ProcessEffectOmissions::decode(
            omissions.append_request("omissions").fact.payload(),
            crate::FleetFormat::current()
        ),
        Err(ProcessEffectReportError::UnsupportedOccurrenceCap { .. })
    ));
}

#[test]
fn the_fold_reads_the_written_bound_in_any_page_order() {
    let mut counts = ProcessEffectOmittedCounts::default();
    counts.record(ProcessEffectOutcomeClass::Success);
    counts.record(ProcessEffectOutcomeClass::Failure);
    counts.record(ProcessEffectOutcomeClass::Failure);
    let events = [
        occurrence("node-b", 1).append_request(),
        occurrence("node-a", 2).append_request(),
        occurrence("node-a", 1).append_request(),
        ProcessEffectOmissions::new(
            BTreeMap::from([("node-a".to_string(), counts)]),
            crate::FleetFormat::current(),
        )
        .append_request("omissions"),
        ProcessEventAppendRequest::observer_added(
            &crate::process_id_for_test("fold"),
            &crate::SessionId::from("observer"),
            &crate::ProcessObserverBy::host("fold"),
        ),
    ];
    let mut forward = ProcessEffectReport::default();
    for request in &events {
        forward
            .fold_event(&request.fact, crate::FleetFormat::current())
            .unwrap();
    }
    let mut reverse = ProcessEffectReport::default();
    for request in events.iter().rev() {
        reverse
            .fold_event(&request.fact, crate::FleetFormat::current())
            .unwrap();
    }
    assert_eq!(forward, reverse);
    let node = forward.node("node-a").unwrap();
    assert_eq!(
        node.occurrences
            .iter()
            .map(|outcome| outcome.at.occurrence.get())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        node.omitted.failure, 2,
        "an omitted failure stays a failure"
    );
    assert_eq!(node.omitted.total(), 3);
    assert_eq!(forward.node("node-b").unwrap().omitted.total(), 0);
}

#[test]
fn effect_identifiers_and_positive_omissions_match_the_host_schemas() {
    let fleet = crate::FleetFormat::current();
    let schema = effect_outcome_payload_schema();
    for field in ["/at/site/node_id", "/operation", "/replay_key"] {
        let mut payload = occurrence("node", 1).append_request().fact.payload();
        *payload.pointer_mut(field).expect(field) = serde_json::json!("");
        assert!(schema.validate(&payload).is_err());
        assert!(
            ProcessEffectOccurrence::decode(payload.clone(), fleet).is_err(),
            "{field}"
        );
        assert!(serde_json::from_value::<ProcessEffectOccurrence>(payload).is_err());
        // A typed occurrence cannot hold an empty node id; the two strings
        // it still holds are checked when it is admitted.
        let mut typed = occurrence("node", 1);
        match field {
            "/at/site/node_id" => continue,
            "/operation" => typed.operation.clear(),
            _ => typed.replay_key.clear(),
        }
        assert!(typed.admit(fleet).is_err());
    }
    let mut zeroth = occurrence("node", 1).append_request().fact.payload();
    zeroth["at"]["occurrence"] = serde_json::json!(0);
    assert!(schema.validate(&zeroth).is_err());
    assert!(ProcessEffectOccurrence::decode(zeroth, fleet).is_err());
    for code in [serde_json::Value::Null, serde_json::json!("")] {
        let mut payload = occurrence("node", 1).append_request().fact.payload();
        payload["outcome_class"] = serde_json::json!("failure");
        payload["code"] = code;
        assert!(schema.validate(&payload).is_err());
        assert!(ProcessEffectOccurrence::decode(payload.clone(), fleet).is_err());
        assert!(serde_json::from_value::<ProcessEffectOccurrence>(payload).is_err());
    }
    let schema = effect_omissions_payload_schema();
    for node in ["", "node"] {
        for counts in [
            ProcessEffectOmittedCounts::default(),
            ProcessEffectOmittedCounts {
                success: 1,
                failure: 0,
                cancelled: 0,
            },
            ProcessEffectOmittedCounts {
                success: 0,
                failure: 1,
                cancelled: 0,
            },
            ProcessEffectOmittedCounts {
                success: 0,
                failure: 0,
                cancelled: 1,
            },
            ProcessEffectOmittedCounts {
                success: u64::MAX,
                failure: u64::MAX,
                cancelled: u64::MAX,
            },
        ] {
            let allowed = !node.is_empty() && counts.total() > 0;
            let typed =
                ProcessEffectOmissions::new(BTreeMap::from([(node.to_owned(), counts)]), fleet);
            let payload = serde_json::to_value(&typed).unwrap();
            assert_eq!(schema.validate(&payload).is_ok(), allowed, "{payload}");
            assert_eq!(typed.admit(fleet).is_ok(), allowed);
            assert_eq!(
                ProcessEffectOmissions::decode(payload.clone(), fleet).is_ok(),
                allowed
            );
            assert_eq!(
                serde_json::from_value::<ProcessEffectOmissions>(payload).is_ok(),
                allowed
            );
        }
    }
}
