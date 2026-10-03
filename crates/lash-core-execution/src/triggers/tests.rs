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
