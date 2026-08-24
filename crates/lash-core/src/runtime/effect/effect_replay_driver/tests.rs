use super::*;

const NOW: u64 = 1_000_000;
const TTL: u64 = 30_000;

fn request() -> EffectClaimRequest {
    EffectClaimRequest {
        scope_id: "session:s1".to_string(),
        session_id: Some("s1".to_string()),
        replay_key: "k1".to_string(),
        envelope_hash: "hash-1".to_string(),
        envelope_json: "{\"envelope\":1}".to_string(),
        owner_id: "owner-1".to_string(),
        lease_token: "owner-1:1".to_string(),
        lease_ttl_ms: TTL,
        sleep_duration_ms: None,
        group_key: None,
        strict_replay: false,
    }
}

fn row(status: &str) -> StoredEffectRow {
    row_with_columns(status, None, None)
}

fn row_with_columns(
    status: &str,
    outcome_json: Option<&str>,
    error_json: Option<&str>,
) -> StoredEffectRow {
    StoredEffectRow {
        envelope_hash: "hash-1".to_string(),
        envelope_json: "{\"envelope\":1}".to_string(),
        state: EffectRowState::from_columns(
            status.to_string(),
            outcome_json.map(str::to_string),
            error_json.map(str::to_string),
        ),
        lease_expires_at_ms: 0,
        due_at_ms: None,
    }
}

#[test]
fn status_columns_are_the_persisted_journal_bytes() {
    for status in [
        EffectRowStatus::InProgress,
        EffectRowStatus::Completed,
        EffectRowStatus::Failed,
    ] {
        assert_eq!(EffectRowStatus::parse(status.column()), Some(status));
    }
    assert_eq!(EffectRowStatus::InProgress.column(), "in_progress");
    assert_eq!(EffectRowStatus::Completed.column(), "completed");
    assert_eq!(EffectRowStatus::Failed.column(), "failed");
    assert_eq!(EffectRowStatus::parse("cancelled"), None);
}

#[test]
fn row_state_decodes_every_status_payload_combination() {
    let unexpected = |status, outcome_json_present, error_json_present| {
        EffectRowState::Corrupt(EffectRowDefect::UnexpectedPayloads {
            status,
            outcome_json_present,
            error_json_present,
        })
    };
    let unknown = || {
        EffectRowState::Corrupt(EffectRowDefect::UnknownStatus {
            status: "other".to_string(),
        })
    };
    let cases: Vec<(&str, Option<&str>, Option<&str>, EffectRowState)> = vec![
        ("in_progress", None, None, EffectRowState::InProgress),
        (
            "in_progress",
            Some("outcome"),
            None,
            unexpected(EffectRowStatus::InProgress, true, false),
        ),
        (
            "in_progress",
            None,
            Some("error"),
            unexpected(EffectRowStatus::InProgress, false, true),
        ),
        (
            "in_progress",
            Some("outcome"),
            Some("error"),
            unexpected(EffectRowStatus::InProgress, true, true),
        ),
        (
            "completed",
            None,
            None,
            EffectRowState::Corrupt(EffectRowDefect::MissingOutcome),
        ),
        (
            "completed",
            Some("outcome"),
            None,
            EffectRowState::Settled(EffectTerminal::Completed {
                outcome_json: "outcome".to_string(),
            }),
        ),
        (
            "completed",
            None,
            Some("error"),
            EffectRowState::Corrupt(EffectRowDefect::MissingOutcome),
        ),
        (
            "completed",
            Some("outcome"),
            Some("error"),
            unexpected(EffectRowStatus::Completed, true, true),
        ),
        (
            "failed",
            None,
            None,
            EffectRowState::Corrupt(EffectRowDefect::MissingError),
        ),
        (
            "failed",
            Some("outcome"),
            None,
            EffectRowState::Corrupt(EffectRowDefect::MissingError),
        ),
        (
            "failed",
            None,
            Some("error"),
            EffectRowState::Settled(EffectTerminal::Failed {
                error_json: "error".to_string(),
            }),
        ),
        (
            "failed",
            Some("outcome"),
            Some("error"),
            unexpected(EffectRowStatus::Failed, true, true),
        ),
        ("other", None, None, unknown()),
        ("other", Some("outcome"), None, unknown()),
        ("other", None, Some("error"), unknown()),
        ("other", Some("outcome"), Some("error"), unknown()),
    ];

    for (status, outcome_json, error_json, expected) in cases {
        assert_eq!(
            EffectRowState::from_columns(
                status.to_string(),
                outcome_json.map(str::to_string),
                error_json.map(str::to_string),
            ),
            expected,
            "status={status}, outcome_json={}, error_json={}",
            outcome_json.is_some(),
            error_json.is_some(),
        );
    }
}

#[test]
fn a_missing_row_is_claimed_by_insert() {
    assert_eq!(
        decide_effect_claim(None, &request(), NOW),
        EffectClaimDecision::Insert(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: None,
            now_ms: NOW,
        })
    );
}

#[test]
fn a_missing_row_under_strict_replay_is_refused_without_writing() {
    let request = EffectClaimRequest {
        strict_replay: true,
        ..request()
    };
    assert_eq!(
        decide_effect_claim(None, &request, NOW),
        EffectClaimDecision::Report(EffectClaimObservation::StrictReplayMiss)
    );
}

#[test]
fn a_fresh_sleep_claim_derives_its_due_time_from_the_claim_instant() {
    let request = EffectClaimRequest {
        sleep_duration_ms: Some(5_000),
        ..request()
    };
    assert_eq!(
        decide_effect_claim(None, &request, NOW),
        EffectClaimDecision::Insert(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: Some(NOW + 5_000),
            now_ms: NOW,
        })
    );
}

#[test]
fn an_envelope_hash_mismatch_outranks_every_status() {
    for status in ["in_progress", "completed", "failed", "nonsense"] {
        let mut stored = row(status);
        stored.envelope_hash = "hash-2".to_string();
        stored.envelope_json = "{\"envelope\":2}".to_string();
        stored.lease_expires_at_ms = NOW + TTL;
        stored.state = EffectRowState::from_columns(
            status.to_string(),
            Some("{}".to_string()),
            Some("{}".to_string()),
        );
        assert_eq!(
            decide_effect_claim(Some(&stored), &request(), NOW),
            EffectClaimDecision::Report(EffectClaimObservation::ReplayMismatch {
                recorded_envelope_json: "{\"envelope\":2}".to_string(),
                stored_envelope_hash: "hash-2".to_string(),
            }),
            "status `{status}` must not outrank a canonical envelope mismatch"
        );
    }
}

#[test]
fn a_completed_row_replays_its_outcome_and_recorded_due_time() {
    let mut stored = row_with_columns("completed", Some("{\"kind\":\"sleep\"}"), None);
    stored.due_at_ms = Some(NOW + 90);
    assert_eq!(
        decide_effect_claim(Some(&stored), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::Completed {
            outcome_json: "{\"kind\":\"sleep\"}".to_string(),
            due_at_ms: Some(NOW + 90),
        })
    );
}

#[test]
fn a_completed_row_with_both_payloads_is_corrupt() {
    let stored = row_with_columns(
        "completed",
        Some("{\"kind\":\"sleep\"}"),
        Some("{\"code\":\"contradiction\"}"),
    );
    assert_eq!(
        decide_effect_claim(Some(&stored), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
            defect: EffectRowDefect::UnexpectedPayloads {
                status: EffectRowStatus::Completed,
                outcome_json_present: true,
                error_json_present: true,
            },
        }),
        "the store-boundary decoder must not discard the contradictory error payload"
    );
}

#[test]
fn a_failed_row_replays_its_error() {
    let stored = row_with_columns("failed", None, Some("{\"code\":\"boom\"}"));
    assert_eq!(
        decide_effect_claim(Some(&stored), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::Failed {
            error_json: "{\"code\":\"boom\"}".to_string(),
        })
    );
}

#[test]
fn terminal_rows_missing_their_payload_are_corrupt() {
    assert_eq!(
        decide_effect_claim(Some(&row("completed")), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
            defect: EffectRowDefect::MissingOutcome,
        })
    );
    assert_eq!(
        decide_effect_claim(Some(&row("failed")), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
            defect: EffectRowDefect::MissingError,
        })
    );
}

#[test]
fn an_unknown_status_is_corrupt_rather_than_claimable() {
    let stored = row("half_done");
    assert_eq!(
        decide_effect_claim(Some(&stored), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::CorruptRow {
            defect: EffectRowDefect::UnknownStatus {
                status: "half_done".to_string(),
            },
        })
    );
}

#[test]
fn a_live_lease_is_busy_and_an_expired_one_is_taken_over() {
    let mut live = row("in_progress");
    live.lease_expires_at_ms = NOW + 1;
    assert_eq!(
        decide_effect_claim(Some(&live), &request(), NOW),
        EffectClaimDecision::Report(EffectClaimObservation::Busy {
            retry_at_ms: NOW + 1,
        })
    );

    let mut expired = row("in_progress");
    expired.lease_expires_at_ms = NOW;
    assert_eq!(
        decide_effect_claim(Some(&expired), &request(), NOW),
        EffectClaimDecision::TakeOver(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: None,
            now_ms: NOW,
        }),
        "a lease expiring exactly now is expired, not live"
    );
}

#[test]
fn a_takeover_keeps_the_recorded_due_time_instead_of_restarting_the_sleep() {
    let mut expired = row("in_progress");
    expired.lease_expires_at_ms = NOW - 1;
    expired.due_at_ms = Some(NOW + 10);
    let request = EffectClaimRequest {
        sleep_duration_ms: Some(5_000),
        ..request()
    };
    assert_eq!(
        decide_effect_claim(Some(&expired), &request, NOW),
        EffectClaimDecision::TakeOver(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: Some(NOW + 10),
            now_ms: NOW,
        })
    );
}

#[test]
fn a_takeover_of_a_sleep_without_a_recorded_due_time_derives_one() {
    let mut expired = row("in_progress");
    expired.lease_expires_at_ms = NOW - 1;
    let request = EffectClaimRequest {
        sleep_duration_ms: Some(5_000),
        ..request()
    };
    assert_eq!(
        decide_effect_claim(Some(&expired), &request, NOW),
        EffectClaimDecision::TakeOver(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: Some(NOW + 5_000),
            now_ms: NOW,
        })
    );
}

#[test]
fn strict_replay_never_refuses_a_row_that_exists() {
    let stored = row_with_columns("completed", Some("{}"), None);
    let request = EffectClaimRequest {
        strict_replay: true,
        ..request()
    };
    assert_eq!(
        decide_effect_claim(Some(&stored), &request, NOW),
        EffectClaimDecision::Report(EffectClaimObservation::Completed {
            outcome_json: "{}".to_string(),
            due_at_ms: None,
        })
    );
}

#[test]
fn strict_replay_takes_over_an_expired_in_progress_row() {
    let mut expired = row("in_progress");
    expired.lease_expires_at_ms = NOW - 1;
    expired.due_at_ms = Some(NOW + 10);
    let request = EffectClaimRequest {
        strict_replay: true,
        sleep_duration_ms: Some(5_000),
        ..request()
    };
    assert_eq!(
        decide_effect_claim(Some(&expired), &request, NOW),
        EffectClaimDecision::TakeOver(EffectLeaseStamp {
            lease_expires_at_ms: NOW + TTL,
            due_at_ms: Some(NOW + 10),
            now_ms: NOW,
        }),
        "strict replay redriving a crashed-mid-execution effect must take the \
         abandoned lease over, not report the effect missing"
    );
}

#[test]
fn strict_replay_reports_a_live_lease_as_busy() {
    let mut live = row("in_progress");
    live.lease_expires_at_ms = NOW + 1;
    let request = EffectClaimRequest {
        strict_replay: true,
        ..request()
    };
    assert_eq!(
        decide_effect_claim(Some(&live), &request, NOW),
        EffectClaimDecision::Report(EffectClaimObservation::Busy {
            retry_at_ms: NOW + 1,
        }),
        "strict replay must wait behind the live owner, not report the effect missing"
    );
}

#[test]
fn lease_stamps_saturate_instead_of_overflowing() {
    let request = EffectClaimRequest {
        lease_ttl_ms: u64::MAX,
        sleep_duration_ms: Some(u64::MAX),
        ..request()
    };
    assert_eq!(
        decide_effect_claim(None, &request, NOW),
        EffectClaimDecision::Insert(EffectLeaseStamp {
            lease_expires_at_ms: u64::MAX,
            due_at_ms: Some(u64::MAX),
            now_ms: NOW,
        })
    );
}

#[test]
fn vocabularies_reproduce_each_backends_shipped_codes() {
    let sqlite = EffectReplayVocabulary::sqlite();
    let postgres = EffectReplayVocabulary::postgres();
    assert_eq!(
        sqlite.code(EffectReplayFailure::LeaseLost),
        RuntimeErrorCode::SqliteEffectReplayLeaseLost
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::HashConflict),
        RuntimeErrorCode::SqliteEffectReplayHashConflict
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::KeyMissing),
        RuntimeErrorCode::SqliteEffectReplayKeyMissing
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::Missing),
        RuntimeErrorCode::SqliteEffectReplayMissing
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::CorruptRow),
        RuntimeErrorCode::SqliteEffectReplayCorruptRow
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::Store),
        RuntimeErrorCode::SqliteEffectReplayStore
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::Encode),
        RuntimeErrorCode::SqliteEffectReplayEncode
    );
    assert_eq!(
        sqlite.code(EffectReplayFailure::Decode),
        RuntimeErrorCode::SqliteEffectReplayDecode
    );
    assert_eq!(
        postgres.code(EffectReplayFailure::LeaseLost),
        RuntimeErrorCode::PostgresEffectReplayLeaseLost
    );
    assert_eq!(
        postgres
            .error(EffectReplayFailure::CorruptRow, "boom")
            .message,
        "boom".to_string()
    );
}

#[test]
fn row_defects_render_the_messages_hosts_already_see() {
    assert_eq!(
        EffectRowDefect::MissingOutcome.message(),
        "completed runtime effect row is missing outcome_json"
    );
    assert_eq!(
        EffectRowDefect::MissingError.message(),
        "failed runtime effect row is missing error_json"
    );
    assert_eq!(
        EffectRowDefect::UnexpectedPayloads {
            status: EffectRowStatus::Completed,
            outcome_json_present: true,
            error_json_present: true,
        }
        .message(),
        "runtime effect replay status `completed` contradicts its payload columns: \
         outcome_json present = true, error_json present = true"
    );
    assert_eq!(
        EffectRowDefect::UnknownStatus {
            status: "x".to_string()
        }
        .message(),
        "unknown runtime effect replay status `x`"
    );
    assert_eq!(
        EffectRowDefect::VanishedUnderClaim.message(),
        "effect replay insert conflicted but no row could be selected"
    );
}

#[test]
fn terminals_write_exactly_one_payload_column() {
    let completed = EffectTerminal::Completed {
        outcome_json: "{\"ok\":true}".to_string(),
    };
    assert_eq!(completed.status(), EffectRowStatus::Completed);
    assert_eq!(completed.outcome_json(), Some("{\"ok\":true}"));
    assert_eq!(completed.error_json(), None);

    let failed = EffectTerminal::Failed {
        error_json: "{\"code\":\"boom\"}".to_string(),
    };
    assert_eq!(failed.status(), EffectRowStatus::Failed);
    assert_eq!(failed.outcome_json(), None);
    assert_eq!(failed.error_json(), Some("{\"code\":\"boom\"}"));
}
