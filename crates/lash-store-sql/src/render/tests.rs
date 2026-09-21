//! What separates this renderer from the regex it replaces.
//!
//! Each test here is a rewrite a substring pass gets wrong: a `?` that is data,
//! a `$1` that is prose, a `?` inside a quoted identifier, and a table name
//! that is a prefix of another table name.

use super::*;

const TABLES: &[&str] = &[
    "await_event_waits",
    "await_event_waits_archive",
    "runtime_effect_replay",
    "processes",
];

/// Every table in this module's `TABLES`, in the connection's own database:
/// the layout of a deployment that holds one file.
const MAIN: TableLayout = TableLayout::new(&[SchemaTables::new("main", TABLES)]);

/// The catalog beside a bound process registry: the two-database layout the
/// attachment GC's owner-death proof needs.
const MAIN_BESIDE_REGISTRY: TableLayout = TableLayout::new(&[
    SchemaTables::new(
        "main",
        &[
            "await_event_waits",
            "await_event_waits_archive",
            "runtime_effect_replay",
        ],
    ),
    SchemaTables::new("process_registry", &["processes"]),
]);

/// The same tables, reached through an `ATTACH`ed journal instead.
const ATTACHED_JOURNAL: TableLayout =
    TableLayout::new(&[SchemaTables::new("effect_journal", TABLES)]);

fn sqlite(neutral: &str) -> String {
    render(neutral, Dialect::sqlite(MAIN), TABLES).expect("renders")
}

fn postgres(neutral: &str) -> String {
    render(neutral, Dialect::postgres(), TABLES).expect("renders")
}

/// A stand-in for the backend's real vocabulary: the same `fn(&str) -> String`
/// shape `lash_core::store_backend_support` exports, spelling two terms the
/// process family will use.
fn live_status(column: &str) -> String {
    format!("{column} IN ('running', 'waiting')")
}

fn retired_status(column: &str) -> String {
    format!("{column} NOT IN ('running', 'waiting')")
}

/// A stand-in for the process family's lifecycle expansions, which are
/// generated from `lash_core`'s enums and cannot be reached from this crate.
fn stub_predicate(column: &str) -> String {
    format!("{column} = ?")
}

fn turn_owner(column: &str) -> String {
    format!("{column} = 'turn'")
}

fn process_owner(column: &str) -> String {
    format!("{column} = 'process'")
}

const VOCABULARY: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new("live_process_status", live_status),
    VocabularyTerm::new("retired_process_status", retired_status),
]);

fn with_vocabulary(neutral: &str) -> Result<String, RenderError> {
    render(
        neutral,
        Dialect::postgres().with_vocabulary(VOCABULARY),
        TABLES,
    )
}

#[test]
fn a_vocabulary_token_expands_once_for_both_backends() {
    let neutral = "SELECT 1 FROM await_event_waits \
                   WHERE {{live_process_status(status)}} AND key_id = ?1";

    assert_eq!(
        with_vocabulary(neutral).expect("renders"),
        "SELECT 1 FROM lash_await_event_waits \
         WHERE status IN ('running', 'waiting') AND key_id = $1"
    );
    assert_eq!(
        render(
            neutral,
            Dialect::sqlite(MAIN).with_vocabulary(VOCABULARY),
            TABLES,
        )
        .expect("renders"),
        "SELECT 1 FROM main.await_event_waits \
         WHERE status IN ('running', 'waiting') AND key_id = ?1"
    );
}

#[test]
fn a_token_carries_a_qualified_column_and_tolerates_inner_spacing() {
    assert_eq!(
        with_vocabulary(
            "SELECT 1 FROM await_event_waits WHERE {{ retired_process_status( p.status ) }}"
        )
        .expect("renders"),
        "SELECT 1 FROM lash_await_event_waits WHERE p.status NOT IN ('running', 'waiting')"
    );
}

#[test]
fn a_token_spelling_inside_a_string_literal_or_a_comment_is_text_not_a_token() {
    // The literal beside the token contains the token's own spelling; only the
    // one outside the quotes expands.
    assert_eq!(
        with_vocabulary(
            "UPDATE await_event_waits SET wait_json = '{{live_process_status(status)}}' \
             WHERE {{live_process_status(status)}}"
        )
        .expect("renders"),
        "UPDATE lash_await_event_waits SET wait_json = '{{live_process_status(status)}}' \
         WHERE status IN ('running', 'waiting')"
    );
    let commented = with_vocabulary(
        "SELECT 1 FROM await_event_waits\n\
         -- {{nonexistent_term(status)}} is prose\n\
         /* {{also_nonexistent(status)}} */ WHERE {{live_process_status(status)}}",
    )
    .expect("renders");
    assert!(commented.contains("-- {{nonexistent_term(status)}} is prose"));
    assert!(commented.contains("/* {{also_nonexistent(status)}} */"));
    assert!(commented.ends_with("WHERE status IN ('running', 'waiting')"));
}

#[test]
fn a_token_with_no_expansion_is_refused_at_render_time() {
    assert_eq!(
        render(
            "SELECT 1 FROM await_event_waits WHERE {{live_process_status(status)}}",
            Dialect::postgres(),
            TABLES,
        )
        .expect_err("no vocabulary attached"),
        RenderError::VocabularyNotSupplied {
            name: "live_process_status".to_string(),
            at: 38,
        }
    );
    assert_eq!(
        with_vocabulary("SELECT 1 FROM await_event_waits WHERE {{pending_cancel(status)}}")
            .expect_err("unknown term"),
        RenderError::UnknownVocabularyTerm {
            name: "pending_cancel".to_string(),
            at: 38,
            known: vec!["live_process_status", "retired_process_status"],
        }
    );
}

#[test]
fn a_token_whose_column_is_not_an_identifier_is_refused() {
    for column in ["'running'", "status = 1", "a.b.c", "", "status)"] {
        let neutral =
            format!("SELECT 1 FROM await_event_waits WHERE {{{{live_process_status({column})}}}}");
        assert!(
            matches!(
                with_vocabulary(&neutral),
                Err(RenderError::VocabularyColumnNotIdentifier { .. })
                    | Err(RenderError::MalformedVocabularyToken { .. })
            ),
            "`{column}` must not render as a column reference"
        );
    }
}

#[test]
fn a_malformed_or_unterminated_token_is_refused_rather_than_copied() {
    assert_eq!(
        with_vocabulary("SELECT 1 FROM await_event_waits WHERE {live_process_status(status)}")
            .expect_err("single brace"),
        RenderError::MalformedVocabularyToken {
            at: 38,
            reason: "a `{` that does not open a vocabulary token",
        }
    );
    assert_eq!(
        with_vocabulary("SELECT 1 FROM await_event_waits WHERE {{live_process_status(status)")
            .expect_err("unterminated token"),
        RenderError::Unterminated {
            kind: "vocabulary token",
            at: 38,
        }
    );
    assert!(matches!(
        with_vocabulary("SELECT 1 FROM await_event_waits WHERE {{live_process_status}}"),
        Err(RenderError::MalformedVocabularyToken { .. })
    ));
    assert!(matches!(
        with_vocabulary("SELECT 1 FROM await_event_waits WHERE status = 1}}"),
        Err(RenderError::MalformedVocabularyToken {
            reason: "a `}` outside a vocabulary token",
            ..
        })
    ));
}

#[test]
fn placeholders_take_the_backend_spelling_and_keep_their_numbers() {
    let neutral =
        "SELECT 1 FROM await_event_waits WHERE key_id = ?1 AND scope_json = ?2 AND key_id <> ?1";

    assert_eq!(
        sqlite(neutral),
        "SELECT 1 FROM main.await_event_waits WHERE key_id = ?1 AND scope_json = ?2 AND key_id <> ?1"
    );
    assert_eq!(
        postgres(neutral),
        "SELECT 1 FROM lash_await_event_waits WHERE key_id = $1 AND scope_json = $2 AND key_id <> $1"
    );
}

#[test]
fn a_question_mark_inside_a_string_literal_is_data_not_a_placeholder() {
    let neutral = "UPDATE await_event_waits SET wait_json = 'who? me??' WHERE key_id = ?1";

    assert_eq!(
        postgres(neutral),
        "UPDATE lash_await_event_waits SET wait_json = 'who? me??' WHERE key_id = $1"
    );
    // The escaped-quote form keeps the literal whole: the `?` after the
    // doubled quote is still inside the string.
    assert_eq!(
        postgres("UPDATE await_event_waits SET wait_json = 'it''s ?1' WHERE key_id = ?1"),
        "UPDATE lash_await_event_waits SET wait_json = 'it''s ?1' WHERE key_id = $1"
    );
}

#[test]
fn a_dollar_placeholder_inside_a_comment_is_prose_and_survives_verbatim() {
    let neutral = "SELECT key_id\n\
                   -- $1 is the key on PostgreSQL; ?1 here is prose too\n\
                   FROM await_event_waits WHERE key_id = ?1";

    let rendered = sqlite(neutral);

    assert!(rendered.contains("-- $1 is the key on PostgreSQL; ?1 here is prose too"));
    assert!(rendered.ends_with("FROM main.await_event_waits WHERE key_id = ?1"));
    assert!(
        postgres("/* $1 ?1 await_event_waits */ SELECT 1 FROM await_event_waits WHERE key_id = ?1")
            .starts_with("/* $1 ?1 await_event_waits */")
    );
}

#[test]
fn a_quoted_identifier_containing_a_question_mark_is_left_alone() {
    let neutral = "SELECT \"why?\" , `how?` , [when?] FROM await_event_waits WHERE key_id = ?1";

    assert_eq!(
        postgres(neutral),
        "SELECT \"why?\" , `how?` , [when?] FROM lash_await_event_waits WHERE key_id = $1"
    );
}

#[test]
fn a_table_name_that_is_a_prefix_of_another_is_matched_as_a_whole_token() {
    let neutral = "SELECT 1 FROM await_event_waits_archive \
                   JOIN await_event_waits ON await_event_waits.key_id = ?1";

    assert_eq!(
        postgres(neutral),
        "SELECT 1 FROM lash_await_event_waits_archive \
         JOIN lash_await_event_waits ON lash_await_event_waits.key_id = $1"
    );
    // A substring pass turns the longer name into `lash_await_event_waits_archive`
    // only by luck of ordering; it turns `await_event_waitsful` into a table.
    assert_eq!(
        postgres("SELECT await_event_waitsful FROM await_event_waits"),
        "SELECT await_event_waitsful FROM lash_await_event_waits"
    );
}

#[test]
fn an_already_qualified_name_is_not_qualified_twice() {
    assert_eq!(
        sqlite("SELECT other.await_event_waits FROM await_event_waits"),
        "SELECT other.await_event_waits FROM main.await_event_waits"
    );
}

#[test]
fn a_subquery_a_table_valued_function_and_extract_are_not_table_positions() {
    assert!(
        render(
            "SELECT 1 FROM (SELECT key_id FROM await_event_waits) AS probe",
            Dialect::postgres(),
            TABLES,
        )
        .is_ok()
    );
    assert!(
        render(
            "UPDATE runtime_effect_replay SET updated_at_ms = \
             floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
            Dialect::postgres(),
            TABLES,
        )
        .is_ok()
    );
}

#[test]
fn a_table_position_naming_an_unowned_relation_is_refused() {
    let error = render(
        "DELETE FROM runtime_turn_commits WHERE turn_id = ?1",
        Dialect::postgres(),
        TABLES,
    )
    .expect_err("unowned table");

    assert_eq!(
        error,
        RenderError::UnknownTable {
            name: "runtime_turn_commits".to_string(),
            at: 12,
        }
    );
    assert!(
        render(
            "INSERT INTO runtime_turn_commits (turn_id) VALUES (?1)",
            Dialect::postgres(),
            TABLES,
        )
        .is_err(),
        "the `(` that follows a column list must not read as a function call"
    );
}

#[test]
fn malformed_neutral_text_is_refused_rather_than_rendered() {
    assert_eq!(
        render(
            "SELECT ? FROM await_event_waits",
            Dialect::postgres(),
            TABLES
        )
        .expect_err("bare question mark"),
        RenderError::UnnumberedPlaceholder { at: 7 }
    );
    assert_eq!(
        render(
            "SELECT $1 FROM await_event_waits",
            Dialect::postgres(),
            TABLES
        )
        .expect_err("dollar in neutral text"),
        RenderError::DollarPlaceholder { at: 7 }
    );
    assert_eq!(
        render(
            "SELECT 'open FROM await_event_waits",
            Dialect::postgres(),
            TABLES
        )
        .expect_err("unterminated literal"),
        RenderError::Unterminated {
            kind: "string literal",
            at: 7,
        }
    );
    assert_eq!(
        render(
            "SELECT 1 /* open FROM await_event_waits",
            Dialect::postgres(),
            TABLES
        )
        .expect_err("unterminated block comment"),
        RenderError::Unterminated {
            kind: "block comment",
            at: 9,
        }
    );
}



#[test]
fn a_name_the_statement_binds_for_itself_is_not_a_table() {
    let neutral = "WITH scope AS (
             SELECT COUNT(*) AS total FROM await_event_waits
         ), keys AS (
             SELECT key_id FROM await_event_waits WHERE scope_json = ?1
         )
         SELECT scope.total, keys.key_id FROM scope LEFT JOIN keys ON TRUE";

    assert_eq!(
        sqlite(neutral),
        "WITH scope AS (
             SELECT COUNT(*) AS total FROM main.await_event_waits
         ), keys AS (
             SELECT key_id FROM main.await_event_waits WHERE scope_json = ?1
         )
         SELECT scope.total, keys.key_id FROM scope LEFT JOIN keys ON TRUE"
    );
    // A derived table's own alias binds the same way.
    assert_eq!(
        postgres("SELECT rows.key_id FROM (SELECT key_id FROM await_event_waits) AS rows"),
        "SELECT rows.key_id FROM (SELECT key_id FROM lash_await_event_waits) AS rows"
    );
    // A column alias is not a relation: `total` is still refused in a table
    // position, so the rule stays as narrow as `WITH … AS (`.
    let aliased = "SELECT COUNT(*) AS total FROM await_event_waits; SELECT 1 FROM total";
    assert_eq!(
        render(aliased, Dialect::postgres(), TABLES).expect_err("a column alias is not a relation"),
        RenderError::UnknownTable {
            name: "total".to_string(),
            at: aliased.rfind("total").expect("the trailing alias"),
        }
    );
}

#[test]
fn every_owned_statement_renders_for_both_backends() {
    // Over the crate's real table list rather than this module's fixture: a
    // statement set is only renderable for a layout that places every table
    // it names, and this self-test asks whether the text is well formed, not
    // where a deployment puts it.
    const EVERY_TABLE_IN_MAIN: TableLayout =
        TableLayout::new(&[SchemaTables::new("main", crate::TABLES)]);
    const EVERY_TABLE_ATTACHED: TableLayout =
        TableLayout::new(&[SchemaTables::new("effect_journal", crate::TABLES)]);
    // Stand-ins for the backends' `AttachmentOwnerKind` expansions, which
    // live in `lash-core` and cannot be reached from this crate. Both
    // backends really do register these names; the gate and the
    // `attachment_owner_sql` unit tests hold the expansions themselves.
    const OWNER_TERMS: Vocabulary = Vocabulary::new(&[
        VocabularyTerm::new("turn_attachment_owner", turn_owner),
        VocabularyTerm::new("process_attachment_owner", process_owner),
        VocabularyTerm::new("live_process_status", stub_predicate),
        VocabularyTerm::new("retired_process_status", stub_predicate),
        VocabularyTerm::new("nonterminal_process_status", stub_predicate),
        VocabularyTerm::new("undelivered_wake_delivery_state", stub_predicate),
        VocabularyTerm::new("pending_wake_delivery_state", stub_predicate),
        VocabularyTerm::new("pending_wake_delivery_state_value", stub_predicate),
        VocabularyTerm::new("enqueuing_wake_delivery_state", stub_predicate),
        VocabularyTerm::new("discarded_wake_delivery_state", stub_predicate),
        VocabularyTerm::new("not_enqueued_wake_delivery_state", stub_predicate),
        VocabularyTerm::new(
            "accepted_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "active_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "deferred_next_turn_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "nonterminal_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "pending_active_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "terminal_turn_input_state",
            stub_predicate,
        ),
        VocabularyTerm::new(
            "undelivered_turn_input_state",
            stub_predicate,
        ),
    ]);

    for statement in crate::all_statements() {
        for dialect in [
            Dialect::sqlite(EVERY_TABLE_IN_MAIN).with_vocabulary(OWNER_TERMS),
            Dialect::sqlite(EVERY_TABLE_ATTACHED).with_vocabulary(OWNER_TERMS),
            Dialect::postgres().with_vocabulary(OWNER_TERMS),
        ] {
            statement
                .render(dialect)
                .unwrap_or_else(|error| panic!("`{}` does not render: {error}", statement.name()));
        }
    }
}

#[test]
fn an_upserts_do_update_is_an_action_clause_not_a_table_position() {
    // `UPDATE` heads a statement *and* closes `ON CONFLICT … DO`. Only the
    // first takes a table; reading the second as one refuses every upsert in
    // the crate on its `SET`.
    let neutral = "INSERT INTO await_event_waits (key_id, scope_json)
         VALUES (?1, ?2)
         ON CONFLICT (key_id) DO UPDATE SET scope_json = excluded.scope_json";

    assert_eq!(
        sqlite(neutral),
        "INSERT INTO main.await_event_waits (key_id, scope_json)
         VALUES (?1, ?2)
         ON CONFLICT (key_id) DO UPDATE SET scope_json = excluded.scope_json"
    );
    assert_eq!(
        postgres(neutral),
        "INSERT INTO lash_await_event_waits (key_id, scope_json)
         VALUES ($1, $2)
         ON CONFLICT (key_id) DO UPDATE SET scope_json = excluded.scope_json"
    );
}

#[test]
fn a_statement_heading_update_still_demands_a_table_it_owns() {
    // The `DO` guard is the only thing that relaxes `UPDATE`, and it reaches
    // exactly one token: a plain `UPDATE` over an unowned table is still the
    // startup failure it was.
    assert_eq!(
        render(
            "UPDATE session_head SET turn_id = ?1",
            Dialect::postgres(),
            TABLES
        ),
        Err(RenderError::UnknownTable {
            name: "session_head".to_string(),
            at: 7,
        })
    );
}

// --- FIG-3406: the schema is a property of the table, under a layout ------

#[test]
fn one_statement_addresses_two_databases_at_once() {
    // The attachment GC's shape: the manifest is in the session catalog and
    // the rows that prove a process owner dead are in an ATTACHed registry.
    // A dialect carrying one schema for the whole statement cannot spell it.
    let neutral = "SELECT 1 FROM await_event_waits AS wait
         WHERE NOT EXISTS (
             SELECT 1 FROM processes AS process
             WHERE process.process_id = wait.owner_id
         )";

    assert_eq!(
        render(neutral, Dialect::sqlite(MAIN_BESIDE_REGISTRY), TABLES).expect("renders"),
        "SELECT 1 FROM main.await_event_waits AS wait
         WHERE NOT EXISTS (
             SELECT 1 FROM process_registry.processes AS process
             WHERE process.process_id = wait.owner_id
         )"
    );
    // PostgreSQL holds both in one database, so the same neutral text is one
    // shared statement: the difference really is a render axis.
    assert_eq!(
        postgres(neutral),
        "SELECT 1 FROM lash_await_event_waits AS wait
         WHERE NOT EXISTS (
             SELECT 1 FROM lash_processes AS process
             WHERE process.process_id = wait.owner_id
         )"
    );
}

#[test]
fn the_same_table_renders_per_layout_and_identically_within_one() {
    let neutral = "SELECT key_id FROM await_event_waits WHERE key_id = ?1
         AND scope_json IN (SELECT scope_json FROM await_event_waits WHERE key_id <> ?1)";

    let main = render(neutral, Dialect::sqlite(MAIN), TABLES).expect("renders");
    let attached = render(neutral, Dialect::sqlite(ATTACHED_JOURNAL), TABLES).expect("renders");

    // Different per layout …
    assert!(main.contains("FROM main.await_event_waits"));
    assert!(attached.contains("FROM effect_journal.await_event_waits"));
    assert_ne!(main, attached);
    // … and the same within one: both occurrences of the table in a single
    // statement resolve through the same layout, so a statement cannot reach
    // two copies of one table by accident.
    assert_eq!(main.matches("main.await_event_waits").count(), 2);
    assert_eq!(
        attached.matches("effect_journal.await_event_waits").count(),
        2
    );
    assert_eq!(
        main.replace("main.", "effect_journal."),
        attached,
        "a layout changes the database, never the statement"
    );
}

#[test]
fn a_table_the_layout_does_not_place_is_refused() {
    // The registry is not attached, so this connection cannot see `processes`
    // at all. Refusing at render time is what keeps the with-registry shape of
    // a statement from being issued on a connection that has none.
    const NO_REGISTRY: TableLayout =
        TableLayout::new(&[SchemaTables::new("main", &["await_event_waits"])]);

    let error = render(
        "SELECT 1 FROM processes WHERE process_id = ?1",
        Dialect::sqlite(NO_REGISTRY),
        TABLES,
    )
    .expect_err("the layout places no `processes`");

    assert_eq!(
        error,
        RenderError::TableNotPlaced {
            name: "processes".to_string(),
            at: 14,
            schemas: vec!["main"],
        }
    );
    assert!(
        error.to_string().contains("[\"main\"]"),
        "the refusal names the databases the layout does reach: {error}"
    );
}

#[test]
fn an_unqualified_sqlite_dialect_places_nothing_and_qualifies_nothing() {
    // `Dialect::sqlite_unqualified()` keeps its meaning: a family on one
    // connection addresses its tables the way its INDEXED BY plans were
    // measured against, and no layout decides anything for it.
    assert_eq!(
        render(
            "SELECT 1 FROM processes WHERE process_id = ?1",
            Dialect::sqlite_unqualified(),
            TABLES,
        )
        .expect("renders"),
        "SELECT 1 FROM processes WHERE process_id = ?1"
    );
}

#[test]
fn a_layout_resolves_a_two_database_table_by_declaration_order() {
    // `effect_scope_retirements` is carried by both the effect journal and a
    // bound process registry (ADR 0049). A layout places it in exactly one,
    // and the other copy is a different layout — never a second entry here.
    const TWO: &[&str] = &["runtime_effect_replay"];
    const JOURNAL_FIRST: TableLayout = TableLayout::new(&[
        SchemaTables::new("main", TWO),
        SchemaTables::new("process_registry", TWO),
    ]);
    const REGISTRY_ONLY: TableLayout =
        TableLayout::new(&[SchemaTables::new("process_registry", TWO)]);

    let neutral = "SELECT scope_id FROM runtime_effect_replay";
    assert_eq!(
        render(neutral, Dialect::sqlite(JOURNAL_FIRST), TABLES).expect("renders"),
        "SELECT scope_id FROM main.runtime_effect_replay"
    );
    assert_eq!(
        render(neutral, Dialect::sqlite(REGISTRY_ONLY), TABLES).expect("renders"),
        "SELECT scope_id FROM process_registry.runtime_effect_replay"
    );
}

/// A common table expression may state its inlining, and is still a relation
/// the statement binds: the process family's PostgreSQL prune opens with
/// `event_count AS MATERIALIZED ( … )`.
#[test]
fn a_materialized_common_table_expression_is_still_a_binding() {
    assert_eq!(
        postgres(
            "WITH counted AS MATERIALIZED (
                 SELECT count(*) AS value FROM await_event_waits
             )
             SELECT value FROM counted"
        ),
        "WITH counted AS MATERIALIZED (
                 SELECT count(*) AS value FROM lash_await_event_waits
             )
             SELECT value FROM counted"
    );
}

/// `FOR UPDATE OF <alias> SKIP LOCKED` names an alias, not a relation: the
/// claim statement the process family's wake queue issues ends that way, and
/// reading its `OF` as a table position refused the statement at startup.
#[test]
fn a_for_update_lock_clause_is_not_a_table_position() {
    assert_eq!(
        postgres(
            "SELECT 1 FROM await_event_waits AS candidate
             FOR UPDATE OF candidate SKIP LOCKED"
        ),
        "SELECT 1 FROM lash_await_event_waits AS candidate
             FOR UPDATE OF candidate SKIP LOCKED"
    );
}
