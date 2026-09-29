use sqlx::{Connection, PgConnection};

use crate::support::{SharedDatabaseLock, database_url};

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

pub async fn postgres_obligation_checks_reject_incomplete_variants() {
    let Some(url) = database_url() else {
        eprintln!("skipping PostgreSQL obligation CHECK laws: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect obligation CHECK fixture");
    let cases = obligation_cases();
    assert_eq!(cases.len(), 480);
    let ddl = lash_postgres_store::PostgresStorage::schema_ddl();
    let migrations = include_str!("../../src/postgres/migrate.rs");
    let mut constraints = 0;
    let mut mismatches = Vec::new();
    for (schema, source) in [("schema", ddl), ("migration", migrations)] {
        for constraint in source.lines().filter_map(obligation_constraint) {
            constraints += 1;
            if schema == "migration" {
                assert!(
                    ddl.lines()
                        .filter_map(obligation_constraint)
                        .any(|published| published == constraint),
                    "migration CHECK must match the published DDL: {constraint}"
                );
            }
            let cleanup = constraint.contains("ck_artifact_cleanup_obligations_obligation");
            let not_null = if cleanup { "NOT NULL" } else { "" };
            let prefix = if constraint.contains("start_obligation_state") {
                "start_"
            } else {
                ""
            };
            sqlx::raw_sql(&format!(
                "CREATE TEMP TABLE obligation_projection (
                    {prefix}obligation_id TEXT {not_null},
                    {prefix}obligation_state TEXT {not_null},
                    {prefix}obligation_due_at_ms BIGINT,
                    {prefix}obligation_claim_token TEXT,
                    {prefix}obligation_stall_reason TEXT,
                    {prefix}obligation_settled_at_ms BIGINT,
                    {constraint});"
            ))
            .execute(&mut connection)
            .await
            .expect("create projection with production obligation CHECK");
            let constraint_name = constraint.split_whitespace().nth(1).expect("named CHECK");
            let mut accepted = 0;
            for (values, valid) in &cases {
                let expected = *valid
                    && (!cleanup
                        || (!values.contains("'delivered'") && !values.starts_with("NULL")));
                let result = sqlx::query(&format!(
                    "INSERT INTO obligation_projection VALUES ({values})"
                ))
                .execute(&mut connection)
                .await;
                if result.is_ok() {
                    accepted += 1;
                }
                if result.is_ok() != expected {
                    mismatches.push(format!("{schema}: {constraint_name}: ({values}) expected {expected}, got {result:?}"));
                }
                if let Err(error) = result {
                    let database_error = error
                        .as_database_error()
                        .expect("a constraint violation is a database error");
                    assert!(
                        database_error.is_check_violation()
                            || (cleanup && database_error.code().as_deref() == Some("23502")),
                        "unexpected insert error: {error}"
                    );
                    if database_error.is_check_violation() {
                        assert_eq!(database_error.constraint(), Some(constraint_name));
                    }
                }
            }
            if mismatches.is_empty() {
                assert_eq!(accepted, if cleanup { 5 } else { 7 });
            }
            sqlx::query("DROP TABLE obligation_projection")
                .execute(&mut connection)
                .await
                .expect("drop obligation projection");
        }
    }
    assert_eq!(
        constraints, 12,
        "exercise all ten published and two migration PostgreSQL CHECKs"
    );
    connection
        .close()
        .await
        .expect("close obligation CHECK fixture");
    assert!(
        mismatches.is_empty(),
        "{} incorrect verdicts across {constraints} constraints:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
