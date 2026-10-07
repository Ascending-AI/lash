//! Follow-on and commit-identity helpers the commit and ingress paths share,
//! each running inside the caller's transaction.

use super::*;

/// The follow-on the head of `session_id` owes (ADR 0101 §3), read inside
/// the caller's transaction under a row lock: `FOR UPDATE` for a writer that
/// decides against it, `FOR SHARE` for an admission.
pub(super) async fn pending_follow_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    for_update: bool,
) -> Result<Option<lash_core_execution::store::PendingFollowOn>, StoreError> {
    let statement = if for_update {
        crate::session_sql::session_sql()
            .head_postgres
            .select_pending_follow_on_for_update
            .sql()
    } else {
        crate::session_sql::session_sql()
            .head_postgres
            .select_pending_follow_on_for_share
            .sql()
    };
    let json = sqlx::query_scalar::<_, Option<String>>(statement)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .flatten();
    lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
        session_id,
        json.as_deref(),
    )
}

/// Refuse `fence` unless it is the session's current shift fence, read in
/// the caller's transaction.
pub(super) async fn require_shift_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &lash_core_execution::store::ShiftFence,
) -> Result<(), StoreError> {
    super::shift_epoch::require_fence_tx(tx, fence.session(), fence).await
}

pub(super) fn requested_append_ancestor(
    stamp: &lash_core_execution::RuntimeTurnCommitStamp,
) -> Option<&str> {
    match &stamp.append_request_identity {
        lash_core_execution::AppendRequestIdentity::Append {
            requested_ancestor_node_id,
            ..
        } => requested_ancestor_node_id.as_deref(),
        lash_core_execution::AppendRequestIdentity::PlainCommit
        | lash_core_execution::AppendRequestIdentity::SemanticBoundary { .. } => None,
    }
}

pub(super) type AppendIdentityColumns<'a> = (Option<&'a str>, Option<i64>, Option<i32>);

pub(super) fn append_identity_columns(
    identity: &lash_core_execution::AppendRequestIdentity,
) -> Result<AppendIdentityColumns<'_>, StoreError> {
    // A semantic-boundary identity persists without a node count; the NULL
    // count is what distinguishes its family on decode (FIG-2480).
    let (encoding_version, request_hash, requested_node_count) = match identity {
        lash_core_execution::AppendRequestIdentity::PlainCommit => return Ok((None, None, None)),
        lash_core_execution::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            *encoding_version,
            request_hash.as_str(),
            Some(*requested_node_count as i64),
        ),
        lash_core_execution::AppendRequestIdentity::SemanticBoundary {
            operation: _,
            encoding_version,
            request_hash,
        } => (*encoding_version, request_hash.as_str(), None),
    };
    let encoding_version =
        i32::try_from(encoding_version).map_err(|_| StoreError::RecordEncodingFailed {
            record_kind: "RuntimeCommitReceipt append identity".to_string(),
            message: format!(
                "identity_encoding_version `{}` does not fit PostgreSQL INTEGER",
                encoding_version
            ),
        })?;
    Ok((
        Some(request_hash),
        requested_node_count,
        Some(encoding_version),
    ))
}
