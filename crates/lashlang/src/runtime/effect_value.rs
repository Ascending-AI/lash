//! Heapless values crossing the owned effect seam. JSON projections lose
//! undefined, tuple identity and non-finite numbers, so this wire uses explicit
//! variants and IEEE bits. A projection crosses as what it is: a scalar with
//! its value, a resource as its `ResourceRef`, never a host handle (ADR 0132
//! §9). Heap references cannot cross.

use super::{ImageValue, Record, ResourceHandle, ResourceRef, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum EffectValue {
    Null,
    Undefined,
    Bool(bool),
    Number(u64),
    String(String),
    Image(ImageValue),
    Resource(ResourceHandle),
    Tuple(Vec<EffectValue>),
    List(Vec<EffectValue>),
    Record(Vec<(String, EffectValue)>),
    Projection {
        name: String,
        scalar: Box<EffectValue>,
    },
    ProjectedResource {
        name: String,
        type_name: String,
        resource: ResourceRef,
    },
}
impl EffectValue {
    fn of(value: &Value, depth: usize) -> Result<Self, &'static str> {
        if depth > 64 {
            return Err("effect value exceeds depth 64");
        }
        let items = |values: &[Value]| {
            values
                .iter()
                .map(|value| Self::of(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(match value {
            Value::Null => Self::Null,
            Value::Undefined => Self::Undefined,
            Value::Bool(v) => Self::Bool(*v),
            Value::Number(v) => Self::Number(v.to_bits()),
            Value::String(v) => Self::String(v.to_string()),
            Value::Image(v) => Self::Image((**v).clone()),
            Value::Resource(v) => Self::Resource(v.clone()),
            Value::Tuple(v) => Self::Tuple(items(v)?),
            Value::List(v) => Self::List(items(v)?),
            Value::Record(v) => Self::Record(
                v.iter()
                    .map(|(key, value)| Ok((key.to_string(), Self::of(value, depth + 1)?)))
                    .collect::<Result<_, &'static str>>()?,
            ),
            Value::Projected(value) => match (value.scalar_value(), value.resource_ref()) {
                (Some(scalar), _) => Self::Projection {
                    name: value.name().to_owned(),
                    scalar: Box::new(Self::of(scalar, depth + 1)?),
                },
                (None, Some(resource)) => Self::ProjectedResource {
                    name: value.name().to_owned(),
                    type_name: value.type_name().to_owned(),
                    resource: resource.clone(),
                },
                (None, None) => return Err("a projection is a scalar or a resource"),
            },
            Value::Ref(_) => return Err("heap references cannot cross an effect frame"),
        })
    }
    fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Undefined => Value::Undefined,
            Self::Bool(v) => Value::Bool(v),
            Self::Number(v) => Value::Number(f64::from_bits(v)),
            Self::String(v) => Value::String(v.into()),
            Self::Image(v) => Value::Image(Box::new(v)),
            Self::Resource(v) => Value::Resource(v),
            Self::Tuple(v) => Value::Tuple(
                v.into_iter()
                    .map(Self::into_value)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::List(v) => Value::List(
                v.into_iter()
                    .map(Self::into_value)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::Projection { name, scalar } => {
                Value::Projected(super::ProjectedValue::scalar(name, scalar.into_value()))
            }
            Self::ProjectedResource {
                name,
                type_name,
                resource,
            } => Value::Projected(super::ProjectedValue::resource(name, type_name, resource)),
            Self::Record(v) => {
                let mut record = Record::new();
                for (key, value) in v {
                    record.insert(key, value.into_value());
                }
                Value::Record(std::sync::Arc::new(record))
            }
        }
    }
}
pub fn serialize<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
    EffectValue::of(value, 0)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    EffectValue::deserialize(deserializer).map(EffectValue::into_value)
}
pub mod list {
    use super::*;
    pub fn serialize<S: Serializer>(values: &[Value], serializer: S) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(|value| EffectValue::of(value, 0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Value>, D::Error> {
        Vec::<EffectValue>::deserialize(deserializer)
            .map(|values| values.into_iter().map(EffectValue::into_value).collect())
    }
}

/// Heapless record serialization preserves projection identities without reads.
pub mod record {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Record, serializer: S) -> Result<S::Ok, S::Error> {
        EffectValue::of(&Value::Record(std::sync::Arc::new(value.clone())), 0)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Record, D::Error> {
        match EffectValue::deserialize(deserializer)?.into_value() {
            Value::Record(record) => Ok((*record).clone()),
            _ => Err(serde::de::Error::custom("expected a record")),
        }
    }
}
pub mod optional {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(|value| EffectValue::of(value, 0))
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error> {
        Option::<EffectValue>::deserialize(deserializer)
            .map(|value| value.map(EffectValue::into_value))
    }
}
pub mod map {
    use super::*;
    use std::collections::BTreeMap;
    pub fn serialize<S: Serializer>(
        value: &BTreeMap<String, Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .iter()
            .map(|(key, value)| EffectValue::of(value, 0).map(|value| (key, value)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, Value>, D::Error> {
        BTreeMap::<String, EffectValue>::deserialize(deserializer).map(|values| {
            values
                .into_iter()
                .map(|(key, value)| (key, value.into_value()))
                .collect()
        })
    }
}
