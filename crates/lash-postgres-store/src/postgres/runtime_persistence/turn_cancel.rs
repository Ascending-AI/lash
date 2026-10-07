//! Commit-identity helpers the commit and ingress paths share,
//! each running inside the caller's transaction.

use super::*;

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
