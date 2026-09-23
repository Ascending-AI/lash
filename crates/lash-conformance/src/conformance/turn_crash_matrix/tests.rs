use super::*;

fn install_known_defect_fixture(table: &mut [TurnCrashOutcome]) -> &mut KnownDefectExpectation {
    let entry = table
        .iter_mut()
        .find(|entry| entry.level_2.is_some())
        .expect("level-2 row");
    let effect_executions = *entry
        .level_2
        .as_ref()
        .expect("level-2 expectation")
        .effect_executions();
    entry.level_2 = Some(Level2Expectation::KnownDefect(KnownDefectExpectation {
        effect_executions,
        ticket: "FIG-999".to_string(),
        expected_defective: DurableEndState {
            terminal: 1,
            pending_inputs: 0,
            queued_work: 1,
        },
    }));
    entry.outcome = "KNOWN-DEFECT FIG-999; correct durable end state terminal=1, pending_inputs=0, queued_work=0".to_string();
    match entry.level_2.as_mut() {
        Some(Level2Expectation::KnownDefect(defect)) => defect,
        _ => unreachable!("installed known-defect fixture"),
    }
}

#[test]
fn golden_trace_generates_exactly_the_reviewed_outcome_table() {
    let generated = generated_points(&golden_trace());
    let table = turn_crash_matrix_outcomes();
    validate_outcome_table(&generated, &table).expect("committed table is valid");
    validate_durable_recovery_rulings(&durable_recovery_rulings())
        .expect("committed durable recovery rulings are valid");
    validate_error_return_rulings(&error_return_rulings())
        .expect("committed error-return rulings are valid");
}

#[test]
fn outcome_validation_rejects_a_dropped_level_1_point() {
    let generated = generated_points(&golden_trace());
    let mut table = turn_crash_matrix_outcomes();
    table.remove(4);
    assert!(
        validate_outcome_table(&generated, &table).is_err(),
        "removing any generated level-1 point must invalidate the oracle"
    );
}

#[test]
fn outcome_validation_rejects_a_relocated_level_2_expectation() {
    let generated = generated_points(&golden_trace());
    let mut table = turn_crash_matrix_outcomes();
    let source = table
        .iter()
        .position(|entry| {
            entry.point
                == ColdProcessTurnAction::ProviderInitialMidStream
                    .point()
                    .expect("crash point")
        })
        .expect("level-2 source row");
    let destination = table
        .iter()
        .position(|entry| {
            entry.point
                == TurnCrashPoint {
                    operation: TurnSeamOperation::Store(StoreOperation::LoadSessionHeadMeta),
                    placement: CrashPlacement::Boundary,
                }
        })
        .expect("level-1-only destination row");
    table[destination].level_2 = table[source].level_2.take();
    assert!(
        validate_outcome_table(&generated, &table).is_err(),
        "moving a level-2 expectation to a different point must invalidate the oracle"
    );
}

#[test]
fn outcome_validation_rejects_a_known_defect_without_a_ticket() {
    let generated = generated_points(&golden_trace());
    let mut table = turn_crash_matrix_outcomes();
    assert!(
        validate_outcome_table(&generated, &table).is_ok(),
        "the synthetic defect test must start from a valid oracle"
    );
    let defect = install_known_defect_fixture(&mut table);
    defect.ticket.clear();
    assert!(
        validate_outcome_table(&generated, &table).is_err(),
        "a known defect without a ticket id must invalidate the oracle"
    );
}

#[test]
fn generated_point_keys_are_unique() {
    let mut keys = std::collections::BTreeMap::new();
    for point in generated_points(&golden_trace()) {
        let key = point_key(&point);
        assert!(keys.insert(key, point).is_none());
    }
}
