//! Per-revision settled material released beside revision pruning: settled
//! prompt audits, and the bodies of settled inputs and the payloads of
//! applied commands (FIG-6607), and settled host tool-intent payloads
//! (FIG-6612), each one bounded page per release.
use lash_kernel_value::identity::SessionId;
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
/// Settled host tool-intent payloads whose owning run's revision is
/// released.
pub(super) static RELEASE_TOOL_INTENT_PAYLOADS: Entry = Entry::new(
    || &ingress().tool_intents.release_settled_payloads,
    &[Ty::Text, Ty::Int],
    Shape::Exec,
);

fn holds() -> &'static lash_store_sql::attachment::session_holds::SessionHoldStatements {
    &crate::attachments::attachment_sql().holds
}

/// Record a session attachment hold (FIG-6611).
pub(super) static HOLD_ATTACHMENT: Entry = Entry::new(
    || &holds().insert,
    &[Ty::Text, Ty::Key, Ty::Text, Ty::Text],
    Shape::Exec,
);
/// Queue what the inputs and batches whose bodies were released held.
pub(super) static QUEUE_RELEASED_INGRESS: Entry =
    Entry::new(|| &holds().queue_released_ingress, &[Ty::Text], Shape::Exec);
/// End those holds.
pub(super) static END_RELEASED_INGRESS: Entry =
    Entry::new(|| &holds().end_released_ingress, &[Ty::Text], Shape::Exec);
/// Drop the session edges of a page of queued attachments no hold names.
pub(super) static RELEASE_ATTACHMENT_EDGES: Entry =
    Entry::new(|| &holds().release_edges, &[Ty::Text, Ty::Int], Shape::Exec);
/// Dequeue the page the release decided.
pub(super) static DEQUEUE_ATTACHMENT_RELEASES: Entry =
    Entry::new(|| &holds().dequeue, &[Ty::Text, Ty::Int], Shape::Exec);

/// One page per transaction; GC repeats pages without requiring new commits.
pub(crate) async fn release_material_batch(
    tx: &mut PgConnection,
    session: Option<&SessionId>,
) -> Result<usize, sqlx::Error> {
    let mut released = super::prompts::release_unretained(tx, session).await?;
    for statement in [
        &ingress().pending_inputs.release_settled_bodies,
        &ingress().queued_batches.release_settled_payloads,
        &ingress().tool_intents.release_settled_payloads,
    ] {
        let rows = crate::observed_sql::query(statement.sql())
            .bind(session.map(SessionId::as_str))
            .bind(i64::from(MATERIAL_RELEASE_BATCH))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await?
            .rows_affected();
        released += usize::try_from(rows).unwrap_or(usize::MAX);
    }
    // A released body no longer holds its attachments (FIG-6611).
    for statement in [
        &holds().queue_released_ingress,
        &holds().end_released_ingress,
    ] {
        crate::observed_sql::query(statement.sql())
            .bind(session.map(SessionId::as_str))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await?;
    }
    Ok(released)
}

/// The owner-commit form of the same release. Callers apply the prune's
/// retention flag to every step, including text reclamation. The session
/// lets go of each queued attachment whose last hold ended (FIG-6611).
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
        Step::any(&QUEUE_RELEASED_INGRESS, vec![Value::text(session)]),
        Step::any(&END_RELEASED_INGRESS, vec![Value::text(session)]),
        Step::any(&RELEASE_ATTACHMENT_EDGES, page()),
        Step::any(&DEQUEUE_ATTACHMENT_RELEASES, page()),
        Step::any(&RELEASE_TOOL_INTENT_PAYLOADS, page()),
    ]
}
