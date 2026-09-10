use std::fmt;

use serde::Deserialize;

use super::model::{ProcessLease, ensure_process_lease_schema_version};

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ProcessLeaseField {
    SchemaVersion,
    ProcessId,
    Owner,
    LeaseToken,
    FencingToken,
    ClaimedAtEpochMs,
    ExpiresAtEpochMs,
    #[serde(other)]
    Ignore,
}

struct ProcessLeaseVisitor;

enum BufferedProcessLeaseField<T> {
    Decoded(T),
    Value(serde_json::Value),
}

impl<'de> serde::de::Visitor<'de> for ProcessLeaseVisitor {
    type Value = ProcessLease;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a versioned process lease")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let schema_version = sequence
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        ensure_process_lease_schema_version(schema_version).map_err(serde::de::Error::custom)?;

        Ok(ProcessLease {
            schema_version,
            process_id: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?,
            owner: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(2, &self))?,
            lease_token: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(3, &self))?,
            fencing_token: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(4, &self))?,
            claimed_at_epoch_ms: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(5, &self))?,
            expires_at_epoch_ms: sequence
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(6, &self))?,
        })
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut schema_version = None;
        let mut process_id = None;
        let mut owner = None;
        let mut lease_token = None;
        let mut fencing_token = None;
        let mut claimed_at_epoch_ms = None;
        let mut expires_at_epoch_ms = None;
        let mut duplicate_field = None;

        while let Some(field) = map.next_key()? {
            match field {
                ProcessLeaseField::SchemaVersion => {
                    if schema_version.is_some() {
                        return Err(serde::de::Error::duplicate_field("schema_version"));
                    }
                    let actual = map.next_value()?;
                    ensure_process_lease_schema_version(actual)
                        .map_err(serde::de::Error::custom)?;
                    schema_version = Some(actual);
                }
                ProcessLeaseField::ProcessId => buffer_process_lease_field(
                    &mut map,
                    &mut process_id,
                    &mut duplicate_field,
                    "process_id",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::Owner => buffer_process_lease_field(
                    &mut map,
                    &mut owner,
                    &mut duplicate_field,
                    "owner",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::LeaseToken => buffer_process_lease_field(
                    &mut map,
                    &mut lease_token,
                    &mut duplicate_field,
                    "lease_token",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::FencingToken => buffer_process_lease_field(
                    &mut map,
                    &mut fencing_token,
                    &mut duplicate_field,
                    "fencing_token",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::ClaimedAtEpochMs => buffer_process_lease_field(
                    &mut map,
                    &mut claimed_at_epoch_ms,
                    &mut duplicate_field,
                    "claimed_at_epoch_ms",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::ExpiresAtEpochMs => buffer_process_lease_field(
                    &mut map,
                    &mut expires_at_epoch_ms,
                    &mut duplicate_field,
                    "expires_at_epoch_ms",
                    schema_version.is_some(),
                )?,
                ProcessLeaseField::Ignore => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        let schema_version =
            schema_version.ok_or_else(|| serde::de::Error::missing_field("schema_version"))?;
        if let Some(field) = duplicate_field {
            return Err(serde::de::Error::duplicate_field(field));
        }

        Ok(ProcessLease {
            schema_version,
            process_id: decode_process_lease_field(process_id, "process_id")?,
            owner: decode_process_lease_field(owner, "owner")?,
            lease_token: decode_process_lease_field(lease_token, "lease_token")?,
            fencing_token: decode_process_lease_field(fencing_token, "fencing_token")?,
            claimed_at_epoch_ms: decode_process_lease_field(
                claimed_at_epoch_ms,
                "claimed_at_epoch_ms",
            )?,
            expires_at_epoch_ms: decode_process_lease_field(
                expires_at_epoch_ms,
                "expires_at_epoch_ms",
            )?,
        })
    }
}

fn buffer_process_lease_field<'de, A, T>(
    map: &mut A,
    field: &mut Option<BufferedProcessLeaseField<T>>,
    duplicate_field: &mut Option<&'static str>,
    name: &'static str,
    version_is_known: bool,
) -> Result<(), A::Error>
where
    A: serde::de::MapAccess<'de>,
    T: Deserialize<'de>,
{
    if field.is_some() {
        map.next_value::<serde::de::IgnoredAny>()?;
        duplicate_field.get_or_insert(name);
    } else if version_is_known {
        *field = Some(BufferedProcessLeaseField::Decoded(map.next_value()?));
    } else {
        *field = Some(BufferedProcessLeaseField::Value(map.next_value()?));
    }
    Ok(())
}

fn decode_process_lease_field<T, E>(
    field: Option<BufferedProcessLeaseField<T>>,
    name: &'static str,
) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let value = field.ok_or_else(|| serde::de::Error::missing_field(name))?;
    match value {
        BufferedProcessLeaseField::Decoded(value) => Ok(value),
        BufferedProcessLeaseField::Value(value) => {
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        }
    }
}

impl<'de> Deserialize<'de> for ProcessLease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "schema_version",
            "process_id",
            "owner",
            "lease_token",
            "fencing_token",
            "claimed_at_epoch_ms",
            "expires_at_epoch_ms",
        ];
        deserializer.deserialize_struct("ProcessLease", FIELDS, ProcessLeaseVisitor)
    }
}
