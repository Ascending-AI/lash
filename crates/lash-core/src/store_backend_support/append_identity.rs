use crate::{AppendRequestIdentity, StoreError};

const RECORD_KIND: &str = "RuntimeCommitReceipt append identity";

/// Decode and validate the SQL columns that carry an append-request identity.
///
/// The durable ancestor column remains write-only: the request hash already
/// binds the ancestor, and replay adjudication must happen before the fresh
/// ancestor fence. Consequently SQL receipt reads intentionally supply only
/// the three columns needed for replay and leave the decoded diagnostic
/// ancestor absent.
pub fn decode_append_request_identity(
    request_hash: Option<String>,
    encoding_version: Option<i64>,
    requested_node_count: Option<i64>,
) -> Result<AppendRequestIdentity, StoreError> {
    match (request_hash, encoding_version, requested_node_count) {
        (None, None, None) => Ok(AppendRequestIdentity::PlainCommit),
        (Some(request_hash), Some(encoding_version), Some(requested_node_count)) => {
            let encoding_version = u32::try_from(encoding_version).map_err(|_| {
                corrupt(format!(
                    "identity_encoding_version `{encoding_version}` does not fit u32"
                ))
            })?;
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
        _ => Err(corrupt(
            "append identity columns must be present or absent as a unit".to_string(),
        )),
    }
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
    fn decoder_requires_append_identity_columns_as_a_unit() {
        let mixed_columns = [
            (Some("hash".to_string()), None, None),
            (None, Some(2), None),
            (None, None, Some(1)),
            (Some("hash".to_string()), Some(2), None),
        ];

        for (request_hash, encoding_version, requested_node_count) in mixed_columns {
            let error = decode_append_request_identity(
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
            decode_append_request_identity(None, None, None).expect("plain commit identity"),
            AppendRequestIdentity::PlainCommit
        ));
        assert!(matches!(
            decode_append_request_identity(Some("hash".to_string()), Some(2), Some(3))
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
    fn decoder_refuses_out_of_range_encoding_versions() {
        for version in [-1, i64::from(u32::MAX) + 1] {
            let error =
                decode_append_request_identity(Some("hash".to_string()), Some(version), Some(1))
                    .expect_err("out-of-range version must be refused");
            assert!(matches!(error, StoreError::StoredDataCorrupt { .. }));
        }
    }
}
