//! Shared blob/record codecs for the SQLite store: JSON + msgpack envelopes
//! and the compressed artifact-blob encoding.
use super::*;
use lash_protocol_types::SessionId;

/// A content address bound at the SQLite boundary. Domain values stay hex;
/// SQLite keys are the digest's 32 bytes, ordered identically to canonical hex.
pub(crate) struct SqlHash<'a>(pub &'a str);

impl rusqlite::ToSql for SqlHash<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let node =
            lash_core_store::checkpoint_graph::NodeId::from_hex(self.0).ok_or_else(|| {
                sqlite_conversion_error(stored_data_corrupt("content address", self.0))
            })?;
        Ok(rusqlite::types::ToSqlOutput::Owned(
            rusqlite::types::Value::Blob(node.0.to_vec()),
        ))
    }
}

/// Decode only the binary baseline; a text key is corrupt stored data.
pub(crate) fn hash_column(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<String> {
    let bytes: Vec<u8> = row.get(column)?;
    let digest: [u8; 32] = bytes.try_into().map_err(|_| {
        sqlite_conversion_error(stored_data_corrupt("content address", "expected 32 bytes"))
    })?;
    Ok(lash_core_store::checkpoint_graph::NodeId(digest).to_hex())
}

pub(crate) fn optional_hash_column(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<String>> {
    if matches!(row.get_ref(column)?, rusqlite::types::ValueRef::Null) {
        Ok(None)
    } else {
        hash_column(row, column).map(Some)
    }
}

/// Only committed reads may publish decoded material. A write transaction
/// can reuse a committed entry but must keep its misses private, including
/// inside a group-commit member that may roll back independently.
#[derive(Clone, Copy)]
pub(crate) enum ConfigRead<'a> {
    Committed(&'a lash_core_store::store_backend_support::ConfigCache),
    Uncommitted(&'a lash_core_store::store_backend_support::ConfigCache),
}

impl<'a> ConfigRead<'a> {
    fn cache(self) -> &'a lash_core_store::store_backend_support::ConfigCache {
        match self {
            Self::Committed(cache) | Self::Uncommitted(cache) => cache,
        }
    }
}

/// The stored SQLite blob envelope's format under the 1.0 freeze.
///
/// version_guard(
///     roots(
///         path = "crates/lash-sqlite-store/src/lib.rs", StoredBlobEnvelope, BlobCompression,
///         BlobStorageHint, BlobArtifactDescriptor,
///     ),
///     items(
///         blob_envelope_admits, should_compress_blob, compress_blob, decompress_blob,
///         encode_artifact_blob, decode_artifact_blob, encode_msgpack,
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_outside_manifest = "backend-local blob encoding: SQLite admits it when reading the blob"
pub const SQLITE_BLOB_ENVELOPE_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the envelope to 2. Its shape
/// is 1's; while `F` is N's epoch the fleet pins writers to 1, so every blob
/// N+1 writes before finalize is one N reads after a rollback.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_outside_manifest = "backend-local blob encoding: SQLite admits it when reading the blob"
pub const SQLITE_BLOB_ENVELOPE_VERSION: u32 = 2;

/// Whether this build encodes and decodes envelope `version`: the envelope
/// surface's supported range, its newest and every older version a
/// `Lift::Decoder` row admits (FIG-3802). The envelope keeps its shape across
/// them, so each decodes natively.
fn blob_envelope_admits(version: u32) -> bool {
    lash_core_store::store::upcast_chain_covers(
        lash_core_store::surface_format!(SQLITE_BLOB_ENVELOPE_VERSION),
        version,
        SQLITE_BLOB_ENVELOPE_VERSION,
    )
}

/// Read a stored identity: a column this store only ever wrote from a typed
/// id, so blank text is corrupt stored data. The refusal travels typed, as the
/// store error [`sqlite_error`](crate::sqlite_error) unwraps.
pub(crate) fn sql_identity<T>(value: String) -> rusqlite::Result<T>
where
    T: std::str::FromStr<Err = lash_protocol_types::BlankIdentity>,
{
    value
        .parse()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(StoreError::from(error))))
}

/// Read a stored process id: a column this store only ever wrote from a
/// minted id, so any other spelling is corrupt stored data.
pub(crate) fn sql_process_id(
    column: usize,
    value: String,
) -> rusqlite::Result<lash_protocol_types::ProcessId> {
    lash_protocol_types::ProcessId::parse(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

/// [`sql_process_id`] for column `column` of `row`.
pub(crate) fn row_process_id(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<lash_protocol_types::ProcessId> {
    sql_process_id(column, row.get(column)?)
}

/// Parse a stored process id outside a row read, as a store error.
pub(crate) fn stored_process_id(
    value: &str,
) -> Result<lash_protocol_types::ProcessId, lash_execution_contracts::plugin_error::PluginError> {
    lash_protocol_types::ProcessId::parse(value).map_err(|error| {
        lash_execution_contracts::plugin_error::PluginError::Session(format!(
            "corrupt stored process id: {error}"
        ))
    })
}

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
    version: u32,
) -> Result<Vec<u8>, StoreError> {
    if !blob_envelope_admits(version) {
        return Err(StoreError::RecordEncodingFailed {
            record_kind: "SQLite stored blob envelope".to_string(),
            message: format!("no encoder for fleet-selected version {version}"),
        });
    }
    let (compression, stored_content) = if should_compress_blob(profile, descriptor, content.len())
    {
        (BlobCompression::Zlib, compress_blob(content)?)
    } else {
        (BlobCompression::None, content.to_vec())
    };
    encode_msgpack(
        &StoredBlobEnvelope {
            version,
            compression: match compression {
                BlobCompression::None => "None",
                BlobCompression::Zlib => "Zlib",
            }
            .to_string(),
            content: stored_content,
        },
        "SQLite stored blob envelope",
    )
}

pub(crate) fn decode_artifact_blob(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    let envelope = rmp_serde::from_slice::<StoredBlobEnvelope>(bytes)
        .map_err(|error| stored_data_corrupt("artifact blob envelope", error))?;
    if !blob_envelope_admits(envelope.version) {
        return Err(StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "SQLite stored blob envelope",
            actual: envelope.version,
            expected: SQLITE_BLOB_ENVELOPE_VERSION,
        });
    }
    match envelope.compression.as_str() {
        "None" => Ok(envelope.content),
        "Zlib" => decompress_blob(&envelope.content),
        unknown => Err(StoreError::Incompatible {
            refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary {
                surface: "SQLite blob compression".to_string(),
                label: unknown.to_string(),
            },
        }),
    }
}

/// Synchronous because it runs inside a `conn.read`/`conn.write` closure on the connection
/// thread.
///
/// `fleet` is the store's recorded `F`: the head-meta JSON admits the
/// `[N-1, N]` reader window `F` names while a finalize is pending
/// (FIG-3796).
pub(crate) fn try_load_session_head_meta_from_conn(
    conn: &Connection,
    session_id: &SessionId,
    fleet: lash_core_store::store::FleetFormat,
    configs: Option<ConfigRead<'_>>,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    select_session_head_row(conn, session_id)?
        .map(|row| row.hydrate(conn, session_id, fleet, configs))
        .transpose()
}

/// The head row of a session: its revision and the references its config,
/// leaf, checkpoint and frame are hydrated from in the same snapshot.
pub(crate) struct SessionHeadRow {
    head_json: Option<String>,
    head_revision: i64,
    leaf_node_id: Option<String>,
    checkpoint_ref: Option<String>,
    current_frame_node_id: Option<String>,
}

pub(crate) fn select_session_head_row(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionHeadRow>, StoreError> {
    conn.prepare_cached(crate::session_sql::session_sql().head.select_meta.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str()], SessionHeadRow::from_row)
        })
        .optional()
        .map_err(sqlite_error)
}

impl SessionHeadRow {
    /// The immutable config identity this row's hydration validates in the
    /// caller's snapshot. Reuse it only after hydration succeeds.
    pub(crate) fn config_ref(&self) -> Option<&str> {
        self.head_json.as_deref()
    }

    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            head_json: optional_hash_column(row, 0)?,
            head_revision: row.get(1)?,
            leaf_node_id: row.get(2)?,
            checkpoint_ref: optional_hash_column(row, 3)?,
            current_frame_node_id: row.get(4)?,
        })
    }

    /// Hydrate the row's head metadata; `conn` must be in the snapshot the
    /// row was selected in.
    pub(crate) fn hydrate(
        self,
        conn: &Connection,
        session_id: &SessionId,
        fleet: lash_core_store::store::FleetFormat,
        configs: Option<ConfigRead<'_>>,
    ) -> Result<SessionHeadMeta, StoreError> {
        assemble_session_head_meta(
            session_id,
            || load_config_conn(conn, self.head_json.as_deref(), fleet, configs),
            self.head_revision,
            self.leaf_node_id,
            self.checkpoint_ref,
            self.current_frame_node_id,
        )
    }
}

/// Assemble the head metadata a revision row names: its head document, its
/// revision, its leaf and checkpoint, and the frame its leaf row names. The
/// live head and a terminal's recorded window (FIG-5931) both read a head
/// this way. Configuration loading is deferred until the revision counter
/// is validated so a corrupt counter refuses before its graph is read.
pub(crate) fn assemble_session_head_meta(
    session_id: &SessionId,
    load_payload: impl FnOnce() -> Result<SessionHeadPayload, StoreError>,
    head_revision: i64,
    leaf_node_id: Option<String>,
    checkpoint_ref: Option<String>,
    current_frame_node_id: Option<String>,
) -> Result<SessionHeadMeta, StoreError> {
    let head_revision = u64::try_from(head_revision).map_err(|_| {
        stored_data_corrupt(
            "SessionHeadMeta",
            format!("head_revision must be non-negative, got {head_revision}"),
        )
    })?;
    let payload = load_payload()?;
    if let Some(leaf) = &leaf_node_id
        && current_frame_node_id.is_none()
    {
        return Err(StoreError::MissingFrameOpenAncestor {
            leaf_node_id: leaf.clone().try_into()?,
        });
    }
    let current_frame_node_id = current_frame_node_id
        .map(lash_core_store::session_identity::FrameNodeId::new)
        .transpose()
        .map_err(|error| stored_data_corrupt("SessionGraph", error))?;
    SessionHeadMeta::assemble(
        session_id,
        payload,
        head_revision,
        checkpoint_ref.map(Into::into),
        leaf_node_id
            .map(lash_protocol_types::NodeId::parse)
            .transpose()?,
        current_frame_node_id,
    )
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

/// Read a revision's canonical configuration graph in the caller's snapshot.
pub(crate) fn load_config_conn(
    conn: &Connection,
    reference: Option<&str>,
    fleet: lash_core_store::store::FleetFormat,
    configs: Option<ConfigRead<'_>>,
) -> Result<SessionHeadPayload, StoreError> {
    use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder, NodeId, config};
    let corrupt = |message: String| stored_data_corrupt("SessionHeadMeta", message);
    let root = reference
        .and_then(NodeId::from_hex)
        .ok_or_else(|| corrupt("revision names a missing config reference".to_owned()))?;
    if let Some(configs) = configs
        // A cache populated by a newer reader cannot lend a root to an
        // older snapshot. The visible immutable root's edges retain its
        // closure; this probe reads no body and grants no retention lease.
        && (conn.is_autocommit()
            || crate::durable::config_root_visible(conn, root).map_err(sqlite_error)?)
        && let Some(payload) = configs.cache().get(root, fleet)?
    {
        return Ok(payload);
    }
    let limits = DecodeLimits::within_total(1 << 32);
    let graph = crate::durable::graph_closure(conn, root, &limits)
        .map_err(sqlite_error)?
        .map_err(|error| corrupt(error.to_string()))?;
    let value: serde_json::Value =
        config::load(&mut GraphDecoder::new(&graph, limits), root).map_err(corrupt)?;
    match configs {
        Some(ConfigRead::Committed(configs)) => configs.decode(root, value, fleet),
        Some(ConfigRead::Uncommitted(_)) | None => {
            SessionHeadPayload::decode_value_for_fleet(value, fleet)
        }
    }
}

#[cfg(test)]
mod guarded_surface_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_envelope_carries_no_descriptor_field() {
        let content = vec![b'x'; 8192];
        for (profile, compression) in [
            (BuiltinBlobProfile::LowLatency, BlobCompression::None),
            (BuiltinBlobProfile::Balanced, BlobCompression::Zlib),
            (BuiltinBlobProfile::Compact, BlobCompression::Zlib),
        ] {
            for (descriptor, expected_compression) in [
                (BlobArtifactDescriptor::checkpoint_component(), compression),
                (
                    BlobArtifactDescriptor::new(Vec::new()),
                    BlobCompression::None,
                ),
            ] {
                let encoded = encode_artifact_blob(
                    &descriptor,
                    profile,
                    &content,
                    SQLITE_BLOB_ENVELOPE_VERSION,
                )
                .expect("encode artifact blob");
                let wire: std::collections::BTreeMap<String, serde::de::IgnoredAny> =
                    rmp_serde::from_slice(&encoded).expect("inspect named MessagePack envelope");
                assert_eq!(
                    wire.keys().collect::<Vec<_>>(),
                    ["compression", "content", "version"],
                    "the wire envelope carries no descriptor field"
                );
                let envelope: StoredBlobEnvelope =
                    rmp_serde::from_slice(&encoded).expect("decode stored blob envelope");
                assert_eq!(envelope.version, SQLITE_BLOB_ENVELOPE_VERSION);
                assert_eq!(
                    envelope.compression,
                    match expected_compression {
                        BlobCompression::None => "None",
                        BlobCompression::Zlib => "Zlib",
                    }
                );
                assert_eq!(decode_artifact_blob(&encoded).unwrap(), content);
            }
        }
    }
}
