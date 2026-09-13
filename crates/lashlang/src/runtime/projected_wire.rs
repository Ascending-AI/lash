//! The one canonical encoding of a `Value::Projected` shared by both durable
//! writers.
//!
//! The VM continuation wire and the `State` snapshot wire used to disagree about
//! projected values: the continuation refused them recursively while the
//! snapshot accepted and degraded them, so an ordinary program that merely put a
//! projected binding inside a list could not park at all (FIG-2865). They now
//! encode the same three fields through the same types, so a value that one
//! writer accepts the other accepts, and a placeholder decoded from either wire
//! is the same placeholder.
//!
//! Only the projection's *identity* is durable — its `name`, its declared
//! `type_name`, and the host-supplied `projection_ref` that lets the producing
//! side rebuild it. The host descriptor behind it is not serializable, so a
//! decoded projection is an unavailable placeholder — every read refuses with
//! `RuntimeError::ProjectedValueUnavailable` — until the live binding is
//! re-supplied; see `super::projected_refresh`.

use serde::{Deserialize, Serialize};

use super::state::{MAX_SNAPSHOT_VALUE_DEPTH, SnapshotDecodeError, child_location};
use super::{ContinuationError, ProjectedValue};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CanonicalProjectedValue {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) projection_ref: Option<CanonicalJsonValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CanonicalJsonValue {
    Null {},
    Bool { value: bool },
    Number { value: serde_json::Number },
    String { value: String },
    Array { items: Vec<CanonicalJsonValue> },
    Object { fields: Vec<CanonicalJsonField> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CanonicalJsonField {
    pub(crate) name: String,
    pub(crate) value: CanonicalJsonValue,
}

impl CanonicalProjectedValue {
    pub(crate) fn from_projected(
        projected: &ProjectedValue,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        Ok(Self {
            name: projected.name().to_string(),
            type_name: projected.value_type_name().to_string(),
            projection_ref: projected
                .projection_ref()
                .map(|value| {
                    CanonicalJsonValue::from_json(
                        value,
                        &format!("{location}.projection_ref"),
                        depth + 1,
                    )
                })
                .transpose()?,
        })
    }

    pub(crate) fn into_projected(self) -> Result<ProjectedValue, SnapshotDecodeError> {
        Ok(
            ProjectedValue::unavailable_after_restore_with_projection_ref(
                self.name,
                self.type_name,
                self.projection_ref
                    .map(CanonicalJsonValue::into_json)
                    .transpose()?,
            ),
        )
    }
}

impl CanonicalJsonValue {
    pub(crate) fn from_json(
        value: &serde_json::Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        if depth > MAX_SNAPSHOT_VALUE_DEPTH {
            return Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "value beyond the snapshot depth limit",
            });
        }
        Ok(match value {
            serde_json::Value::Null => Self::Null {},
            serde_json::Value::Bool(value) => Self::Bool { value: *value },
            serde_json::Value::Number(value) => Self::Number {
                value: value.clone(),
            },
            serde_json::Value::String(value) => Self::String {
                value: value.clone(),
            },
            serde_json::Value::Array(items) => Self::Array {
                items: items
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        Self::from_json(value, &format!("{location}[{index}]"), depth + 1)
                    })
                    .collect::<Result<_, _>>()?,
            },
            serde_json::Value::Object(fields) => {
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Object {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location(location, name);
                            Ok(CanonicalJsonField {
                                name: name.clone(),
                                value: Self::from_json(value, &location, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
        })
    }

    pub(crate) fn into_json(self) -> Result<serde_json::Value, SnapshotDecodeError> {
        Ok(match self {
            Self::Null {} => serde_json::Value::Null,
            Self::Bool { value } => serde_json::Value::Bool(value),
            Self::Number { value } => serde_json::Value::Number(value),
            Self::String { value } => serde_json::Value::String(value),
            Self::Array { items } => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(Self::into_json)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object { fields } => serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|field| field.value.into_json().map(|value| (field.name, value)))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}
