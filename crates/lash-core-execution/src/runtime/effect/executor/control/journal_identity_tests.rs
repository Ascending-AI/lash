use super::*;
use crate::SessionId;

#[test]
fn journal_identity_is_typed_and_session_qualified() {
    let scopes = [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::session_operation("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process(crate::process_id_for_test("shared")),
        ExecutionScope::runtime_operation("shared"),
    ];
    let identities = scopes
        .iter()
        .map(|scope| scope.journal_identity().expect("durable identity"))
        .collect::<Vec<_>>();
    let keys = identities
        .iter()
        .map(EffectJournalIdentity::key)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), scopes.len());
    for identity in &identities[..3] {
        assert_eq!(identity.session_id(), Some(&SessionId::from("session")));
    }
    for identity in &identities[3..] {
        assert_eq!(identity.session_id(), None);
    }
}

/// Every variant survives the round trip, not just the one the drain
/// happens to exercise.
///
/// `from_journal_key` is the drain's only way back from a journal row to a
/// scope, and it re-derives each variant from a `kind` string by hand. A
/// variant whose forward and backward spellings drift apart makes every
/// row written under it undrainable — refused as a scope no version of this
/// runtime writes, which is exactly the wrong answer for a scope this
/// version writes constantly. Only the whole set proves the mapping; one
/// variant proves the plumbing.
#[test]
fn every_scope_variant_round_trips_through_its_journal_key() {
    for scope in [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::session_operation("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process(crate::process_id_for_test("shared")),
        ExecutionScope::runtime_operation("shared"),
    ] {
        let key = scope
            .journal_identity()
            .expect("durable identity")
            .key()
            .to_string();
        assert_eq!(
            ExecutionScope::from_journal_key(&key),
            Some(scope.clone()),
            "scope {scope:?} did not come back from its own journal key `{key}`"
        );
    }
}

/// A key this build cannot read is refused rather than guessed at, which is
/// what lets the drain treat `None` as corruption instead of as a default.
#[test]
fn a_journal_key_this_build_cannot_read_is_refused() {
    for key in [
        "",
        "not json",
        r#"{"version":1,"kind":"turn","session_id":"s","execution_id":"t"}"#,
        r#"{"version":2,"kind":"nonsense","execution_id":"t"}"#,
        // Right kind, missing the field that kind requires.
        r#"{"version":2,"kind":"turn","session_id":"s"}"#,
        // Decodes, but to a scope the forward direction would refuse.
        r#"{"version":2,"kind":"process","execution_id":""}"#,
    ] {
        assert_eq!(
            ExecutionScope::from_journal_key(key),
            None,
            "`{key}` is not a scope this build wrote"
        );
    }
}
