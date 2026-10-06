use super::*;
use lash::SessionId;

// The multi-session workbench: a roster of sessions surviving the web process
// that created them (FIG-1306).
//
// Every fixture here executes the production route handlers rather than the
// roster type, because the mechanism under test is not "does a map remember a
// string" — it is whether a session an operator created is still the session
// the *executor* runs after the handle that created it is gone.
//
// ADR 0096: TypeScript is the sole RLM language, so the halves of these
// fixtures that asserted a second dialect beside it are gone.

// ADR 0096: the fixture that created a Lashlang session on a TypeScript
// deployment is gone with the second dialect.

/// A create request that still names a language does not decode.
///
/// The field is gone rather than pinned (ADR 0096), and a plain Serde struct
/// would drop an unknown key silently — so a stale form posting
/// `{"dialect": "lashscript"}` would be answered `201` and served TypeScript,
/// which is exactly the quiet substitution the removal is meant to prevent.
/// `deny_unknown_fields` is what makes it an answer instead.
#[test]
fn a_create_request_that_still_names_a_language_does_not_decode() {
    let error = serde_json::from_value::<SessionCreateRequest>(serde_json::json!({
        "name": "typo",
        "dialect": "lashscript",
    }))
    .expect_err("a create request naming a language must be refused");
    assert!(
        error.to_string().contains("dialect"),
        "the refusal names the retired field: {error}"
    );

    let accepted = serde_json::from_value::<SessionCreateRequest>(serde_json::json!({
        "name": "ok",
    }))
    .expect("a request carrying only a name still decodes");
    assert_eq!(accepted.name.as_deref(), Some("ok"));
}

/// A reset replaces the session behind a roster slot, and the replacement keeps
/// the slot the operator named.
///
/// ADR 0096: the dialect half of this fixture is gone with the second dialect;
/// the slot itself still has to survive the rotation.
#[test]
fn a_reset_carries_the_slot_name_to_the_rotated_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = WorkbenchSessions::persistent(temp.path().join("session-id")).expect("roster");
    let original = sessions.current();
    sessions.record(original.clone(), "typescript work".to_string());

    let (old, new) = sessions.rotate();

    assert_eq!(old, original);
    assert_eq!(
        sessions.entry(&new).map(|entry| entry.name),
        Some("typescript work".to_string())
    );
    assert!(
        sessions.entry(&old).is_none(),
        "the retired session leaves the roster with the slot it held"
    );
}

// ADR 0096: the typed dialect-pin conflict fixture (FIG-1555) is gone with
// `RlmSessionConfigConflict::Dialect`.

/// An unnamed session takes its first prompt as its sidebar title; a named
/// one, and a session already titled, keep theirs. Selecting a session does
/// not reorder the list: only a sent prompt makes a session recently active.
#[test]
fn the_first_prompt_titles_an_unnamed_session_and_selection_keeps_the_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = WorkbenchSessions::persistent(temp.path().join("session-id")).expect("roster");
    let unnamed = new_session_id();
    sessions.record(unnamed.clone(), unnamed.to_string());
    let named = new_session_id();
    sessions.record(named.clone(), "typescript work".to_string());
    let carried = new_session_id();
    sessions.record(carried.clone(), new_session_id().to_string());

    sessions.record_prompt(
        &unnamed,
        "\n  Fix   the flaky   cron test\nand explain why it flaked",
    );
    sessions.record_prompt(&unnamed, "a later prompt never retitles it");
    sessions.record_prompt(&named, "this prompt is not a title");
    sessions.record_prompt(&carried, &"long ".repeat(40));

    let name = |id: &SessionId| sessions.entry(id).expect("rostered").name;
    assert_eq!(name(&unnamed), "Fix the flaky cron test");
    assert_eq!(name(&named), "typescript work");
    let long_title = name(&carried);
    assert_eq!(long_title.chars().count(), 60);
    assert!(long_title.ends_with('…'));

    let active_before = sessions.entry(&named).expect("rostered").last_active_ms;
    std::thread::sleep(std::time::Duration::from_millis(5));
    sessions.select(&named).expect("rostered session selects");
    assert_eq!(
        sessions.entry(&named).expect("rostered").last_active_ms,
        active_before,
        "selecting is not use"
    );
}
