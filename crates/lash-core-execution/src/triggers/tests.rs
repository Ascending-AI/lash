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
