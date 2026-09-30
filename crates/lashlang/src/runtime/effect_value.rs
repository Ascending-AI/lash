//! Heapless values crossing the owned effect seam. JSON projections lose
//! undefined, tuple identity and non-finite numbers, so this wire uses explicit
//! variants and IEEE bits. Heap references and live descriptors cannot cross.

use super::{ImageValue, Record, ResourceHandle, Value};
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
            Value::Ref(_) | Value::Projected(_) => {
                return Err("heap references and host descriptors cannot cross an effect frame");
            }
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
pub(super) fn serialize<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
    EffectValue::of(value, 0)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    EffectValue::deserialize(deserializer).map(EffectValue::into_value)
}
pub(super) mod list {
    use super::*;
    pub(crate) fn serialize<S: Serializer>(
        values: &[Value],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(|value| EffectValue::of(value, 0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Value>, D::Error> {
        Vec::<EffectValue>::deserialize(deserializer)
            .map(|values| values.into_iter().map(EffectValue::into_value).collect())
    }
}
