//! Shared blob/record codecs for the SQLite store: JSON + msgpack envelopes
//! and the compressed artifact-blob encoding.
use super::*;
use lash_sansio::SessionId;

pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: "persisted JSON record".to_string(),
        message: error.to_string(),
    })
}

pub(crate) fn should_compress_blob(
    profile: BuiltinBlobProfile,
    descriptor: &BlobArtifactDescriptor,
    len: usize,
) -> bool {
    if !descriptor.hints.contains(&BlobStorageHint::Compressible) {
        return false;
    }
    match profile {
        BuiltinBlobProfile::LowLatency => false,
        BuiltinBlobProfile::Balanced => len >= 4 * 1024,
        BuiltinBlobProfile::Compact => len >= 1024,
    }
}

pub(crate) fn compress_blob(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, content).map_err(|error| {
        StoreError::RecordEncodingFailed {
            record_kind: "compressed artifact blob".to_string(),
            message: error.to_string(),
        }
    })?;
    encoder
        .finish()
        .map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "compressed artifact blob".to_string(),
            message: error.to_string(),
        })
}

pub(crate) fn decompress_blob(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut decoder = ZlibDecoder::new(content);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out)
        .map_err(|error| stored_data_corrupt("compressed artifact blob", error))?;
    Ok(out)
}

pub(crate) fn encode_artifact_blob(
    descriptor: &BlobArtifactDescriptor,
    profile: BuiltinBlobProfile,
    content: &[u8],
) -> Result<Vec<u8>, StoreError> {
    let (compression, stored_content) = if should_compress_blob(profile, descriptor, content.len())
    {
        (BlobCompression::Zlib, compress_blob(content)?)
    } else {
        (BlobCompression::None, content.to_vec())
    };
    encode_msgpack(
        &StoredBlobEnvelope {
            descriptor: descriptor.clone(),
            compression,
            content: stored_content,
        },
        "SQLite stored blob envelope",
    )
}

pub(crate) fn decode_artifact_blob(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    let envelope = rmp_serde::from_slice::<StoredBlobEnvelope>(bytes)
        .map_err(|error| stored_data_corrupt("artifact blob envelope", error))?;
    match envelope.compression {
        BlobCompression::None => Ok(envelope.content),
        BlobCompression::Zlib => decompress_blob(&envelope.content),
    }
}

/// Synchronous because it runs inside a `conn.call`/`conn.write` closure on the connection
/// thread.
pub(crate) fn try_load_session_head_meta_from_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let row = conn
        .query_row(
            crate::session_sql::session_sql().head.select_meta.sql(),
            params![session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((head_json, head_revision, leaf_node_id, checkpoint_ref)) = row else {
        return Ok(None);
    };
    let payload: SessionHeadPayload = lash_core::store::decode_versioned_json_record(
        &head_json,
        "SessionHeadMeta",
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .map_err(|error| map_record_decode_error("SessionHeadMeta", error))?;
    Ok(Some(SessionHeadMeta::assemble(
        session_id,
        payload,
        u64::try_from(head_revision).map_err(|_| {
            stored_data_corrupt(
                "SessionHeadMeta",
                format!("head_revision must be non-negative, got {head_revision}"),
            )
        })?,
        checkpoint_ref.map(Into::into),
        leaf_node_id.map(lash_core::NodeId::from),
    )?))
}

pub(crate) fn decode_checkpoint(bytes: &[u8]) -> Result<SessionCheckpoint, StoreError> {
    let value: serde_json::Value = rmp_serde::from_slice(bytes)
        .map_err(|err| stored_data_corrupt("SessionCheckpoint", err))?;
    lash_core::store::ensure_supported_record_schema_version(
        "SessionCheckpoint",
        &value,
        lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
    )?;
    rmp_serde::from_slice(bytes).map_err(|err| stored_data_corrupt("SessionCheckpoint", err))
}

pub(crate) fn encode_msgpack<T: serde::Serialize>(
    value: &T,
    record_kind: &str,
) -> Result<Vec<u8>, StoreError> {
    // Pre-size the buffer so the per-byte writes inside rmp_serde don't
    // walk the Vec through 0→4→8→16→32… reallocations on every call.
    let mut buf = Vec::with_capacity(1024);
    rmp_serde::encode::write_named(&mut buf, value).map_err(|error| {
        StoreError::RecordEncodingFailed {
            record_kind: record_kind.to_string(),
            message: error.to_string(),
        }
    })?;
    Ok(buf)
}

pub(crate) fn decode_msgpack<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    rmp_serde::from_slice(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_envelope_stores_hints_without_payload_family() {
        #[derive(serde::Deserialize)]
        struct WireEnvelope {
            descriptor: serde_json::Value,
            compression: BlobCompression,
        }
        let content = vec![b'x'; 8192];
        for (profile, compression) in [
            (BuiltinBlobProfile::LowLatency, BlobCompression::None),
            (BuiltinBlobProfile::Balanced, BlobCompression::Zlib),
            (BuiltinBlobProfile::Compact, BlobCompression::Zlib),
        ] {
            for (descriptor, expected_descriptor, expected_compression) in [
                (
                    BlobArtifactDescriptor::checkpoint_component(),
                    serde_json::json!({"hints": ["Compressible", "LargePayload"]}),
                    compression,
                ),
                (
                    BlobArtifactDescriptor::new(Vec::new()),
                    serde_json::json!({}),
                    BlobCompression::None,
                ),
            ] {
                let encoded = encode_artifact_blob(&descriptor, profile, &content)
                    .expect("encode artifact blob");
                let wire: WireEnvelope =
                    rmp_serde::from_slice(&encoded).expect("inspect named MessagePack envelope");
                assert_eq!(wire.descriptor, expected_descriptor);
                assert_eq!(wire.compression, expected_compression);
                assert_eq!(decode_artifact_blob(&encoded).unwrap(), content);
            }
        }
    }

    #[tokio::test]
    async fn blob_identity_uses_logical_content_across_storage_profiles() {
        let content = vec![b'x'; 8192];
        let expected = BlobRef::for_content(&content);
        for blob_profile in [BuiltinBlobProfile::LowLatency, BuiltinBlobProfile::Compact] {
            let store = Store::memory_with_options(StoreOptions {
                blob_profile,
                ..StoreOptions::default()
            })
            .await
            .expect("open blob store");
            for descriptor in [
                BlobArtifactDescriptor::checkpoint_component(),
                BlobArtifactDescriptor::new(Vec::new()),
            ] {
                let reference = store
                    .put_unrooted_artifact_blob_for_testing(descriptor, &content)
                    .await
                    .expect("store logical payload");
                assert_eq!(reference, expected);
                assert_eq!(
                    store.get_blob(&reference).await.unwrap(),
                    Some(content.clone())
                );
            }
        }
    }
}
