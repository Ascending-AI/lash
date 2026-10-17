//! Per-revision settled material released beside revision pruning: settled
//! prompt audits, and the bodies of settled inputs and the payloads of
//! applied commands (FIG-6607), each one bounded page per release.
use lash_sansio::SessionId;
use lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH;
use sqlx::PgConnection;

use super::steps::{Entry, Shape, Step, Ty, Value};

fn ingress() -> &'static crate::turn_ingress::TurnIngressSql {
    crate::turn_ingress::turn_ingress_sql()
}

/// The bodies of settled inputs whose run's revision is released.
pub(super) static RELEASE_INPUT_BODIES: Entry = Entry::new(
    || &ingress().pending_inputs.release_settled_bodies,
    &[Ty::Text, Ty::Int],
    Shape::Exec,
);
/// The payloads of applied commands whose applying revision is released.
pub(super) static RELEASE_COMMAND_PAYLOADS: Entry = Entry::new(
    || &ingress().queued_batches.release_settled_payloads,
    &[Ty::Text, Ty::Int],
    Shape::Exec,
);

/// One page per transaction; GC repeats pages without requiring new commits.
pub(crate) async fn release_material_batch(
    tx: &mut PgConnection,
    session: Option<&SessionId>,
) -> Result<usize, sqlx::Error> {
    let mut released = super::prompts::release_unretained(tx, session).await?;
    for statement in [
        &ingress().pending_inputs.release_settled_bodies,
        &ingress().queued_batches.release_settled_payloads,
    ] {
        let rows = crate::observed_sql::query(statement.sql())
            .bind(session.map(SessionId::as_str))
            .bind(i64::from(MATERIAL_RELEASE_BATCH))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await?
            .rows_affected();
        released += usize::try_from(rows).unwrap_or(usize::MAX);
    }
    Ok(released)
}

/// The owner-commit form of the same release. Callers apply the prune's
/// retention flag to every step, including text reclamation.
pub(super) fn release_material_steps(session: &str) -> Vec<Step> {
    let page = || {
        vec![
            Value::text(session),
            Value::int(i64::from(MATERIAL_RELEASE_BATCH)),
        ]
    };
    vec![
        Step::any(&super::prompts::EXPIRE_AUDITS, page()),
        Step::any(&super::prompts::RECLAIM, Vec::new()),
        Step::any(&RELEASE_INPUT_BODIES, page()),
        Step::any(&RELEASE_COMMAND_PAYLOADS, page()),
    ]
}
