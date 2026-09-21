//! Laws the turn-ingress family's shared statements hold to.

use crate::Statement;

fn statements() -> Vec<Statement> {
    let mut all = Vec::new();
    all.extend_from_slice(super::TurnIngressStatements::NEUTRAL);
    all.extend_from_slice(super::cancel_requests::CancelRequestStatements::NEUTRAL);
    all.extend_from_slice(super::cancellation_bindings::CancellationBindingStatements::NEUTRAL);
    all.extend_from_slice(super::closure_authorizations::ClosureAuthorizationStatements::NEUTRAL);
    all.extend_from_slice(super::closure_participants::ClosureParticipantStatements::NEUTRAL);
    all.extend_from_slice(super::pending_inputs::PendingInputStatements::NEUTRAL);
    all.extend_from_slice(super::queued_batches::QueuedBatchStatements::NEUTRAL);
    all.extend_from_slice(super::queued_items::QueuedItemStatements::NEUTRAL);
    all.extend_from_slice(super::retired_scopes::RetiredScopeStatements::NEUTRAL);
    all.extend_from_slice(
        super::session_execution_leases::SessionExecutionLeaseStatements::NEUTRAL,
    );
    all.extend_from_slice(super::tool_intent_submissions::ToolIntentSubmissionStatements::NEUTRAL);
    all
}

fn squeezed(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The four claim-identity columns `ck_pending_turn_inputs_claim_identity_all_or_none`
/// makes all-or-none, plus the generation every release zeroes with them.
const CLAIM_RELEASE_ASSIGNMENTS: [&str; 5] = [
    "claim_id = NULL",
    "claim_owner_id = NULL",
    "claim_owner_incarnation_id = NULL",
    "claim_token = NULL",
    "claim_session_lease_generation = 0",
];

#[test]
fn every_release_statement_clears_the_whole_claim_identity() {
    // `ck_pending_turn_inputs_claim_identity_all_or_none` refuses a row that
    // carries some of the identity and not the rest, so a release that clears
    // `claim_token` alone is a constraint failure at run time rather than a
    // compile error here. Every statement in this family that lets go of a
    // claim spells all five assignments; this is what holds the spelling
    // together now that it is no longer one interpolated constant.
    let mut releases = 0;
    for statement in statements() {
        let sql = squeezed(statement.neutral());
        if !sql.contains("claim_token = NULL") {
            continue;
        }
        releases += 1;
        for assignment in CLAIM_RELEASE_ASSIGNMENTS {
            assert!(
                sql.contains(assignment),
                "`{}` releases a claim without `{assignment}`",
                statement.name(),
            );
        }
    }
    assert!(
        releases >= 4,
        "expected the family's release statements to be found, saw {releases}",
    );
}

#[test]
fn no_shared_statement_spells_a_turn_input_state() {
    // `pending_turn_inputs.state` is lifecycle vocabulary: a statement names
    // the partition as a `{{term(column)}}` token and the backend expands it
    // from the enum. A spelled literal is how a new variant silently stops
    // being claimed while still being prunable, so the repository gate refuses
    // one — and this is the same rule held over the shared statements alone,
    // so the refusal arrives in this crate's own suite.
    for state in [
        "'pending_active'",
        "'deferred_next_turn'",
        "'accepted'",
        "'cancelled'",
        "'completed'",
    ] {
        for statement in statements() {
            assert!(
                !statement.neutral().contains(state),
                "`{}` spells the turn-input state {state}",
                statement.name(),
            );
        }
    }
}
