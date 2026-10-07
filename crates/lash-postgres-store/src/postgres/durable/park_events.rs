//! The operator's park feed on PostgreSQL: the `park_events` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::domain::{
    ParkEventKind, ParkEventRow, ParkEventSeq, ParkEventWrite, RedriveAnswer, RedriveRequest,
};
use lash_durable::{DurableError, DurableInstant, Woken};
use sqlx::PgConnection;

use super::{Committing, SQL, actor_key, corrupt, get, sqlx_failure, wake_within};

async fn append(
    tx: &mut PgConnection,
    actor: &str,
    kind: ParkEventKind,
    reason_json: &str,
    at: DurableInstant,
) -> Result<(), DurableError> {
    sqlx::query(SQL.park_events.append.sql())
        .bind(actor)
        .bind(kind.as_str())
        .bind(reason_json)
        .bind(at.0)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ParkEventWrite,
) -> Result<(), DurableError> {
    let actor = commit.actor.as_str();
    match write {
        ParkEventWrite::Park { reason_json } => {
            sqlx::query(SQL.park.set_park.sql())
                .bind(actor)
                .bind(reason_json)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            append(tx, actor, ParkEventKind::Parked, reason_json, commit.now).await
        }
        ParkEventWrite::Ended { reason_json } => {
            let cleared = sqlx::query(SQL.park.clear_park.sql())
                .bind(actor)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            if cleared.is_some() {
                append(tx, actor, ParkEventKind::Ended, reason_json, commit.now).await?;
            }
            Ok(())
        }
    }
}

/// Redrive a parked actor: control-wake it, then clear its park and its
/// failed activations and record the redrive. A ready actor needs no park,
/// so the wake goes first.
pub(super) async fn redrive(
    tx: &mut PgConnection,
    request: &RedriveRequest,
    now: DurableInstant,
) -> Result<(RedriveAnswer, Option<Woken>), DurableError> {
    let actor = request.actor.as_str();
    let park: Option<(Option<String>, i64)> = sqlx::query_as(SQL.park.park_of.sql())
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    if park.and_then(|(park, _)| park).is_none() {
        return Ok((RedriveAnswer::NotParked, None));
    }
    let (woken, _) = wake_within(tx, &request.actor, true, now).await?;
    sqlx::query(SQL.park.clear_park.sql())
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let reason = serde_json::json!({ "requester": request.requester }).to_string();
    append(tx, actor, ParkEventKind::Redriven, &reason, now).await?;
    Ok((RedriveAnswer::Redriven, Some(woken)))
}

pub(super) async fn read(
    tx: &mut PgConnection,
    after: Option<ParkEventSeq>,
    limit: usize,
) -> Result<Vec<ParkEventRow>, DurableError> {
    let rows = sqlx::query(SQL.park_events.page.sql())
        .bind(after.map_or(0, |seq| seq.0))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    rows.iter()
        .map(|row| {
            let kind: String = get(row, 2)?;
            Ok(ParkEventRow {
                seq: ParkEventSeq(get(row, 0)?),
                actor: actor_key(&get::<String>(row, 1)?)?,
                kind: ParkEventKind::parse(&kind)
                    .ok_or_else(|| corrupt("park event kind", &kind))?,
                reason_json: get(row, 3)?,
                at: DurableInstant(get(row, 4)?),
            })
        })
        .collect()
}
