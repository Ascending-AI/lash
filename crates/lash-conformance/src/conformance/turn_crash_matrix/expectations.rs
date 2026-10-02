use super::*;

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
            ReviewedTurnCrashRuling::ErrorReturn(_) => None,
        })
        .collect()
}

/// The reviewed fail-stop rulings for the FIG-3524 error-return placements.
pub(super) fn error_return_rulings() -> Vec<ErrorReturnRuling> {
    reviewed_turn_crash_rulings()
        .into_iter()
        .filter_map(|ruling| match ruling {
            ReviewedTurnCrashRuling::ErrorReturn(entry) => Some(entry.error_return),
            ReviewedTurnCrashRuling::CrashPoint(_) => None,
        })
        .collect()
}

/// The error-return table must rule on every placement exactly once, and a
/// ruling pinning violations is a known-defect row: it requires a ticket,
/// and its prose must name that ticket.
pub(super) fn validate_error_return_rulings(rulings: &[ErrorReturnRuling]) -> Result<(), String> {
    let mut seen = Vec::new();
    for ruling in rulings {
        if seen.contains(&ruling.placement) {
            return Err(format!(
                "duplicate error-return ruling for {:?}",
                ruling.placement
            ));
        }
        seen.push(ruling.placement);
        match (&ruling.ticket, ruling.violations.is_empty()) {
            (Some(ticket), false) => {
                if !is_ticket_id(ticket) {
                    return Err(format!(
                        "error-return known defect requires a ticket id: {:?}",
                        ruling.placement
                    ));
                }
                if !ruling.outcome.contains(ticket.as_str()) {
                    return Err(format!(
                        "error-return known-defect outcome prose must name {ticket}: {:?}",
                        ruling.placement
                    ));
                }
            }
            (Some(_), true) => {
                return Err(format!(
                    "error-return ruling carries a ticket but pins no violations: {:?}",
                    ruling.placement
                ));
            }
            (None, false) => {
                return Err(format!(
                    "error-return ruling pins violations without a ticket: {:?}",
                    ruling.placement
                ));
            }
            (None, true) => {}
        }
        if ruling.outcome.trim().is_empty() {
            return Err(format!(
                "error-return ruling for {:?} must explain its ruling",
                ruling.placement
            ));
        }
    }
    let missing = [
        ErrorReturnPlacement::ToolAttempt,
        ErrorReturnPlacement::ToolAttemptSessionRetirement,
    ]
    .into_iter()
    .filter(|placement| !seen.contains(placement))
    .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "error-return rulings must cover every placement; missing {missing:?}"
        ));
    }
    Ok(())
}

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
