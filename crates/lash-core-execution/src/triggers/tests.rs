use super::*;

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

fn mutation_receipt(result: TriggerEffectResult) -> TriggerMutationReceipt {
    match result.expect("the mutation commits") {
        TriggerCommandOutcome::Mutation { receipt } => *receipt,
        other => panic!("expected one mutation receipt, got {other:?}"),
    }
}

/// ADR 0113 §1: the revision a command commits is known before it commits.
/// `Register` and `Revive` write the incarnation their operation id derives;
/// every other mutation keeps the row's incarnation at the next revision.
#[test]
fn register_and_revive_write_the_operation_incarnation_and_updates_keep_it() {
    let owner = TriggerOwnerScope::session(SessionId::from("session"));
    let actor = crate::ProcessOriginator::session(crate::SessionScope::new("session"));
    let registered = mutation_receipt(
        evaluate_trigger_mutation(
            None,
            TriggerCommand::Register {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                draft: incarnation_fixture_draft(),
            },
            "register-op",
            1,
        )
        .expect("register evaluates"),
    );
    assert_eq!(
        registered.incarnation,
        trigger_incarnation(&owner, "register-op")
    );
    assert_eq!(registered.revision, 1);
    assert_ne!(
        trigger_incarnation(&owner, "register-op"),
        trigger_incarnation(&owner, "other-op"),
        "distinct operations mint distinct incarnations"
    );

    let disabled = mutation_receipt(
        evaluate_trigger_mutation(
            Some(registered.record_snapshot.clone()),
            TriggerCommand::Disable {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                expected_revision: 1,
            },
            "disable-op",
            2,
        )
        .expect("disable evaluates"),
    );
    assert_eq!(disabled.incarnation, registered.incarnation);
    assert_eq!(disabled.revision, 2);

    let deleted = mutation_receipt(
        evaluate_trigger_mutation(
            Some(disabled.record_snapshot.clone()),
            TriggerCommand::Delete {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                expected_revision: 2,
            },
            "delete-op",
            3,
        )
        .expect("delete evaluates"),
    );
    let revived = mutation_receipt(
        evaluate_trigger_mutation(
            Some(deleted.record_snapshot.clone()),
            TriggerCommand::Revive {
                owner_scope: owner.clone(),
                actor,
                subscription_key: "sub".to_string(),
                draft: incarnation_fixture_draft(),
                expected_revision: deleted.revision,
            },
            "revive-op",
            4,
        )
        .expect("revive evaluates"),
    );
    assert_eq!(
        revived.incarnation,
        trigger_incarnation(&owner, "revive-op")
    );
    assert_eq!(revived.revision, deleted.revision + 1);
}
