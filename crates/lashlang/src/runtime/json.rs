//! JSON serialization for `Value`. Bidirectional bridge between the
//! lashlang value tree and `serde_json::Value`. Tuples serialize as arrays in
//! runtime/public JSON; persisted snapshots use the separate canonical typed
//! MessagePack codec in `runtime::state`.
//! Image attachments encode
//! as `{"type": "image", "id": ..., "mime": ..., "label": ..., "size": ...,
//! "width": ..., "height": ...}` and round-trip via `image_to_json` /
//! `image_from_json_map`. Projected values serialize with the canonical
//! `{"__projected__": <inner>}` wrapper (defined in `lash-rlm-types`).

use std::sync::Arc;

#[cfg(test)]
use super::value_contains_projected;
use super::{ImageValue, ProjectedFuture, ResourceHandle, Value};
use serde::Serialize;
use serde::ser::{SerializeMap, SerializeSeq};
use std::fmt::Write as _;

pub(crate) fn json_number(value: f64) -> Option<serde_json::Number> {
    if !value.is_finite() {
        return None;
    }
    if value.is_finite() && value.fract() == 0.0 {
        let as_i64 = value as i64 as f64;
        if as_i64 == value {
            return Some(serde_json::Number::from(value as i64));
        }
        let as_u64 = value as u64 as f64;
        if as_u64 == value {
            return Some(serde_json::Number::from(value as u64));
        }
    }
    serde_json::Number::from_f64(value)
}

#[cfg(test)]
pub(crate) fn to_json_async<'a>(value: &'a Value) -> ProjectedFuture<'a, serde_json::Value> {
    Box::pin(async move {
        match value {
            Value::Null => serde_json::Value::Null,
            Value::Bool(value) => serde_json::Value::Bool(*value),
            Value::Number(value) => json_number(*value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(value) => serde_json::Value::String(value.to_string()),
            Value::Image(image) => image_to_json(image),
            Value::Resource(handle) => resource_to_json(handle),
            Value::Tuple(values) | Value::List(values) => {
                let mut out = Vec::with_capacity(values.len());
                for value in values.iter() {
                    out.push(to_json_async(value).await);
                }
                serde_json::Value::Array(out)
            }
            Value::Record(record) => {
                let mut object = serde_json::Map::with_capacity(record.len());
                for (key, value) in record.iter() {
                    object.insert(key.to_string(), to_json_async(value).await);
                }
                serde_json::Value::Object(object)
            }
            Value::Projected(value) => to_json_async(&value.materialize_async().await).await,
            Value::Ref(_) => unreachable!("heap references must be exported before JSON"),
        }
    })
}

#[cfg(test)]
pub(crate) fn to_json(value: &Value) -> serde_json::Value {
    if value_contains_projected(value) {
        futures_executor::block_on(to_json_async(value))
    } else {
        to_json_direct(value)
    }
}

#[cfg(test)]
pub(crate) fn to_json_direct(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Number(value) => json_number(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(value) => serde_json::Value::String(value.to_string()),
        Value::Image(image) => image_to_json(image),
        Value::Resource(handle) => resource_to_json(handle),
        Value::Tuple(values) | Value::List(values) => {
            serde_json::Value::Array(values.iter().map(to_json_direct).collect())
        }
        Value::Record(record) => {
            let mut object = serde_json::Map::with_capacity(record.len());
            for (key, value) in record.iter() {
                object.insert(key.to_string(), to_json_direct(value));
            }
            serde_json::Value::Object(object)
        }
        Value::Projected(_) => unreachable!("projected values require async json conversion"),
        Value::Ref(_) => unreachable!("heap references must be exported before JSON"),
    }
}

pub(crate) struct RuntimeJson<'a>(pub(crate) &'a Value);
pub(crate) struct DirectJson<'a>(pub(crate) &'a Value);

impl Serialize for RuntimeJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Value::Projected(projected) => {
                RuntimeJson(&projected.materialize()).serialize(serializer)
            }
            value => serialize_value(value, serializer, ProjectedMode::Runtime),
        }
    }
}

impl Serialize for DirectJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_value(self.0, serializer, ProjectedMode::Direct)
    }
}

#[derive(Clone, Copy)]
enum ProjectedMode {
    Runtime,
    Direct,
}

fn serialize_value<S>(
    value: &Value,
    serializer: S,
    projected_mode: ProjectedMode,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Value::Null => serializer.serialize_none(),
        Value::Bool(value) => serializer.serialize_bool(*value),
        Value::Number(value) => match json_number(*value) {
            Some(value) => value.serialize(serializer),
            None => serializer.serialize_none(),
        },
        Value::String(value) => serializer.serialize_str(value),
        Value::Image(image) => serialize_image(image, serializer),
        Value::Resource(handle) => serialize_resource(handle, serializer),
        Value::Tuple(values) | Value::List(values) => {
            let mut sequence = serializer.serialize_seq(Some(values.len()))?;
            for value in values.iter() {
                match projected_mode {
                    ProjectedMode::Runtime => sequence.serialize_element(&RuntimeJson(value))?,
                    ProjectedMode::Direct => sequence.serialize_element(&DirectJson(value))?,
                }
            }
            sequence.end()
        }
        Value::Record(record) => {
            let mut entries = record.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            let mut map = serializer.serialize_map(Some(entries.len()))?;
            for (key, value) in entries {
                match projected_mode {
                    ProjectedMode::Runtime => map.serialize_entry(key, &RuntimeJson(value))?,
                    ProjectedMode::Direct => map.serialize_entry(key, &DirectJson(value))?,
                }
            }
            map.end()
        }
        Value::Projected(projected) => match projected_mode {
            ProjectedMode::Runtime => RuntimeJson(&projected.materialize()).serialize(serializer),
            ProjectedMode::Direct => {
                unreachable!("projected values require runtime json conversion")
            }
        },
        Value::Ref(_) => Err(serde::ser::Error::custom(
            "heap references must be exported before JSON serialization",
        )),
    }
}

fn serialize_image<S>(image: &ImageValue, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let mut map = serializer.serialize_map(Some(7))?;
    map.serialize_entry("height", &image.height)?;
    map.serialize_entry("id", &image.id)?;
    map.serialize_entry("mime", &image.mime)?;
    map.serialize_entry("label", &image.label)?;
    map.serialize_entry("size", &image.size)?;
    map.serialize_entry("type", "image")?;
    map.serialize_entry("width", &image.width)?;
    map.end()
}

pub(crate) fn append_direct_json(output: &mut String, value: &Value) {
    output.push_str(
        &serde_json::to_string(&DirectJson(value))
            .expect("value json serialization should succeed"),
    );
}

pub(crate) fn append_runtime_json_async<'a>(
    output: &'a mut String,
    value: &'a Value,
) -> ProjectedFuture<'a, ()> {
    Box::pin(async move {
        match value {
            Value::Null => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => match json_number(*value) {
                Some(value) => write!(output, "{value}").expect("string writes should not fail"),
                None => output.push_str("null"),
            },
            Value::String(value) => output.push_str(
                &serde_json::to_string(value).expect("string json serialization should succeed"),
            ),
            Value::Image(_) | Value::Resource(_) => append_direct_json(output, value),
            Value::Tuple(values) | Value::List(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    append_runtime_json_async(output, value).await;
                }
                output.push(']');
            }
            Value::Record(record) => {
                let mut entries = record.iter().collect::<Vec<_>>();
                entries.sort_unstable_by_key(|(key, _)| *key);
                output.push('{');
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(
                        &serde_json::to_string(key)
                            .expect("record key json serialization should succeed"),
                    );
                    output.push(':');
                    append_runtime_json_async(output, value).await;
                }
                output.push('}');
            }
            Value::Projected(projected) => {
                append_runtime_json_async(output, &projected.materialize_async().await).await;
            }
            Value::Ref(_) => unreachable!("heap references must be exported before JSON"),
        }
    })
}

#[cfg(test)]
pub(crate) fn image_to_json(image: &ImageValue) -> serde_json::Value {
    let mut object = serde_json::Map::with_capacity(7);
    object.insert(
        "type".to_string(),
        serde_json::Value::String("image".to_string()),
    );
    object.insert(
        "id".to_string(),
        serde_json::Value::String(image.id.clone()),
    );
    object.insert(
        "mime".to_string(),
        serde_json::Value::String(image.mime.to_string()),
    );
    object.insert(
        "label".to_string(),
        serde_json::Value::String(image.label.clone()),
    );
    object.insert(
        "size".to_string(),
        serde_json::Value::Number(serde_json::Number::from(image.size)),
    );
    object.insert(
        "width".to_string(),
        image
            .width
            .map(|width| serde_json::Value::Number(serde_json::Number::from(width)))
            .unwrap_or(serde_json::Value::Null),
    );
    object.insert(
        "height".to_string(),
        image
            .height
            .map(|height| serde_json::Value::Number(serde_json::Number::from(height)))
            .unwrap_or(serde_json::Value::Null),
    );
    serde_json::Value::Object(object)
}

pub fn from_json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => Value::Number(value.as_f64().unwrap_or_default()),
        serde_json::Value::String(value) => Value::String(value.into()),
        serde_json::Value::Array(values) => {
            Value::List(values.into_iter().map(from_json).collect::<Vec<_>>().into())
        }
        serde_json::Value::Object(map) => image_from_json_map(&map)
            .map(|image| Value::Image(Box::new(image)))
            .or_else(|| resource_from_json_map(&map).map(Value::Resource))
            .unwrap_or_else(|| {
                Value::Record(Arc::new(
                    map.into_iter()
                        .map(|(key, value)| (key, from_json(value)))
                        .collect(),
                ))
            }),
    }
}

fn serialize_resource<S>(handle: &ResourceHandle, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let mut map = serializer.serialize_map(Some(3))?;
    map.serialize_entry("__resource__", &true)?;
    map.serialize_entry("type", &handle.resource_type)?;
    map.serialize_entry("alias", &handle.alias)?;
    map.end()
}

#[cfg(test)]
fn resource_to_json(handle: &ResourceHandle) -> serde_json::Value {
    serde_json::json!({
        "__resource__": true,
        "type": handle.resource_type,
        "alias": handle.alias,
    })
}

pub(crate) fn resource_from_json_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<ResourceHandle> {
    if !map.get("__resource__")?.as_bool()? {
        return None;
    }
    Some(ResourceHandle::new(
        map.get("type")?.as_str()?.to_string(),
        map.get("alias")?.as_str()?.to_string(),
    ))
}

pub(crate) fn image_from_json_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<ImageValue> {
    if map.get("type")?.as_str()? != "image" {
        return None;
    }
    Some(ImageValue {
        id: map.get("id")?.as_str()?.to_string(),
        mime: crate::MediaType::parse(map.get("mime")?.as_str()?).ok()?,
        label: map.get("label")?.as_str()?.to_string(),
        size: map.get("size")?.as_u64()?,
        width: optional_u32_field(map.get("width")?)?,
        height: optional_u32_field(map.get("height")?)?,
    })
}

pub(crate) fn optional_u32_field(value: &serde_json::Value) -> Option<Option<u32>> {
    match value {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some),
        _ => None,
    }
}
