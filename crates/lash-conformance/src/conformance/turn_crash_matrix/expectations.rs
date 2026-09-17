use super::*;

/// Return the committed trace-derived matrix and its hand-written outcomes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn reviewed_turn_crash_rulings() -> Vec<ReviewedTurnCrashRuling> {
    serde_json::from_str(OUTCOME_TABLE).expect("committed turn crash outcome table is valid")
}

pub(super) fn turn_crash_matrix_outcomes() -> Vec<TurnCrashOutcome> {
    reviewed_turn_crash_rulings()
        .into_iter()
        .filter_map(|ruling| match ruling {
            ReviewedTurnCrashRuling::CrashPoint(outcome) => Some(outcome),
            ReviewedTurnCrashRuling::DurableRecovery(_) => None,
        })
        .collect()
}

pub(super) fn durable_recovery_rulings() -> Vec<DurableRecoveryRuling> {
    reviewed_turn_crash_rulings()
        .into_iter()
        .filter_map(|ruling| match ruling {
            ReviewedTurnCrashRuling::CrashPoint(_) => None,
            ReviewedTurnCrashRuling::DurableRecovery(ruling) => Some(ruling),
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) fn validate_outcome_table(
    generated: &[TurnCrashPoint],
    table: &[TurnCrashOutcome],
) -> Result<(), String> {
    let table_points = table
        .iter()
        .map(|entry| entry.point.clone())
        .collect::<Vec<_>>();
    if table_points != generated {
        return Err(format!(
            "outcome table must cover every generated level-1 point exactly in trace order\nexpected: {generated:#?}\nactual: {table_points:#?}"
        ));
    }

    let expected_level_2 = ColdProcessTurnAction::CRASH_ACTIONS
        .into_iter()
        .map(|action| action.point().expect("crash action has a point"))
        .collect::<Vec<_>>();
    let actual_level_2 = table
        .iter()
        .filter(|entry| entry.level_2.is_some())
        .map(|entry| entry.point.clone())
        .collect::<Vec<_>>();
    let same_set = actual_level_2.len() == expected_level_2.len()
        && expected_level_2
            .iter()
            .all(|point| actual_level_2.contains(point));
    if !same_set {
        return Err(format!(
            "outcome table level-2 point set must match the cold-process actions\nexpected: {expected_level_2:#?}\nactual: {actual_level_2:#?}"
        ));
    }

    for entry in table.iter().filter(|entry| entry.level_2.is_some()) {
        let expectation = entry.level_2.as_ref().expect("filtered level-2 row");
        match expectation {
            Level2Expectation::Exact(_) => {}
            Level2Expectation::KnownDefect(known_defect) => {
                if !is_ticket_id(&known_defect.ticket) {
                    return Err(format!(
                        "known-defect expectation requires a ticket id: {:?}",
                        entry.point
                    ));
                }
                if known_defect.expected_defective == DurableEndState::CORRECT {
                    return Err(format!(
                        "known-defect expectation must differ from the correct durable end state: {:?}",
                        entry.point
                    ));
                }
                let correct_summary = DurableEndState::CORRECT.summary().replace(' ', ", ");
                if !entry.outcome.contains(&known_defect.ticket)
                    || !entry.outcome.contains(&correct_summary)
                {
                    return Err(format!(
                        "known-defect outcome prose must name {} and the correct end state `{correct_summary}`: {:?}",
                        known_defect.ticket, entry.point
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_durable_recovery_rulings(
    rulings: &[DurableRecoveryRuling],
) -> Result<(), String> {
    const EXPECTED_SCENARIOS: [&str; 4] = [
        "active_turn_input_pinned_to_recovered_turn",
        "checkpoint_execute_finalize",
        "checkpoint_replacement_double_crash",
        "peer_reclaim",
    ];
    let actual = rulings
        .iter()
        .map(|ruling| ruling.scenario.as_str())
        .collect::<Vec<_>>();
    if actual.len() != EXPECTED_SCENARIOS.len()
        || !EXPECTED_SCENARIOS
            .iter()
            .all(|expected| actual.contains(expected))
    {
        return Err(format!(
            "reviewed durable recovery scenarios must be exactly {EXPECTED_SCENARIOS:?}; got {actual:?}"
        ));
    }
    for ruling in rulings {
        if ruling.outcome.trim().is_empty() {
            return Err(format!(
                "durable recovery scenario `{}` must explain its ruling",
                ruling.scenario
            ));
        }
    }
    Ok(())
}

fn is_ticket_id(ticket: &str) -> bool {
    let Some((project, number)) = ticket.split_once('-') else {
        return false;
    };
    !project.is_empty()
        && project
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}
