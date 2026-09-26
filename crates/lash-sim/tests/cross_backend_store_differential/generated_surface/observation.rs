//! Observation and normalization for the generated surface differential:
//! per-backend readers over the durable tables, the normalized row shapes,
//! and the cross-backend agreement predicate.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct ProcessLeaseObservation {
    pub(super) process_id: ProcessId,
    pub(super) owner: serde_json::Value,
    pub(super) lease_token_present: bool,
    pub(super) fencing_token: u64,
    // PostgreSQL stamps `lease_expires_at_ms` from database wall time and
    // re-stamps it when a held lease is extended while `lease_claimed_at_ms`
    // stays put, so `expires - claimed` carries real elapsed time there; the
    // SQLite backend reads the harness's frozen injected clock and reports the
    // requested term verbatim. The durable temporal contract that crosses the
    // backend boundary is `claimed`, not an epoch-difference.
    pub(super) claimed: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct ProcessRows {
    pub(super) records: Vec<serde_json::Value>,
    pub(super) events: Vec<serde_json::Value>,
    pub(super) observers: Vec<(SessionId, ProcessId)>,
    pub(super) leases: Vec<ProcessLeaseObservation>,
    pub(super) wake_deliveries: Vec<serde_json::Value>,
    pub(super) wake_allocation_floors: Vec<(SessionId, ProcessId, u64)>,
    pub(super) tombstones: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct TriggerRows {
    pub(super) subscriptions: Vec<serde_json::Value>,
    pub(super) mutation_receipts: Vec<serde_json::Value>,
    pub(super) occurrences: Vec<serde_json::Value>,
    pub(super) deliveries: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(super) struct SurfaceState {
    pub(super) processes: ProcessRows,
    pub(super) wake_redelivery_fences: Vec<(String, String, u64)>,
    pub(super) triggers: TriggerRows,
    /// `turn_parks`, normalized: the session's parked-turn record (FIG-3586).
    pub(super) turn_parks: Vec<serde_json::Value>,
    /// The `load_turn_park` answers the record ops produced, in operation
    /// order. Recorded by the runner, not read off the tables.
    pub(super) turn_park_loads: Vec<serde_json::Value>,
}

pub(super) enum SurfaceReader {
    Sqlite {
        runtime_path: PathBuf,
        process_path: PathBuf,
        trigger_path: PathBuf,
    },
    Postgres {
        pool: PgPool,
    },
}

impl SurfaceReader {
    pub(super) async fn observe(&self) -> SurfaceState {
        match self {
            Self::Sqlite {
                runtime_path,
                process_path,
                trigger_path,
            } => read_sqlite_surface(runtime_path, process_path, trigger_path),
            Self::Postgres { pool } => read_postgres_surface(pool).await,
        }
    }
}

pub(super) fn normalized_json(mut value: serde_json::Value) -> serde_json::Value {
    normalize_json_fields(&mut value, None);
    value
}

pub(super) fn normalized_trigger_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    normalize_json_fields(&mut value, Some(incarnations));
    value
}

pub(super) fn normalized_trigger_receipt_json(
    value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    normalized_trigger_json(value, incarnations)
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) fn normalized_trigger_delivery_json(
    mut value: serde_json::Value,
    incarnations: &mut BTreeMap<String, String>,
) -> serde_json::Value {
    let fields = value
        .as_object_mut()
        .expect("trigger delivery observation must be an object");
    // A delivery's process is minted when the delivery starts it, so the two
    // backends agree on whether a process is bound, not on its id.
    let process_id_bound = fields
        .remove("process_id")
        .is_some_and(|process_id| !process_id.is_null());
    fields.insert(
        "process_id_bound".to_string(),
        serde_json::Value::Bool(process_id_bound),
    );
    normalized_trigger_json(value, incarnations)
}

pub(super) fn normalize_json_fields(
    value: &mut serde_json::Value,
    mut incarnations: Option<&mut BTreeMap<String, String>>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_fields(value, incarnations.as_deref_mut());
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
                    "incarnation" | "subscription_incarnation" => {
                        if let (Some(raw), Some(map)) =
                            (value.as_str(), incarnations.as_deref_mut())
                        {
                            let next = map.len();
                            let alias = map
                                .entry(raw.to_string())
                                .or_insert_with(|| format!("incarnation-{next}"))
                                .clone();
                            *value = serde_json::Value::String(alias);
                        }
                    }
                    _ => normalize_json_fields(value, incarnations.as_deref_mut()),
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
pub(super) fn read_sqlite_surface(
    runtime_path: &Path,
    process_path: &Path,
    trigger_path: &Path,
) -> SurfaceState {
    let runtime = rusqlite::Connection::open(runtime_path).expect("open SQLite runtime reader");
    let process = rusqlite::Connection::open(process_path).expect("open SQLite process reader");
    let trigger = rusqlite::Connection::open(trigger_path).expect("open SQLite trigger reader");
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
                SessionId::from(row.get::<_, String>(0)?),
                stored_process_id(row.get::<_, String>(1)?),
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let leases = {
        let mut stmt = process
            .prepare(
                "SELECT process_id, lease_owner_id, lease_owner_incarnation_id,
                    lease_token, lease_fencing_token, lease_claimed_at_ms,
                    lease_expires_at_ms
             FROM process_leases ORDER BY process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            let owner_id: Option<String> = row.get(1)?;
            let incarnation_id: Option<String> = row.get(2)?;
            let claimed: i64 = row.get(5)?;
            Ok(ProcessLeaseObservation {
                process_id: stored_process_id(row.get::<_, String>(0)?),
                lease_token_present: row.get::<_, Option<String>>(3)?.is_some(),
                owner: if row.get::<_, Option<String>>(3)?.is_some() {
                    serde_json::to_value(decode_lease_owner(owner_id, incarnation_id)).unwrap()
                } else {
                    serde_json::Value::Null
                },
                fencing_token: row.get::<_, i64>(4)? as u64,
                claimed: claimed != 0,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let wake_deliveries = sqlite_simple_json_rows(
        &process,
        "SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms,
                next_attempt_at_ms, expires_at_ms, discard_reason
         FROM process_wake_deliveries ORDER BY delivery_id",
        |row| {
            let json: String = row.get(1)?;
            let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
            let mut value = serde_json::json!({
                "delivery_id": row.get::<_, String>(0)?,
                "wake": wake,
            });
            let fields = value.as_object_mut().unwrap();
            fields.insert(
                "state".to_string(),
                serde_json::json!(row.get::<_, String>(2)?),
            );
            if let Some(token) = row.get::<_, Option<String>>(3)? {
                fields.insert("claim_token".to_string(), serde_json::json!(token));
            } else {
                fields.remove("claim_token");
            }
            fields.insert(
                "attempts".to_string(),
                serde_json::json!(row.get::<_, i64>(4)?),
            );
            fields.insert(
                "first_attempt_ms".to_string(),
                serde_json::json!(row.get::<_, Option<i64>>(5)?),
            );
            fields.insert(
                "next_attempt_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(6)?),
            );
            fields.insert(
                "expires_at_ms".to_string(),
                serde_json::json!(row.get::<_, i64>(7)?),
            );
            fields.insert(
                "discard_reason".to_string(),
                serde_json::json!(row.get::<_, Option<String>>(8)?),
            );
            Ok(normalized_json(value))
        },
    );
    let wake_allocation_floors = {
        let mut stmt = process
            .prepare(
                "SELECT target_session_id, process_id, allocation_floor
                 FROM wake_allocation_floors ORDER BY target_session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                SessionId::from(row.get::<_, String>(0)?),
                stored_process_id(row.get::<_, String>(1)?),
                row.get::<_, i64>(2)? as u64,
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
    let wake_redelivery_fences = {
        let mut stmt = runtime
            .prepare(
                "SELECT session_id, process_id, allocation_floor
             FROM wake_redelivery_fences ORDER BY session_id, process_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_sqlite_triggers(&trigger),
        turn_parks: sqlite_simple_json_rows(
            &runtime,
            "SELECT session_id, turn_id, park_id, reason_code, reason_json,
                    since_ms, last_refused_ms, attempts FROM turn_parks
             ORDER BY session_id",
            |row| {
                let reason: String = row.get(4)?;
                Ok(normalized_json(serde_json::json!({
                    "session_id": row.get::<_, String>(0)?,
                    "turn_id": row.get::<_, String>(1)?,
                    "park_id": row.get::<_, i64>(2)?,
                    "reason_code": row.get::<_, String>(3)?,
                    "reason": serde_json::from_str::<serde_json::Value>(&reason).unwrap(),
                    "since_ms": row.get::<_, i64>(5)?,
                    "last_refused_ms": row.get::<_, i64>(6)?,
                    "attempts": row.get::<_, i64>(7)?,
                })))
            },
        ),
        turn_park_loads: Vec::new(),
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
pub(super) fn read_sqlite_triggers(connection: &rusqlite::Connection) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions = sqlite_simple_json_rows(
        connection,
        "SELECT record_json FROM trigger_subscriptions ORDER BY subscription_id",
        |row| {
            let json: String = row.get(0)?;
            Ok(serde_json::from_str(&json).unwrap())
        },
    )
    .into_iter()
    .map(|row| normalized_trigger_json(row, &mut incarnations))
    .collect();
    let mutation_receipts = sqlite_simple_json_rows(connection, "SELECT operation_id, owner_kind, owner_id, request_fingerprint, result_json FROM trigger_mutation_receipts ORDER BY operation_id", |row| {
        let result: String = row.get(4)?;
        Ok(serde_json::json!({"operation_id": row.get::<_, String>(0)?, "owner_kind": row.get::<_, String>(1)?, "owner_id": row.get::<_, String>(2)?, "request_fingerprint": row.get::<_, String>(3)?, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_receipt_json(row, &mut incarnations)).collect();
    let occurrences = sqlite_simple_json_rows(connection, "SELECT record_json FROM trigger_occurrences ORDER BY occurrence_id", |row| {
        let record: String = row.get(0)?;
        Ok(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_json(row, &mut incarnations)).collect();
    let deliveries = sqlite_simple_json_rows(connection, "SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM trigger_deliveries ORDER BY occurrence_id, subscription_id", |row| {
        let snapshot: String = row.get(3)?;
        Ok(serde_json::json!({"occurrence_id": row.get::<_, String>(0)?, "subscription_id": row.get::<_, String>(1)?, "process_id": row.get::<_, String>(2)?, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}))
    }).into_iter().map(|row| normalized_trigger_delivery_json(row, &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
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
        (SessionId::from(session_id), stored_process_id(process_id))
    })
    .collect();
    type PgLeaseRow = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
    );
    let lease_rows: Vec<PgLeaseRow> = sqlx::query_as("SELECT process_id, lease_owner_id, lease_owner_incarnation_id, lease_token, lease_fencing_token, lease_claimed_at_ms FROM lash_process_leases ORDER BY process_id").fetch_all(pool).await.unwrap();
    let leases = lease_rows
        .into_iter()
        .map(
            |(process_id, owner_id, incarnation, token, fencing, claimed)| {
                ProcessLeaseObservation {
                    process_id: stored_process_id(process_id),
                    owner: if token.is_some() {
                        serde_json::to_value(decode_lease_owner(owner_id, incarnation)).unwrap()
                    } else {
                        serde_json::Value::Null
                    },
                    lease_token_present: token.is_some(),
                    fencing_token: fencing as u64,
                    claimed: claimed != 0,
                }
            },
        )
        .collect();
    type PgWakeRow = (
        String,
        String,
        String,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        i64,
        Option<String>,
    );
    let wake_rows: Vec<PgWakeRow> = sqlx::query_as("SELECT delivery_id, delivery_json, state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms, discard_reason FROM lash_process_wake_deliveries ORDER BY delivery_id").fetch_all(pool).await.unwrap();
    let wake_deliveries = wake_rows
        .into_iter()
        .map(
            |(
                delivery_id,
                json,
                state,
                token,
                attempts,
                first_attempt,
                next_attempt,
                expires,
                discard,
            )| {
                let wake: serde_json::Value = serde_json::from_str(&json).unwrap();
                let mut value = serde_json::json!({
                    "delivery_id": delivery_id,
                    "wake": wake,
                });
                let fields = value.as_object_mut().unwrap();
                fields.insert("state".to_string(), serde_json::json!(state));
                if let Some(token) = token {
                    fields.insert("claim_token".to_string(), serde_json::json!(token));
                } else {
                    fields.remove("claim_token");
                }
                fields.insert("attempts".to_string(), serde_json::json!(attempts));
                fields.insert(
                    "first_attempt_ms".to_string(),
                    serde_json::json!(first_attempt),
                );
                fields.insert(
                    "next_attempt_at_ms".to_string(),
                    serde_json::json!(next_attempt),
                );
                fields.insert("expires_at_ms".to_string(), serde_json::json!(expires));
                fields.insert("discard_reason".to_string(), serde_json::json!(discard));
                normalized_json(value)
            },
        )
        .collect();
    let allocation_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT target_session_id, process_id, allocation_floor
         FROM lash_wake_allocation_floors ORDER BY target_session_id, process_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let wake_allocation_floors = allocation_rows
        .into_iter()
        .map(|(session, process, sequence)| {
            (
                SessionId::from(session),
                stored_process_id(process),
                sequence as u64,
            )
        })
        .collect();
    let tombstone_rows: Vec<(String, String, i64, i64)> = sqlx::query_as("SELECT process_id, terminal_label, pruned_at_ms, pruned_change_seq FROM lash_process_tombstones ORDER BY process_id").fetch_all(pool).await.unwrap();
    let tombstones = tombstone_rows.into_iter().map(|(process_id, terminal_label, pruned_at_ms, pruned_change_seq)| normalized_json(serde_json::json!({"process_id": process_id, "terminal_label": terminal_label, "pruned_at_ms": pruned_at_ms, "pruned_change_seq": pruned_change_seq}))).collect();
    let fence_rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT session_id, process_id, allocation_floor FROM lash_wake_redelivery_fences ORDER BY session_id, process_id").fetch_all(pool).await.unwrap();
    let wake_redelivery_fences = fence_rows
        .into_iter()
        .map(|(session, process, sequence)| (session, process, sequence as u64))
        .collect();
    SurfaceState {
        processes: ProcessRows {
            records,
            events,
            observers,
            leases,
            wake_deliveries,
            wake_allocation_floors,
            tombstones,
        },
        wake_redelivery_fences,
        triggers: read_postgres_triggers(pool).await,
        turn_parks: read_postgres_turn_parks(pool).await,
        turn_park_loads: Vec::new(),
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_turn_parks(pool: &PgPool) -> Vec<serde_json::Value> {
    type Row = (String, String, i64, String, String, i64, i64, i64);
    sqlx::query_as::<_, Row>(
        "SELECT session_id, turn_id, park_id, reason_code, reason_json,
                since_ms, last_refused_ms, attempts
         FROM lash_turn_parks ORDER BY session_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(
        |(
            session_id,
            turn_id,
            park_id,
            reason_code,
            reason,
            since_ms,
            last_refused_ms,
            attempts,
        )| {
            normalized_json(serde_json::json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "park_id": park_id,
                "reason_code": reason_code,
                "reason": serde_json::from_str::<serde_json::Value>(&reason).unwrap(),
                "since_ms": since_ms,
                "last_refused_ms": last_refused_ms,
                "attempts": attempts,
            }))
        },
    )
    .collect()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_postgres_triggers(pool: &PgPool) -> TriggerRows {
    let mut incarnations = BTreeMap::new();
    let subscriptions: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_subscriptions ORDER BY subscription_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let subscriptions = subscriptions
        .into_iter()
        .map(|row| normalized_trigger_json(serde_json::from_str(&row).unwrap(), &mut incarnations))
        .collect();
    let receipts: Vec<(String, String, String, String, String)> = sqlx::query_as("SELECT operation_id, owner_kind, owner_id, request_fingerprint, result_json FROM lash_trigger_mutation_receipts ORDER BY operation_id").fetch_all(pool).await.unwrap();
    let mutation_receipts = receipts.into_iter().map(|(operation_id, owner_kind, owner_id, request_fingerprint, result)| normalized_trigger_receipt_json(serde_json::json!({"operation_id": operation_id, "owner_kind": owner_kind, "owner_id": owner_id, "request_fingerprint": request_fingerprint, "result": serde_json::from_str::<serde_json::Value>(&result).unwrap()}), &mut incarnations)).collect();
    let occurrence_rows: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM lash_trigger_occurrences ORDER BY occurrence_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let occurrences = occurrence_rows.into_iter().map(|record| normalized_trigger_json(serde_json::json!({"record": serde_json::from_str::<serde_json::Value>(&record).unwrap()}), &mut incarnations)).collect();
    let delivery_rows: Vec<(String, String, String, String)> = sqlx::query_as("SELECT occurrence_id, subscription_id, process_id, subscription_snapshot_json FROM lash_trigger_deliveries ORDER BY occurrence_id, subscription_id").fetch_all(pool).await.unwrap();
    let deliveries = delivery_rows.into_iter().map(|(occurrence_id, subscription_id, process_id, snapshot)| normalized_trigger_delivery_json(serde_json::json!({"occurrence_id": occurrence_id, "subscription_id": subscription_id, "process_id": process_id, "subscription_snapshot": serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()}), &mut incarnations)).collect();
    TriggerRows {
        subscriptions,
        mutation_receipts,
        occurrences,
        deliveries,
    }
}

pub(super) fn states_agree(observations: &[(&str, SurfaceState)]) -> bool {
    observations.windows(2).all(|pair| {
        pair[0].1.processes == pair[1].1.processes
            && pair[0].1.wake_redelivery_fences == pair[1].1.wake_redelivery_fences
            && pair[0].1.triggers == pair[1].1.triggers
            && pair[0].1.turn_parks == pair[1].1.turn_parks
            && pair[0].1.turn_park_loads == pair[1].1.turn_park_loads
    })
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
