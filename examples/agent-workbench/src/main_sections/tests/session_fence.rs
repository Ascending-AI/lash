use super::*;
use lash::SessionId;

// Session fencing (FIG-2358): a delete and a turn submit order through one
// fence, and a delete settles the turn it finds running.

#[test]
fn replacing_a_non_current_session_does_not_rotate_the_selected_session() {
    let ids = WorkbenchSessions::fresh();
    let retired = ids.current();
    ids.ensure(&retired);
    let selected = "workbench-selected-during-delete";
    ids.record(
        SessionId::fixture(selected.to_string()),
        "selected".to_string(),
    );
    ids.select(&SessionId::from(selected))
        .expect("select competing session");

    let (replacement, replaced_current) = ids.replace(&retired);

    assert!(!replaced_current);
    assert_eq!(ids.current(), selected);
    assert!(ids.entry(&retired).is_none());
    assert_eq!(
        ids.entry(&replacement)
            .expect("replacement keeps retired roster slot")
            .name,
        retired
    );
}

#[test]
fn replacing_an_unrostered_session_records_its_replacement() {
    let ids = WorkbenchSessions::fresh();
    let retired = "workbench-external-session";

    let (replacement, replaced_current) = ids.replace(&SessionId::from(retired));

    assert!(!replaced_current);
    let entry = ids
        .entry(&replacement)
        .expect("replacement joins the roster");
    assert_eq!(entry.name, retired);
}

#[test]
fn a_retiring_mark_refuses_the_claim_until_the_delete_is_abandoned() {
    let active_turns = ActiveTurns::default();
    assert!(active_turns.begin_retirement(&SessionId::from("fenced")));
    assert!(!active_turns.begin_retirement(&SessionId::from("fenced")));
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("late-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Refused(SessionRetirement::Retiring)
    );
    assert!(
        active_turns
            .for_session(&SessionId::from("fenced"))
            .is_none()
    );

    active_turns.abandon_retirement(&SessionId::from("fenced"));
    assert_eq!(active_turns.retirement(&SessionId::from("fenced")), None);
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("after-abandon"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Claimed
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("fenced"),
            &TurnId::from("second"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Busy
    );
}

#[test]
fn a_confirmed_retirement_is_never_lifted() {
    let active_turns = ActiveTurns::default();
    active_turns.begin_retirement(&SessionId::from("gone"));
    active_turns.confirm_retirement(&SessionId::from("gone"));
    active_turns.abandon_retirement(&SessionId::from("gone"));
    assert_eq!(
        active_turns.retirement(&SessionId::from("gone")),
        Some(SessionRetirement::Retired)
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("gone"),
            &TurnId::from("late-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Refused(SessionRetirement::Retired)
    );
    // Confirming straight from the durable fact needs no prior mark.
    active_turns.confirm_retirement(&SessionId::from("tombstoned"));
    assert_eq!(
        active_turns.retirement(&SessionId::from("tombstoned")),
        Some(SessionRetirement::Retired)
    );
}

// FIG-3292: the ledger is keyed by session, so the states the old
// `BTreeSet<(SessionId, TurnId)>` beside a separate prompt map could represent
// are gone from memory. A file written by that build can still hold them, so
// the loader has to answer for each one.

fn write_active_turns_file(path: &std::path::Path, body: Value) {
    std::fs::write(
        path,
        serde_json::to_vec(&body).expect("encode active turns"),
    )
    .expect("write active turns fixture");
}

#[test]
fn a_persisted_prompt_for_an_absent_turn_is_dropped_rather_than_restored() {
    let temp = tempfile::tempdir().expect("orphan prompt tempdir");
    let path = temp.path().join("active-turns.json");
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "live-turn"]],
            "prompts": [
                { "session_id": "s1", "turn_id": "retired-turn", "prompt": "orphan",
                  "row_id": "orphan-ui-input", "at": "2026-10-03T00:00:00Z" }
            ],
        }),
    );

    let restored = ActiveTurns::persistent(path).expect("an orphan prompt must not fail the boot");

    let active = restored
        .for_session(&SessionId::from("s1"))
        .expect("the live turn is restored");
    assert_eq!(active.address.turn_id, TurnId::from("live-turn"));
    assert_eq!(
        active.prompt, None,
        "a prompt naming a turn the file does not carry is not that turn's prompt"
    );
}

#[test]
fn a_persisted_second_turn_for_one_session_is_dropped_on_load() {
    let temp = tempfile::tempdir().expect("double claim tempdir");
    let path = temp.path().join("active-turns.json");
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "alpha"], ["s1", "beta"], ["s2", "gamma"]],
            "prompts": [],
        }),
    );

    let restored =
        ActiveTurns::persistent(path).expect("a doubly claimed session must not fail the boot");

    assert_eq!(
        restored
            .for_session(&SessionId::from("s1"))
            .map(|active| active.address.turn_id),
        Some(TurnId::from("alpha")),
        "one session holds one turn; the first in key order survives"
    );
    assert_eq!(
        restored
            .for_session(&SessionId::from("s2"))
            .map(|active| active.address.turn_id),
        Some(TurnId::from("gamma")),
        "a second session is untouched by the first session's repair"
    );
}

#[test]
fn a_persisted_turn_without_a_kind_recovers_one_from_its_id() {
    let temp = tempfile::tempdir().expect("legacy kind tempdir");
    let path = temp.path().join("active-turns.json");
    // A file this build did not write: no `kinds` array at all.
    write_active_turns_file(
        &path,
        json!({
            "turns": [["s1", "workbench-queued-abc"], ["s2", "workbench-turn-def"]],
            "prompts": [],
        }),
    );

    let restored = ActiveTurns::persistent(path).expect("a pre-kind file must still load");

    assert_eq!(
        restored
            .for_session(&SessionId::from("s1"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::Queued)
    );
    assert_eq!(
        restored
            .for_session(&SessionId::from("s2"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::User)
    );
}

#[test]
fn a_claimed_kind_survives_a_restart_without_consulting_the_turn_id() {
    let temp = tempfile::tempdir().expect("kind round trip tempdir");
    let path = temp.path().join("active-turns.json");
    let session_id = SessionId::from("s1");
    // An id whose prefix says "user"; only the persisted kind says otherwise.
    let turn_id = TurnId::from("workbench-turn-mislabelled");
    let turns = ActiveTurns::persistent(path.clone()).expect("open active turns");
    assert_eq!(
        turns.try_insert_with_prompt_for_idle_session(
            &session_id,
            &turn_id,
            WorkbenchTurnKind::Queued,
            Some("held prompt".to_string()),
            None,
        ),
        ActiveTurnClaim::Claimed
    );
    drop(turns);

    let restored = ActiveTurns::persistent(path).expect("reopen active turns");

    let active = restored
        .for_session(&session_id)
        .expect("the claim is restored");
    assert_eq!(active.address.turn_id, turn_id);
    assert_eq!(
        active.kind,
        WorkbenchTurnKind::Queued,
        "the persisted kind outranks whatever the id prefix suggests"
    );
    assert_eq!(
        active.prompt.map(|prompt| prompt.text),
        Some("held prompt".to_string()),
        "the turn and its prompt are restored as one value"
    );
}

#[test]
fn two_sessions_claim_their_own_slots_without_interference() {
    let active_turns = ActiveTurns::default();
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("left"),
            &TurnId::from("left-turn"),
            WorkbenchTurnKind::User,
        ),
        ActiveTurnClaim::Claimed
    );
    assert_eq!(
        active_turns.try_insert_for_idle_session(
            &SessionId::from("right"),
            &TurnId::from("right-turn"),
            WorkbenchTurnKind::Queued,
        ),
        ActiveTurnClaim::Claimed,
        "the claim is a lookup on this session, not a scan of every claim"
    );
    active_turns.remove(&SessionId::from("left"), &TurnId::from("left-turn"));
    assert!(active_turns.for_session(&SessionId::from("left")).is_none());
    assert_eq!(
        active_turns
            .for_session(&SessionId::from("right"))
            .map(|active| active.kind),
        Some(WorkbenchTurnKind::Queued)
    );
}

#[test]
fn releasing_a_claim_a_later_turn_already_replaced_leaves_that_turn_alone() {
    let active_turns = ActiveTurns::default();
    let session_id = SessionId::from("s1");
    active_turns.insert(&session_id, "first", WorkbenchTurnKind::User);
    active_turns.remove(&session_id, &TurnId::from("first"));
    active_turns.insert(&session_id, "second", WorkbenchTurnKind::Queued);

    // The first turn's submission guard runs late.
    active_turns.remove(&session_id, &TurnId::from("first"));

    assert_eq!(
        active_turns
            .for_session(&session_id)
            .map(|active| active.address.turn_id),
        Some(TurnId::from("second")),
        "release is addressed to a turn, not a session-wide clear"
    );
}
