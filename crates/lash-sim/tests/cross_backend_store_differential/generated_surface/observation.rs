//! Observation and normalization for the generated surface differential:
//! per-backend readers over the durable tables, the normalized row shapes,
//! and the cross-backend agreement predicate.

use super::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct ProcessRows {
    pub(super) records: Vec<serde_json::Value>,
    pub(super) events: Vec<serde_json::Value>,
    pub(super) observers: Vec<(SessionId, ProcessId)>,
    pub(super) tombstones: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct SurfaceState {
    pub(super) processes: ProcessRows,
}

pub(super) enum SurfaceReader {
    Sqlite { process_path: PathBuf },
    Postgres { pool: PgPool },
}

impl SurfaceReader {
    pub(super) async fn observe(&self) -> SurfaceState {
        match self {
            Self::Sqlite { process_path } => read_sqlite_surface(process_path),
            Self::Postgres { pool } => read_postgres_surface(pool).await,
        }
    }
}

pub(super) fn normalized_json(mut value: serde_json::Value) -> serde_json::Value {
    normalize_json_fields(&mut value);
    value
}

pub(super) fn normalize_json_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_fields(value);
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                match name.as_str() {
                    "created_at_ms"
                    | "updated_at_ms"
                    | "occurred_at_ms"
                    | "deleted_at_ms"
                    | "pruned_at_ms"
                    | "first_attempt_ms"
                    | "next_attempt_at_ms"
                    | "expires_at_ms"
                    | "created_at_epoch_ms"
                    | "updated_at_epoch_ms"
                    | "claimed_at_epoch_ms"
                    | "expires_at_epoch_ms"
                    | "resolved_at_ms"
                    | "lease_expires_at_ms"
                    | "due_at_ms"
                    | "since_ms"
                    | "last_refused_ms"
                    | "occurred_at" => {
                        if !value.is_null() {
                            *value = serde_json::json!("normalized_timestamp");
                        }
                    }
                    "claim_token" | "lease_token" | "lease_owner_id" => {
                        *value = serde_json::Value::Bool(!value.is_null());
                    }
                    _ => normalize_json_fields(value),
                }
            }
        }
        _ => {}
    }
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]

pub(super) fn read_sqlite_surface(process_path: &Path) -> SurfaceState {
    let process = rusqlite::Connection::open(process_path).expect("open SQLite process reader");

    let records = sqlite_simple_json_rows(
        &process,
        "SELECT record_json, change_seq FROM processes ORDER BY process_id",
        |row| {
            let record: String = row.get(0)?;
            Ok(normalized_json(serde_json::json!({
                "change_seq": row.get::<_, i64>(1)?,
                "record": serde_json::from_str::<serde_json::Value>(&record).unwrap(),
            })))
        },
    );
    let events = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, event_json FROM process_events ORDER BY process_id, sequence",
        |row| {
            let event: String = row.get(1)?;
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "event": serde_json::from_str::<serde_json::Value>(&event).unwrap(),
            })))
        },
    );
    let observers = {
        let mut stmt = process
            .prepare("SELECT session_id, process_id FROM process_observers ORDER BY session_id, process_id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                SessionId::fixture(row.get::<_, String>(0)?),
                stored_process_id(row.get::<_, String>(1)?),
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let tombstones = sqlite_simple_json_rows(
        &process,
        "SELECT process_id, terminal_label, pruned_at_ms, pruned_change_seq
         FROM process_tombstones ORDER BY process_id",
        |row| {
            Ok(normalized_json(serde_json::json!({
                "process_id": row.get::<_, String>(0)?,
                "terminal_label": row.get::<_, String>(1)?,
                "pruned_at_ms": row.get::<_, i64>(2)?,
                "pruned_change_seq": row.get::<_, i64>(3)?,
            })))
        },
    );
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            tombstones,
        },
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn sqlite_simple_json_rows<F>(
    connection: &rusqlite::Connection,
    query: &str,
    decode: F,
) -> Vec<serde_json::Value>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
{
    let mut stmt = connection.prepare(query).unwrap();
    stmt.query_map([], decode)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_surface(pool: &PgPool) -> SurfaceState {
    let record_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT record_json, change_seq FROM lash_processes ORDER BY process_id")
            .fetch_all(pool)
            .await
            .unwrap();
    let records = record_rows.into_iter().map(|(record, change_seq)| normalized_json(serde_json::json!({"change_seq": change_seq, "record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))).collect();
    let event_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT process_id, event_json FROM lash_process_events ORDER BY process_id, sequence",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let events = event_rows.into_iter().map(|(process_id, event)| normalized_json(serde_json::json!({"process_id": process_id, "event": serde_json::from_str::<serde_json::Value>(&event).unwrap()}))).collect();
    let observers = sqlx::query_as(
        "SELECT session_id, process_id FROM lash_process_observers ORDER BY session_id, process_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(session_id, process_id): (String, String)| {
        (
            SessionId::fixture(session_id),
            stored_process_id(process_id),
        )
    })
    .collect();
    let tombstone_rows: Vec<(String, String, i64, i64)> = sqlx::query_as("SELECT process_id, terminal_label, pruned_at_ms, pruned_change_seq FROM lash_process_tombstones ORDER BY process_id").fetch_all(pool).await.unwrap();
    let tombstones = tombstone_rows.into_iter().map(|(process_id, terminal_label, pruned_at_ms, pruned_change_seq)| normalized_json(serde_json::json!({"process_id": process_id, "terminal_label": terminal_label, "pruned_at_ms": pruned_at_ms, "pruned_change_seq": pruned_change_seq}))).collect();
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            tombstones,
        },
    }
}

pub(super) fn states_agree(observations: &[(&str, SurfaceState)]) -> bool {
    observations
        .windows(2)
        .all(|pair| pair[0].1.processes == pair[1].1.processes)
}

/// A process id a store row holds: always one a registrar minted, or a
/// fixture of the same shape.
#[expect(
    clippy::expect_used,
    reason = "test support: a stored process id that does not decode is a store defect the differential must surface"
)]
fn stored_process_id(value: String) -> ProcessId {
    ProcessId::parse(&value).expect("a stored process id decodes")
}
