//! Laws the turn-ingress family's shared statements hold to.

use crate::Statement;

fn statements() -> Vec<Statement> {
    let mut all = Vec::new();
    all.extend_from_slice(super::TurnIngressStatements::NEUTRAL);
    all.extend_from_slice(super::cancel_requests::CancelRequestStatements::NEUTRAL);
    all.extend_from_slice(super::cancellation_bindings::CancellationBindingStatements::NEUTRAL);
    all.extend_from_slice(super::closure_authorizations::ClosureAuthorizationStatements::NEUTRAL);
    all.extend_from_slice(super::pending_inputs::PendingInputStatements::NEUTRAL);
    all.extend_from_slice(super::queued_batches::QueuedBatchStatements::NEUTRAL);
    all.extend_from_slice(super::queued_items::QueuedItemStatements::NEUTRAL);
    all.extend_from_slice(super::retired_scopes::RetiredScopeStatements::NEUTRAL);
    all.extend_from_slice(super::tool_intent_submissions::ToolIntentSubmissionStatements::NEUTRAL);
    all.extend_from_slice(super::turn_parks::TurnParkStatements::NEUTRAL);
    all
}

fn squeezed(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_release_statement_clears_the_whole_admission() {
    // `ck_pending_turn_inputs_admission_all_or_none` and its batch twin refuse
    // a row that names a root without the step that bound it, or the other
    // way round, so a release that clears one column alone is a constraint
    // failure at run time rather than a compile error here. Every statement
    // in this family that lets go of an admission clears both.
    let mut releases = 0;
    for statement in statements() {
        let sql = squeezed(statement.neutral());
        let clears_root = sql.contains("admitted_root = NULL");
        let clears_step = sql.contains("admitted_by = NULL");
        if !clears_root && !clears_step {
            continue;
        }
        releases += 1;
        assert!(
            clears_root && clears_step,
            "`{}` releases half an admission",
            statement.name(),
        );
    }
    assert!(
        releases >= 5,
        "expected the family's release statements to be found, saw {releases}",
    );
}

#[test]
fn every_admission_write_is_predicated_on_the_rows_binding() {
    // A row is bound only while open, and settled or released only by the
    // root that holds it (FIG-3927). The predicate is the write's backstop,
    // so every statement that sets or clears a binding must carry one.
    for statement in statements() {
        let sql = squeezed(statement.neutral());
        let writes_binding = sql.contains("SET") && sql.contains("admitted_root =");
        let deletes_admitted = sql.starts_with("DELETE") && sql.contains("admitted_root");
        if !(writes_binding || deletes_admitted) {
            continue;
        }
        let where_clause = sql.rsplit_once("WHERE").map_or("", |(_, tail)| tail);
        assert!(
            where_clause.contains("admitted_root IS NULL")
                || where_clause.contains("admitted_root = ?")
                || where_clause.contains("nonterminal"),
            "`{}` writes a binding without predicating the row's own",
            statement.name(),
        );
    }
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
