use std::{borrow::Cow, fmt};

use serde::Deserialize;
use serde_content::{Data, Value};

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

struct ProcessLeaseVisitor {
    human_readable: bool,
}

type BufferedProcessLeaseField = Value<'static>;

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
                )?,
                ProcessLeaseField::Owner => {
                    buffer_process_lease_field(&mut map, &mut owner, &mut duplicate_field, "owner")?
                }
                ProcessLeaseField::LeaseToken => buffer_process_lease_field(
                    &mut map,
                    &mut lease_token,
                    &mut duplicate_field,
                    "lease_token",
                )?,
                ProcessLeaseField::FencingToken => buffer_process_lease_field(
                    &mut map,
                    &mut fencing_token,
                    &mut duplicate_field,
                    "fencing_token",
                )?,
                ProcessLeaseField::ClaimedAtEpochMs => buffer_process_lease_field(
                    &mut map,
                    &mut claimed_at_epoch_ms,
                    &mut duplicate_field,
                    "claimed_at_epoch_ms",
                )?,
                ProcessLeaseField::ExpiresAtEpochMs => buffer_process_lease_field(
                    &mut map,
                    &mut expires_at_epoch_ms,
                    &mut duplicate_field,
                    "expires_at_epoch_ms",
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
            process_id: decode_process_lease_field(process_id, "process_id", self.human_readable)?,
            owner: decode_process_lease_field(owner, "owner", self.human_readable)?,
            lease_token: decode_process_lease_field(
                lease_token,
                "lease_token",
                self.human_readable,
            )?,
            fencing_token: decode_process_lease_field(
                fencing_token,
                "fencing_token",
                self.human_readable,
            )?,
            claimed_at_epoch_ms: decode_process_lease_field(
                claimed_at_epoch_ms,
                "claimed_at_epoch_ms",
                self.human_readable,
            )?,
            expires_at_epoch_ms: decode_process_lease_field(
                expires_at_epoch_ms,
                "expires_at_epoch_ms",
                self.human_readable,
            )?,
        })
    }
}

fn buffer_process_lease_field<'de, A>(
    map: &mut A,
    field: &mut Option<BufferedProcessLeaseField>,
    duplicate_field: &mut Option<&'static str>,
    name: &'static str,
) -> Result<(), A::Error>
where
    A: serde::de::MapAccess<'de>,
{
    if field.is_some() {
        map.next_value::<serde::de::IgnoredAny>()?;
        duplicate_field.get_or_insert(name);
    } else {
        *field = Some(map.next_value()?);
    }
    Ok(())
}

fn decode_process_lease_field<T, E>(
    field: Option<BufferedProcessLeaseField>,
    name: &'static str,
    human_readable: bool,
) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let value = field.ok_or_else(|| serde::de::Error::missing_field(name))?;
    // Preserve the coercions that common self-describing formats apply when
    // decoding directly into String/u64 fields, while the content value keeps
    // every map entry and the full Serde integer range until the version fence.
    let value = normalize_utf8_bytes(value);
    let decoded = match value {
        Value::Seq(values) | Value::Tuple(values) => {
            let values = values
                .into_iter()
                .map(|value| content_deserializer(value, human_readable));
            T::deserialize(serde::de::value::SeqDeserializer::new(values))
        }
        value => T::deserialize(content_deserializer(value, human_readable)),
    };
    decoded.map_err(serde::de::Error::custom)
}

fn content_deserializer(
    value: Value<'static>,
    human_readable: bool,
) -> serde_content::Deserializer<'static> {
    let deserializer = serde_content::Deserializer::new(value).coerce_numbers();
    if human_readable {
        deserializer.human_readable()
    } else {
        deserializer
    }
}

fn normalize_utf8_bytes(value: Value<'static>) -> Value<'static> {
    match value {
        Value::Bytes(bytes) => match String::from_utf8(bytes.into_owned()) {
            Ok(value) => Value::String(Cow::Owned(value)),
            Err(error) => Value::Bytes(Cow::Owned(error.into_bytes())),
        },
        Value::Seq(values) => Value::Seq(values.into_iter().map(normalize_utf8_bytes).collect()),
        Value::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (normalize_utf8_bytes(key), normalize_utf8_bytes(value)))
                .collect(),
        ),
        Value::Option(value) => {
            Value::Option(value.map(|value| Box::new(normalize_utf8_bytes(*value))))
        }
        Value::Struct(mut value) => {
            value.data = normalize_utf8_data(value.data);
            Value::Struct(value)
        }
        Value::Enum(mut value) => {
            value.data = normalize_utf8_data(value.data);
            Value::Enum(value)
        }
        Value::Tuple(values) => {
            Value::Tuple(values.into_iter().map(normalize_utf8_bytes).collect())
        }
        value => value,
    }
}

fn normalize_utf8_data(data: Data<'static>) -> Data<'static> {
    match data {
        Data::Unit => Data::Unit,
        Data::NewType { value } => Data::NewType {
            value: normalize_utf8_bytes(value),
        },
        Data::Tuple { values } => Data::Tuple {
            values: values.into_iter().map(normalize_utf8_bytes).collect(),
        },
        Data::Struct { fields } => Data::Struct {
            fields: fields
                .into_iter()
                .map(|(key, value)| (key, normalize_utf8_bytes(value)))
                .collect(),
        },
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
        let human_readable = deserializer.is_human_readable();
        deserializer.deserialize_struct(
            "ProcessLease",
            FIELDS,
            ProcessLeaseVisitor { human_readable },
        )
    }
}
