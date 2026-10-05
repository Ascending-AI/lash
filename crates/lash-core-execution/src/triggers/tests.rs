use super::*;

#[tokio::test]
async fn trigger_registration_refuses_non_engine_target() {
    let targets = [
        crate::ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        crate::ProcessInput::SessionTurn {
            definition_key: "child".to_string(),
            create_request: Box::new(crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )),
            turn_input: Box::new(crate::TurnInput::empty()),
            result: crate::SessionTurnOutcome::Turn,
        },
    ];
    for target in targets {
        let expected = target.engine_kind();
        let mut draft = incarnation_fixture_draft();
        draft.target = target.into();
        let registry = crate::ProcessEngineRegistry::default();
        for error in [
            draft
                .validate()
                .expect_err("store registration refuses the target"),
            admit_trigger_registration_target(&registry, &mut draft)
                .await
                .expect_err("engine admission refuses the target"),
        ] {
            assert!(
                matches!(error, PluginError::InvalidTriggerTarget { kind } if kind == expected)
            );
        }
    }
}

#[test]
fn raced_occurrence_delete_requires_reinspection_instead_of_claiming_emptiness() {
    let report = TriggerOccurrenceReclamationReport {
        inspected_occurrence_count: 1,
        reinspection_deferred_count: 1,
        ..TriggerOccurrenceReclamationReport::default()
    };

    assert_eq!(
        crate::store::MaintenanceReport::sweep(&report),
        crate::store::MaintenanceSweep::Incomplete
    );
    assert_eq!(
        report.reclaimed_occurrence_count
            + report.live_fan_out_count
            + report.grace_deferred_count
            + report.reinspection_deferred_count
            + report.audit_retained_count,
        report.inspected_occurrence_count
    );
}

#[test]
fn retained_audit_rows_are_not_a_blocker_in_the_reclamation_sweep() {
    let report = TriggerOccurrenceReclamationReport {
        inspected_occurrence_count: 1,
        audit_retained_count: 1,
        ..TriggerOccurrenceReclamationReport::default()
    };

    assert_eq!(
        crate::store::MaintenanceReport::sweep(&report),
        crate::store::MaintenanceSweep::NothingToDo,
        "durable audit history is not stuck fan-out and must not report an incomplete sweep"
    );
}

fn incarnation_fixture_draft() -> TriggerSubscriptionDraft {
    TriggerSubscriptionDraft::for_process(
        "sub",
        crate::ProcessExecutionEnvRef::new("env"),
        "source",
        "key",
        crate::ProcessInput::Engine {
            kind: "engine".to_string(),
            payload: serde_json::json!({"payload": 0}),
        },
        crate::ProcessIdentity::new("kind"),
    )
}

/// S10 F2: a stored mutation has one incarnation, owned by its record.
#[test]
fn mutation_receipt_refuses_a_second_incarnation() {
    let outcome = evaluate_trigger_mutation_with_incarnation(
        None,
        TriggerCommand::Register {
            owner_scope: TriggerOwnerScope::Session {
                session_id: SessionId::from("session"),
            },
            actor: crate::ProcessOriginator::Session {
                session_id: SessionId::from("session"),
                agent_frame_id: None,
            },
            draft: incarnation_fixture_draft(),
        },
        1,
        "incarnation-a".to_string(),
    )
    .expect("valid registration")
    .expect("mutation");
    let TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("mutation")
    };
    let mut stored = serde_json::to_value(&receipt).expect("stored receipt");
    assert_eq!(stored.as_object().expect("receipt record").len(), 2);
    assert_eq!(
        stored["record"]["incarnation"],
        serde_json::json!(receipt.incarnation())
    );
    let restored: TriggerMutationReceipt =
        serde_json::from_value(stored.clone()).expect("canonical receipt");
    assert_eq!(restored, *receipt);
    let projected = trigger_handle_outcome_value(&restored).expect("public handle");
    assert_eq!(projected["incarnation"], stored["record"]["incarnation"]);
    assert!(projected.get("record").is_none());
    assert!(projected.get("env_ref").is_none());
    assert!(projected.get("source_capture").is_none());
    stored["incarnation"] = serde_json::json!("incarnation-b");
    assert!(
        serde_json::from_value::<TriggerMutationReceipt>(stored).is_err(),
        "a stored receipt must refuse a contradictory top-level incarnation"
    );
}
