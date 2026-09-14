use crate::store::SemanticBoundaryOperation;
use crate::{AppendRequestIdentity, StoreError};

const RECORD_KIND: &str = "RuntimeCommitReceipt append identity";

/// Decode and validate the SQL columns that carry a runtime-commit receipt
/// identity.
///
/// The columns encode three families. All-NULL is a plain commit. A populated
/// hash and version with a requested-node count is an append identity. A
/// populated hash and version with a NULL count is a semantic-boundary
/// identity (FIG-2480): its typed operation tag is recovered from
/// `operation_key`, the `OperationId::key` of the receipt row, which the
/// backend's exact-match lookup guarantees equals the stored operation.
///
/// The durable ancestor column remains write-only: the request hash already
/// binds the ancestor, and replay adjudication must happen before the fresh
/// ancestor fence. Consequently SQL receipt reads intentionally supply only
/// the columns needed for replay and leave the decoded diagnostic ancestor
/// absent.
pub fn decode_append_request_identity(
    operation_key: &str,
    request_hash: Option<String>,
    encoding_version: Option<i64>,
    requested_node_count: Option<i64>,
) -> Result<AppendRequestIdentity, StoreError> {
    match (request_hash, encoding_version, requested_node_count) {
        (None, None, None) => Ok(AppendRequestIdentity::PlainCommit),
        (Some(request_hash), Some(encoding_version), Some(requested_node_count)) => {
            let encoding_version = decode_encoding_version(encoding_version)?;
            let requested_node_count = u64::try_from(requested_node_count).map_err(|_| {
                corrupt(format!(
                    "requested_node_count `{requested_node_count}` is negative"
                ))
            })?;
            Ok(AppendRequestIdentity::Append {
                encoding_version,
                request_hash,
                requested_node_count,
                requested_ancestor_node_id: None,
            })
        }
        (Some(request_hash), Some(encoding_version), None) => {
            let operation = SemanticBoundaryOperation::from_operation_key(operation_key)
                .ok_or_else(|| {
                    corrupt(format!(
                        "operation `{operation_key}` does not own a semantic-boundary identity family"
                    ))
                })?;
            let encoding_version = decode_encoding_version(encoding_version)?;
            Ok(AppendRequestIdentity::SemanticBoundary {
                operation,
                encoding_version,
                request_hash,
            })
        }
        _ => Err(corrupt(
            "append identity columns must be present or absent as a unit".to_string(),
        )),
    }
}

fn decode_encoding_version(encoding_version: i64) -> Result<u32, StoreError> {
    u32::try_from(encoding_version).map_err(|_| {
        corrupt(format!(
            "identity_encoding_version `{encoding_version}` does not fit u32"
        ))
    })
}

fn corrupt(message: String) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: RECORD_KIND,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_requires_identity_columns_as_a_unit() {
        let mixed_columns = [
            (Some("hash".to_string()), None, None),
            (None, Some(2), None),
            (None, None, Some(1)),
            (None, Some(2), Some(1)),
        ];

        for (request_hash, encoding_version, requested_node_count) in mixed_columns {
            let error = decode_append_request_identity(
                "append-session-nodes",
                request_hash,
                encoding_version,
                requested_node_count,
            )
            .expect_err("mixed append identity columns must be refused");
            assert!(matches!(
                error,
                StoreError::StoredDataCorrupt {
                    record_kind: RECORD_KIND,
                    ref message,
                } if message == "append identity columns must be present or absent as a unit"
            ));
        }
    }

    #[test]
    fn decoder_accepts_plain_and_complete_append_identities() {
        assert!(matches!(
            decode_append_request_identity("append-session-nodes", None, None, None)
                .expect("plain commit identity"),
            AppendRequestIdentity::PlainCommit
        ));
        assert!(matches!(
            decode_append_request_identity(
                "append-session-nodes",
                Some("hash".to_string()),
                Some(2),
                Some(3)
            )
            .expect("complete append identity"),
            AppendRequestIdentity::Append {
                encoding_version: 2,
                ref request_hash,
                requested_node_count: 3,
                requested_ancestor_node_id: None,
            } if request_hash == "hash"
        ));
    }

    #[test]
    fn decoder_recovers_semantic_boundary_identities_by_operation_family() {
        for (key, operation) in [
            ("record-config", SemanticBoundaryOperation::RecordConfig),
            ("create-session", SemanticBoundaryOperation::CreateSession),
            ("usage-ledger", SemanticBoundaryOperation::UsageLedger),
        ] {
            assert!(matches!(
                decode_append_request_identity(key, Some("hash".to_string()), Some(1), None)
                    .expect("semantic-boundary identity"),
                AppendRequestIdentity::SemanticBoundary {
                    operation: decoded,
                    encoding_version: 1,
                    ref request_hash,
                } if decoded == operation && request_hash == "hash"
            ));
        }
    }

    #[test]
    fn decoder_refuses_a_countless_identity_for_a_foreign_operation() {
        for key in ["append-session-nodes", "initial-park", "commit"] {
            let error =
                decode_append_request_identity(key, Some("hash".to_string()), Some(1), None)
                    .expect_err("a foreign operation must not decode a semantic-boundary family");
            assert!(matches!(
                error,
                StoreError::StoredDataCorrupt { ref message, .. }
                    if message == &format!(
                        "operation `{key}` does not own a semantic-boundary identity family"
                    )
            ));
        }
    }

    #[test]
    fn decoder_refuses_out_of_range_encoding_versions() {
        for version in [-1, i64::from(u32::MAX) + 1] {
            for requested_node_count in [Some(1), None] {
                let error = decode_append_request_identity(
                    if requested_node_count.is_some() {
                        "append-session-nodes"
                    } else {
                        "record-config"
                    },
                    Some("hash".to_string()),
                    Some(version),
                    requested_node_count,
                )
                .expect_err("out-of-range version must be refused");
                assert!(matches!(error, StoreError::StoredDataCorrupt { .. }));
            }
        }
    }
}
