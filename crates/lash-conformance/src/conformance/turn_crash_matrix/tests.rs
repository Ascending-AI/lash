use super::*;

#[test]
fn golden_trace_generates_exactly_the_reviewed_outcome_table() {
    let generated = generated_points(&golden_trace());
    let table = turn_crash_matrix_outcomes();
    validate_outcome_table(&generated, &table).expect("committed table is valid");
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
fn generated_point_keys_are_unique() {
    let mut keys = std::collections::BTreeMap::new();
    for point in generated_points(&golden_trace()) {
        let key = point_key(&point);
        assert!(keys.insert(key, point).is_none());
    }
}

#[test]
fn fig_4679_outcomes_have_no_unexecuted_cold_process_rulings() {
    let rows: serde_json::Value = serde_json::from_str(OUTCOME_TABLE).expect("outcome rows");
    for row in rows.as_array().expect("outcome array") {
        assert!(row.get("level_2").is_none(), "unmounted level-2 row: {row}");
        assert!(
            row.get("scenario").is_none(),
            "unmounted recovery row: {row}"
        );
    }
}
