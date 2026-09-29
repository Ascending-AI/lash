use super::*;

fn occurrence(node: &str, occurrence: u64) -> ProcessEffectOccurrence {
    ProcessEffectOccurrence::new(
        node,
        occurrence,
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
        "node",
        3,
        "triggers.disable",
        ProcessEffectOutcomeClass::Failure,
        Some(lash_sansio::FailureCode::lash(
            lash_sansio::TurnFailureCode::from_wire("trigger_conflict"),
        )),
        "effect:node:3",
        crate::FleetFormat::current(),
    );
    let request = outcome.append_request();
    assert_eq!(request.payload, serde_json::to_value(&outcome).unwrap());
    assert_eq!(request.payload["code"], "lash:trigger_conflict");
    assert_eq!(
        request.replay.as_ref().map(|replay| replay.key.as_str()),
        Some("effect:node:3")
    );
    assert_eq!(
        ProcessEffectOccurrence::decode(request.payload, crate::FleetFormat::current()).unwrap(),
        outcome
    );
}

#[test]
fn decode_refuses_other_versions_unknown_fields_and_uncapped_occurrences() {
    let mut payload = occurrence("node", 1).append_request().payload;
    payload["vocabulary_version"] = serde_json::json!(0);
    assert!(matches!(
        ProcessEffectOccurrence::decode(payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::UnsupportedVocabularyVersion { actual: 0, .. })
    ));

    let mut payload = occurrence("node", 1).append_request().payload;
    payload["unknown"] = serde_json::json!(true);
    assert!(matches!(
        ProcessEffectOccurrence::decode(payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::InvalidPayload(_))
    ));

    let beyond = PROCESS_EFFECT_OCCURRENCE_CAP + 1;
    assert!(!ProcessEffectOccurrence::is_within_cap(beyond));
    assert!(!ProcessEffectOccurrence::is_within_cap(0));
    assert!(matches!(
        ProcessEffectOccurrence::decode(occurrence("node", beyond).append_request().payload, crate::FleetFormat::current()),
        Err(ProcessEffectReportError::OccurrenceOutsideCap { occurrence }) if occurrence == beyond
    ));
}

#[test]
fn omission_records_are_strict() {
    let empty = ProcessEffectOmissions::new(BTreeMap::new(), crate::FleetFormat::current());
    assert!(matches!(
        ProcessEffectOmissions::decode(
            empty.append_request("omissions").payload,
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
    omissions.occurrence_cap += 1;
    assert!(matches!(
        ProcessEffectOmissions::decode(
            omissions.append_request("omissions").payload,
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
        ProcessEventAppendRequest::new("process.custom", serde_json::json!({})),
    ];
    let mut forward = ProcessEffectReport::default();
    for request in &events {
        forward
            .fold_event(
                &request.event_type,
                &request.payload,
                crate::FleetFormat::current(),
            )
            .unwrap();
    }
    let mut reverse = ProcessEffectReport::default();
    for request in events.iter().rev() {
        reverse
            .fold_event(
                &request.event_type,
                &request.payload,
                crate::FleetFormat::current(),
            )
            .unwrap();
    }
    assert_eq!(forward, reverse);
    let node = forward.node("node-a").unwrap();
    assert_eq!(
        node.occurrences
            .iter()
            .map(|outcome| outcome.occurrence)
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
