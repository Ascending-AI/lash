//! Rendering for the process-listing statement: a clause and the
//! parameter it reads are added in one place, so an index-served
//! conjunct cannot drift from its binding.

use super::*;

/// Substitute the optional clauses into every arm of a list statement.
pub(super) fn render_list_sql(template: &str, extra: &str) -> String {
    template.replace("{extra}", extra)
}

/// Render the list statement and its bindings together, so an optional clause
/// and the parameter it reads are added in one place.
///
/// The parent-scope and pending-cancel clauses are conjuncts, present only
/// when the filter asks for them: each has a partial index behind it, and
/// SQLite plans an index for `AND cancel_requested_at_ms IS NOT NULL ...` but
/// never for `AND (?n IS NULL OR ...)`.
pub(super) fn list_processes_query(
    filter: &lash_core::ProcessListFilter,
    status: Option<String>,
    definition: Option<String>,
) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;

    fn text(value: Option<String>) -> Value {
        value.map_or(Value::Null, Value::Text)
    }
    fn integer(value: Option<u64>) -> Value {
        value.map_or(Value::Null, |value| {
            Value::Integer(crate::clamp_epoch_ms(value))
        })
    }

    let mut values = vec![
        text(status),
        text(filter.originator.as_ref().map(|o| o.originator_id())),
        text(filter.identity_kind.clone()),
        text(filter.identity_label.clone()),
        text(definition),
        text(filter.caused_by_occurrence_id.clone()),
        text(filter.caused_by_subscription_id.clone()),
        integer(filter.created_at_start_ms),
        integer(filter.created_at_end_ms),
    ];
    let retired = filter.retired_since_ms.is_some();
    if retired {
        values.push(integer(filter.retired_since_ms));
    }

    let mut extra = String::new();
    if let Some(parent) = &filter.parent_scope {
        values.push(Value::Text(parent.storage_kind().to_string()));
        let kind = values.len();
        values.push(text(parent.storage_id()));
        let id = values.len();
        // `IS` rather than `=`: a Host scope stores a NULL id, and the check
        // constraint ties that NULL to the kind, so the pair is still an
        // equality lookup on `idx_processes_parent_scope`.
        extra.push_str(&format!(
            "\n           AND parent_scope_kind = ?{kind}\n           AND parent_scope_id IS ?{id}"
        ));
    }
    if let Some(before_ms) = filter.cancel_pending_before_ms {
        values.push(integer(Some(before_ms)));
        let before = values.len();
        extra.push_str(&format!(
            "\n           AND cancel_requested_at_ms IS NOT NULL\n           AND cancel_requested_at_ms < ?{before}\n           AND {nonterminal}",
            nonterminal = nonterminal_process_status("status"),
        ));
    }

    // The unnarrowed statements are the common case, so they are rendered once
    // and reused rather than rebuilt per call.
    let sql = match (retired, extra.is_empty()) {
        (true, true) => LIST_PROCESSES_RECENT_RETIRED_SQL.clone(),
        (true, false) => render_list_sql(&LIST_PROCESSES_RECENT_RETIRED_SQL_TEMPLATE, &extra),
        (false, true) => LIST_PROCESSES_SQL.clone(),
        (false, false) => render_list_sql(LIST_PROCESSES_SQL_TEMPLATE, &extra),
    };
    (sql, values)
}
