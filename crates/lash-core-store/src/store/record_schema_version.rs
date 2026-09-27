//! The `schema_version` every persisted record carries, and the refusal of a
//! version this binary does not read.

use super::StoreError;

/// Reject a persisted record whose `schema_version` does not match the
/// version this binary supports. Backends call this immediately after
/// deserializing a record from durable storage.
pub fn ensure_supported_schema_version(
    record_kind: &'static str,
    actual: u32,
    expected: u32,
) -> Result<(), StoreError> {
    if actual == expected {
        Ok(())
    } else {
        Err(StoreError::UnsupportedRecordSchemaVersion {
            record_kind,
            actual,
            expected,
        })
    }
}

pub fn ensure_supported_record_schema_version(
    record_kind: &'static str,
    value: &serde_json::Value,
    expected: u32,
) -> Result<(), StoreError> {
    let actual = record_schema_version(record_kind, value, expected)?;
    ensure_supported_schema_version(record_kind, actual, expected)
}

/// The `schema_version` a persisted record carries — the read half of
/// [`ensure_supported_record_schema_version`], split out so the fleet's read
/// window can admit the extracted version instead of a bare `expected`.
pub(super) fn record_schema_version(
    record_kind: &'static str,
    value: &serde_json::Value,
    expected: u32,
) -> Result<u32, StoreError> {
    let Some(schema_version) = value.get("schema_version") else {
        // A persisted record that did not decode to an object at all is
        // corruption, not a version refusal: there is no record here whose
        // version could be missing. Backends whose blobs carry no framing of
        // their own (PostgreSQL stores the checkpoint manifest as bare
        // MessagePack) otherwise report arbitrary corrupt bytes that happen to
        // form a valid scalar -- a lone `0x00` decodes as the integer `0` --
        // as `MissingRecordSchemaVersion`, while a framed backend reports
        // `StoredDataCorrupt` for the same bytes (FIG-2841).
        if !value.is_object() {
            return Err(StoreError::StoredDataCorrupt {
                record_kind,
                message: format!("persisted {record_kind} record did not decode to an object"),
            });
        }
        return Err(StoreError::MissingRecordSchemaVersion {
            record_kind,
            expected,
        });
    };
    schema_version
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| StoreError::InvalidRecordSchemaVersion {
            record_kind,
            actual: schema_version.to_string(),
            expected,
        })
}
