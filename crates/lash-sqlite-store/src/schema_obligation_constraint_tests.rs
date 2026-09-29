//! Execute each physical obligation CHECK against every tagged field shape.

use super::{PROCESS_SCHEMA, SCHEMA, SESSION_ROOTS_TABLES, TRIGGER_SCHEMA};
use rusqlite::Connection;

fn obligation_cases() -> Vec<(String, bool)> {
    let mut cases = Vec::new();
    for state in [
        "NULL",
        "'due'",
        "'claimed'",
        "'delivered'",
        "'stalled'",
        "'unknown'",
    ] {
        for reason in [
            "NULL",
            "'attempts_exhausted'",
            "'refused'",
            "'undecodable'",
            "'unknown'",
        ] {
            for presence in 0_u8..16 {
                let id = presence & 1 != 0;
                let due = presence & 2 != 0;
                let claim = presence & 4 != 0;
                let settled = presence & 8 != 0;
                let valid = match state {
                    "NULL" => !id && !due && !claim && reason == "NULL" && !settled,
                    "'due'" => id && due && !claim && reason == "NULL" && !settled,
                    "'claimed'" => id && due && claim && reason == "NULL" && !settled,
                    "'delivered'" => id && !due && !claim && reason == "NULL" && settled,
                    "'stalled'" => {
                        id && !due
                            && !claim
                            && settled
                            && matches!(
                                reason,
                                "'attempts_exhausted'" | "'refused'" | "'undecodable'"
                            )
                    }
                    _ => false,
                };
                cases.push((
                    format!(
                        "{}, {state}, {}, {}, {reason}, {}",
                        if id { "'obligation'" } else { "NULL" },
                        if due { "17" } else { "NULL" },
                        if claim { "'claim'" } else { "NULL" },
                        if settled { "23" } else { "NULL" }
                    ),
                    valid,
                ));
            }
        }
    }
    cases
}

fn obligation_constraint(line: &str) -> Option<&str> {
    let line = line.trim().trim_end_matches(',');
    (line.starts_with("CONSTRAINT ck_")
        && line.contains("obligation CHECK")
        && line.contains("obligation_state"))
    .then_some(line)
}

#[test]
fn sqlite_obligation_checks_reject_incomplete_variants() {
    let cases = obligation_cases();
    assert_eq!(cases.len(), 480);
    let mut constraints = 0;
    let mut mismatches = Vec::new();
    for (schema, ddl) in [
        ("core", SCHEMA),
        ("process", PROCESS_SCHEMA),
        ("roots", SESSION_ROOTS_TABLES),
        ("trigger", TRIGGER_SCHEMA),
    ] {
        for constraint in ddl.lines().filter_map(obligation_constraint) {
            constraints += 1;
            let cleanup = constraint.contains("ck_artifact_cleanup_obligations_obligation");
            let not_null = if cleanup { "NOT NULL" } else { "" };
            let connection = Connection::open_in_memory().expect("open obligation CHECK fixture");
            // Preserve the production predicate, including the process start prefix.
            let prefix = if constraint.contains("start_obligation_state") {
                "start_"
            } else {
                ""
            };
            connection
                .execute_batch(&format!(
                    "CREATE TABLE obligation_projection (
                    {prefix}obligation_id TEXT {not_null},
                    {prefix}obligation_state TEXT {not_null},
                    {prefix}obligation_due_at_ms BIGINT,
                    {prefix}obligation_claim_token TEXT,
                    {prefix}obligation_stall_reason TEXT,
                    {prefix}obligation_settled_at_ms BIGINT,
                    {constraint});"
                ))
                .expect("create projection with the production obligation CHECK");
            let mut accepted = 0;
            for (values, valid) in &cases {
                let expected = *valid
                    && (!cleanup
                        || (!values.contains("'delivered'") && !values.starts_with("NULL")));
                let result = connection.execute(
                    &format!("INSERT INTO obligation_projection VALUES ({values})"),
                    [],
                );
                if result.is_ok() {
                    accepted += 1;
                }
                if result.is_ok() != expected {
                    mismatches.push(format!(
                        "{schema}: {constraint}: ({values}) expected {expected}, got {result:?}"
                    ));
                }
                if let Err(error) = result {
                    assert!(
                        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation),
                        "unexpected insert error: {error}"
                    );
                }
            }
            if mismatches.is_empty() {
                assert_eq!(accepted, if cleanup { 5 } else { 7 });
            }
        }
    }
    assert_eq!(constraints, 11, "exercise every physical SQLite CHECK");
    assert!(
        mismatches.is_empty(),
        "{} incorrect verdicts across {constraints} constraints:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
