//! Current park diagnostics and control redrive, committed under the actor fence.

use lash_durable::domain::{RedriveAnswer, RedriveRequest};
use lash_durable::{DurableError, DurableInstant, Woken};
use sqlx::PgConnection;

use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{SQL, sqlx_failure, wake_within};

/// The park's statement an owner commit runs as a step.
pub(super) static SET_PARK: Entry =
    Entry::new(|| &SQL.park.set_park, &[Ty::Text, Ty::Text], Shape::Exec);

/// Actor `actor`'s park, set to `reason` (cleared when `None`), as a step.
pub(super) fn step(actor: &str, reason: &Option<String>) -> Step {
    Step::any(
        &SET_PARK,
        vec![Value::text(actor), Value::Text(reason.clone())],
    )
}

pub(super) async fn set(
    tx: &mut PgConnection,
    actor: &str,
    reason: &Option<String>,
) -> Result<(), DurableError> {
    crate::observed_sql::query(SQL.park.set_park.sql())
        .bind(actor)
        .bind(reason)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

/// Redrive a parked actor: control-wake it, then clear its park and its
/// failed activations. A ready actor needs no park,
/// so the wake goes first.
pub(super) async fn redrive(
    tx: &mut PgConnection,
    request: &RedriveRequest,
    now: DurableInstant,
) -> Result<(RedriveAnswer, Option<Woken>), DurableError> {
    let actor = request.actor.as_str();
    let park: Option<(Option<String>, i64)> = crate::observed_sql::query_as(SQL.park.park_of.sql())
        .bind(actor)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if park.and_then(|(park, _)| park).is_none() {
        return Ok((RedriveAnswer::NotParked, None));
    }
    let (woken, _) = wake_within(tx, &request.actor, true, now).await?;
    crate::observed_sql::query(SQL.park.clear_park.sql())
        .bind(actor)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok((RedriveAnswer::Redriven, Some(woken)))
}
