//! App installation and the Events API delivery outbox.
//!
//! Kept apart from `db.rs` because it is the platform's *outbound* half: the
//! part that behaves like Slack towards the bot rather than like a chat product
//! towards humans.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _, params};

use super::db::{AppRow, OutboxRow};
use crate::ids::Ts;

/// Retries after the first attempt, matching Slack's three.
pub const MAX_RETRIES: u32 = 3;

/// Install the app if absent and return it.
pub fn ensure_app(
    connection: &Connection,
    app_id: &str,
    bot_id: &str,
    bot_user_id: &str,
) -> Result<AppRow> {
    connection.execute(
        "INSERT OR IGNORE INTO apps (id, bot_id, bot_user_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![app_id, bot_id, bot_user_id, Ts::now().epoch_seconds()],
    )?;
    installed_app(connection)?.ok_or_else(|| anyhow::anyhow!("app vanished after install"))
}

/// The single installed app, if any.
pub fn installed_app(connection: &Connection) -> Result<Option<AppRow>> {
    Ok(connection
        .query_row(
            "SELECT id, bot_id, bot_user_id, request_url, verified_at FROM apps LIMIT 1",
            [],
            |row| {
                Ok(AppRow {
                    id: row.get(0)?,
                    bot_id: row.get(1)?,
                    bot_user_id: row.get(2)?,
                    request_url: row.get(3)?,
                    verified_at: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Record a verified Events API request URL.
///
/// Real Slack stores this from the app-configuration UI after its own
/// `url_verification` handshake succeeds; the platform performs the same
/// handshake from `POST /platform/apps` and only then writes the row.
pub fn set_request_url(connection: &Connection, app_id: &str, request_url: &str) -> Result<()> {
    connection.execute(
        "UPDATE apps SET request_url = ?2, verified_at = ?3 WHERE id = ?1",
        params![app_id, request_url, Ts::now().epoch_seconds()],
    )?;
    Ok(())
}

/// Queue an event for delivery.
///
/// `event_id` is unique, so a caller that retries the enqueue itself cannot
/// double-deliver: the second insert is ignored and the existing row keeps its
/// attempt history.
pub fn enqueue_event(
    connection: &Connection,
    app_id: &str,
    event_id: &str,
    payload_json: &str,
) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO event_outbox (app_id, event_id, payload_json, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![app_id, event_id, payload_json, now_millis()],
    )?;
    Ok(())
}

/// Read up to `limit` events whose next attempt is due.
///
/// **This reads; it does not claim.** The statement is a plain `SELECT` — no row
/// is marked, leased or reserved, so two readers would both see the same event
/// and both deliver it.
///
/// What makes that safe here is not this function but its caller: exactly one
/// dispatcher loop runs per platform process, and it awaits each pass before
/// starting the next, so a row stays in the ready set only until that same loop
/// records the attempt. Two dispatchers *would* double-deliver — which the bot's
/// `event_id` ledger tolerates by design, since Slack redelivers anyway — but the
/// retry counters would drift and the backoff schedule would stop meaning
/// anything. A multi-process platform needs a real lease (claim token, owner id,
/// expiry) exactly as `lash`'s own stores use; that is out of scope for an
/// example and is listed in the README rather than faked.
pub fn due_events(connection: &Connection, limit: usize) -> Result<Vec<OutboxRow>> {
    let mut statement = connection.prepare(
        "SELECT outbox.id, outbox.event_id, outbox.payload_json, outbox.attempts,
                outbox.last_reason, apps.request_url
         FROM event_outbox AS outbox
         JOIN apps ON apps.id = outbox.app_id
         WHERE outbox.delivered_at IS NULL
           AND outbox.abandoned_at IS NULL
           AND outbox.next_attempt_at <= ?1
           AND apps.request_url IS NOT NULL
         ORDER BY outbox.id
         LIMIT ?2",
    )?;
    let rows = statement
        .query_map(params![now_millis(), limit as i64], |row| {
            Ok(OutboxRow {
                id: row.get(0)?,
                event_id: row.get(1)?,
                payload_json: row.get(2)?,
                attempts: row.get(3)?,
                last_reason: row.get(4)?,
                request_url: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Mark an event delivered.
pub fn mark_delivered(connection: &Connection, id: i64) -> Result<()> {
    connection.execute(
        "UPDATE event_outbox
         SET delivered_at = ?2, attempts = attempts + 1, last_error = NULL
         WHERE id = ?1",
        params![id, now_millis()],
    )?;
    Ok(())
}

/// Record a failed attempt, scheduling the next one or abandoning the event.
///
/// Returns the new attempt count from the `UPDATE` itself, so the value cannot
/// disagree with what was written.
pub fn mark_failed(
    connection: &Connection,
    id: i64,
    error: &str,
    reason: &str,
    backoff_millis: i64,
) -> Result<u32> {
    Ok(connection.query_row(
        "UPDATE event_outbox
         SET attempts = attempts + 1,
             last_error = ?2,
             last_reason = ?3,
             next_attempt_at = ?4,
             abandoned_at = CASE WHEN attempts + 1 > ?5 THEN ?6 ELSE NULL END
         WHERE id = ?1
         RETURNING attempts",
        params![
            id,
            error,
            reason,
            now_millis() + backoff_millis,
            MAX_RETRIES,
            now_millis(),
        ],
        |row| row.get(0),
    )?)
}

/// Delivery counters, surfaced on `/healthz` so a runbook or smoke test can see
/// whether the platform is actually reaching the bot.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DeliveryStats {
    pub queued: u32,
    pub delivered: u32,
    pub abandoned: u32,
}

/// Read the delivery counters.
pub fn delivery_stats(connection: &Connection) -> Result<DeliveryStats> {
    Ok(connection.query_row(
        "SELECT
            COALESCE(SUM(delivered_at IS NULL AND abandoned_at IS NULL), 0),
            COALESCE(SUM(delivered_at IS NOT NULL), 0),
            COALESCE(SUM(abandoned_at IS NOT NULL), 0)
         FROM event_outbox",
        [],
        |row| {
            Ok(DeliveryStats {
                queued: row.get(0)?,
                delivered: row.get(1)?,
                abandoned: row.get(2)?,
            })
        },
    )?)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
