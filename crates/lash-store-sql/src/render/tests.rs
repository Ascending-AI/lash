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
];

fn sqlite(neutral: &str) -> String {
    render(neutral, Dialect::sqlite("main"), TABLES).expect("renders")
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
            Dialect::sqlite("main").with_vocabulary(VOCABULARY),
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
fn every_owned_statement_renders_for_both_backends() {
    for statement in crate::all_statements() {
        for dialect in [
            Dialect::sqlite("main"),
            Dialect::sqlite("effect_journal"),
            Dialect::postgres(),
        ] {
            statement
                .render(dialect)
                .unwrap_or_else(|error| panic!("`{}` does not render: {error}", statement.name()));
        }
    }
}
