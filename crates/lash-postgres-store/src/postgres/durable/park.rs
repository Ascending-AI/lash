//! Current park diagnostics and control redrive, committed under the actor fence.

use lash_durable::domain::{RedriveAnswer, RedriveRequest, SessionParksRedrive};
use lash_durable::{ActorKey, ActorState, DurableError, DurableInstant, Woken};
use sqlx::PgConnection;

use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{SQL, actor_key, sqlx_failure};

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

/// Redrive only the observed actual park, advancing revision and mailbox
/// together with readiness and diagnostic cleanup. A claimed actor refuses.
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
    let Some(park) = park.and_then(|(park, _)| park) else {
        return Ok((RedriveAnswer::NotParked, None));
    };
    let changed = crate::observed_sql::query(SQL.park.redrive_parked.sql())
        .bind(actor)
        .bind(now.0)
        .bind(park)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if changed.is_none() {
        return Ok((RedriveAnswer::NotParked, None));
    }
    Ok((
        RedriveAnswer::Redriven,
        Some(Woken {
            actor: request.actor.clone(),
            state: ActorState::Ready,
            owner: None,
        }),
    ))
}

/// Redrive, for a boot, every session parked with a reason `redrive` names
/// (FIG-6310), each as [`redrive`] would and only while the park read here
/// still stands: the sessions redriven, by key, which also locks them in key
/// order.
pub(super) async fn redrive_session_parks(
    tx: &mut PgConnection,
    redrive: &SessionParksRedrive,
    now: DurableInstant,
) -> Result<(Vec<ActorKey>, Vec<Woken>), DurableError> {
    let parked: Vec<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.park.parked_sessions.sql())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let mut redriven = Vec::new();
    let mut woken = Vec::new();
    for (key, park) in parked {
        let Some(park) = park.filter(|park| redrive.redrives(park)) else {
            continue;
        };
        let readied: Option<(String,)> =
            crate::observed_sql::query_as(SQL.park.redrive_parked.sql())
                .bind(&key)
                .bind(now.0)
                .bind(&park)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
        if readied.is_none() {
            continue;
        }
        let actor = actor_key(&key)?;
        woken.push(Woken {
            actor: actor.clone(),
            state: ActorState::Ready,
            owner: None,
        });
        redriven.push(actor);
    }
    Ok((redriven, woken))
}
