//! Per-revision settled material released beside revision pruning.
use super::steps::{Step, Value};
use lash_sansio::SessionId;
use sqlx::PgConnection;

/// One page per transaction; GC repeats pages without requiring new commits.
pub(crate) async fn release_material_batch(
    tx: &mut PgConnection,
    session: Option<&SessionId>,
) -> Result<usize, sqlx::Error> {
    super::prompts::release_unretained(tx, session).await
}

/// The owner-commit form of the same release. Callers apply the prune's
/// retention flag to every step, including text reclamation.
pub(super) fn release_material_steps(session: &str) -> Vec<Step> {
    vec![
        Step::any(
            &super::prompts::EXPIRE_AUDITS,
            vec![
                Value::text(session),
                Value::int(i64::from(
                    lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH,
                )),
            ],
        ),
        Step::any(&super::prompts::RECLAIM, Vec::new()),
    ]
}
