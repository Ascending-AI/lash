use std::sync::Arc;

use lash_core::{SessionAppendNode, ToolArgumentProjectionPolicy};
use lash_rlm_types::{PROJECTED_JSON_TAG, PROJECTION_REF_JSON_TAG};
use lashlang::{
    BudgetedJsonProjector, ImageValue, ProjectedFuture, ProjectedValue, Record as FlowRecord,
    State as FlowState, Value as FlowValue, ValueProjectionContext, ValueProjector,
};
use serde_json::Value;

use super::bindings::{ProjectionRef, ProjectionResolver, RlmProjectedSeedError};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RlmSeed {
    pub projected: lash_rlm_types::RlmProjectedSeedSnapshot,
    pub globals: serde_json::Map<String, Value>,
}

impl RlmSeed {
    pub fn from_tool_args(args: &Value) -> Result<Self, String> {
        match args.get("seed") {
            None => Ok(Self::default()),
            Some(seed) => Self::from_seed_value(seed),
        }
    }

    pub fn from_seed_value(seed: &Value) -> Result<Self, String> {
        let raw = match seed {
            Value::Null => return Ok(Self::default()),
            Value::Object(map) => map,
            _ => return Err("`seed` must be a record/dict".to_string()),
        };
        let mut out = Self::default();
        for (name, value) in raw.iter() {
            if let Some(inner) = lash_rlm_types::projection_inner(value) {
                out.projected.push(name.clone(), inner.clone());
            } else {
                out.globals.insert(name.clone(), value.clone());
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.projected.is_empty()
    }

    pub fn into_event_body(self) -> lash_rlm_types::RlmSeedPluginBody {
        lash_rlm_types::RlmSeedPluginBody {
            globals: self.globals,
            projected: self.projected,
        }
    }
}

pub fn rlm_seed_initial_nodes(seed: RlmSeed) -> Vec<SessionAppendNode> {
    if seed.is_empty() {
        return Vec::new();
    }
    vec![SessionAppendNode::protocol_event(
        super::context::rlm_protocol_event(lash_rlm_types::RlmProtocolEvent::RlmSeed(
            seed.into_event_body(),
        )),
    )]
}

pub(crate) fn normalize_tool_args_for_projection(
    args: Value,
    policy: &ToolArgumentProjectionPolicy,
) -> Value {
    match policy {
        ToolArgumentProjectionPolicy::MaterializeProjectedValues => {
            materialize_projected_json(args)
        }
        ToolArgumentProjectionPolicy::PreserveProjectedRefsInField { field } => {
            normalize_seed_preserving_tool_args(args, field)
        }
    }
}

#[cfg(test)]
pub(crate) async fn flow_record_to_tool_args(
    record: &FlowRecord,
    policy: &ToolArgumentProjectionPolicy,
) -> Value {
    normalize_tool_args_for_projection(flow_record_to_json_value(record).await, policy)
}

fn normalize_seed_preserving_tool_args(args: Value, field: &str) -> Value {
    let Value::Object(args) = args else {
        return materialize_projected_json(args);
    };
    Value::Object(
        args.into_iter()
            .map(|(key, value)| {
                let value = if key == field {
                    normalize_projected_seed(value)
                } else {
                    materialize_projected_json(value)
                };
                (key, value)
            })
            .collect(),
    )
}

fn normalize_projected_seed(seed: Value) -> Value {
    let Value::Object(seed) = seed else {
        return materialize_projected_json(seed);
    };
    Value::Object(
        seed.into_iter()
            .map(|(key, value)| {
                let value = if lash_rlm_types::projection_inner(&value).is_some() {
                    value
                } else {
                    materialize_projected_json(value)
                };
                (key, value)
            })
            .collect(),
    )
}

fn materialize_projected_json(value: Value) -> Value {
    if let Some(inner) = lash_rlm_types::projection_inner(&value) {
        return materialize_projected_json(inner.clone());
    }
    match value {
        Value::Array(items) => {
            Value::Array(items.into_iter().map(materialize_projected_json).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, materialize_projected_json(value)))
                .collect(),
        ),
        value => value,
    }
}

pub(crate) fn flow_to_json_value<'a>(value: &'a FlowValue) -> ProjectedFuture<'a, Value> {
    Box::pin(async move {
        match value {
            FlowValue::Null | FlowValue::Undefined => Value::Null,
            FlowValue::Bool(value) => Value::Bool(*value),
            FlowValue::Number(value) => json_number(*value),
            FlowValue::String(value) => Value::String(value.to_string()),
            FlowValue::Image(image) => serde_json::to_value(image)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            FlowValue::Resource(resource) => serde_json::to_value(resource)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            FlowValue::Tuple(values) | FlowValue::List(values) => {
                let mut out = Vec::with_capacity(values.len());
                for value in values.iter() {
                    out.push(flow_to_json_value(value).await);
                }
                Value::Array(out)
            }
            FlowValue::Record(record) => flow_record_to_json_value(record).await,
            FlowValue::Projected(value) => {
                if let Some(reference) = value.projection_ref() {
                    let mut ref_obj = serde_json::Map::with_capacity(1);
                    ref_obj.insert(PROJECTION_REF_JSON_TAG.to_string(), reference.clone());
                    let mut obj = serde_json::Map::with_capacity(1);
                    obj.insert(PROJECTED_JSON_TAG.to_string(), Value::Object(ref_obj));
                    return Value::Object(obj);
                }
                let inner = flow_to_json_value(&value.materialize_async().await).await;
                let mut obj = serde_json::Map::with_capacity(1);
                obj.insert(PROJECTED_JSON_TAG.to_string(), inner);
                Value::Object(obj)
            }
            FlowValue::Ref(_) => {
                unreachable!("VM heap references must be materialized before JSON rendering")
            }
        }
    })
}

pub(crate) async fn flow_record_to_json_value(record: &FlowRecord) -> Value {
    let mut object = serde_json::Map::with_capacity(record.len());
    for (key, value) in record.iter() {
        if matches!(value, FlowValue::Undefined) {
            continue;
        }
        object.insert(key.to_string(), flow_to_json_value(value).await);
    }
    Value::Object(object)
}

fn json_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 {
        let as_i64 = value as i64 as f64;
        if as_i64 == value {
            return Value::Number(serde_json::Number::from(value as i64));
        }
        let as_u64 = value as u64 as f64;
        if as_u64 == value {
            return Value::Number(serde_json::Number::from(value as u64));
        }
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub(crate) fn json_to_flow_value(value: Value) -> FlowValue {
    match value {
        Value::Null => FlowValue::Null,
        Value::Bool(value) => FlowValue::Bool(value),
        Value::Number(value) => FlowValue::Number(value.as_f64().unwrap_or_default()),
        Value::String(value) => FlowValue::String(value.into()),
        Value::Array(values) => {
            FlowValue::List(values.into_iter().map(json_to_flow_value).collect())
        }
        Value::Object(map) => json_map_to_image(&map)
            .map(|image| FlowValue::Image(Box::new(image)))
            .unwrap_or_else(|| {
                FlowValue::Record(Arc::new(
                    map.into_iter()
                        .map(|(key, value)| (key, json_to_flow_value(value)))
                        .collect::<FlowRecord>(),
                ))
            }),
    }
}

/// Resolves every projected reference held in the session's globals.
///
/// The rehydrated bindings are committed as one batch: the turn's whole
/// rehydration costs one heap copy and one collection rather than one of each
/// per key, and a rehydration that cannot be committed leaves the globals as
/// they were.
pub(crate) async fn rehydrate_projected_globals(
    rlm: &mut FlowState,
    projection_resolver: Arc<dyn ProjectionResolver>,
) -> Result<(), String> {
    let keys = rlm.globals().keys().map(str::to_string).collect::<Vec<_>>();
    let mut patch = Vec::new();
    for key in keys {
        if let Some(mut value) = rlm.globals().get(&key).cloned()
            && rehydrate_projected_value(&mut value, Arc::clone(&projection_resolver)).await?
        {
            patch.push(lashlang::GlobalPatch::Insert { name: key, value });
        }
    }
    rlm.patch_globals(patch)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn rehydrate_projected_value<'a>(
    value: &'a mut FlowValue,
    projection_resolver: Arc<dyn ProjectionResolver>,
) -> ProjectedFuture<'a, Result<bool, String>> {
    Box::pin(async move {
        match value {
            FlowValue::Projected(projected) => {
                let Some(ref_json) = projected.projection_ref().cloned() else {
                    return Ok(false);
                };
                let name = projected.name().to_string();
                let reference = serde_json::from_value::<ProjectionRef>(ref_json.clone())
                    .map_err(|err| format!("invalid projection ref for `{name}`: {err}"))?;
                let resolved = projection_resolver
                    .resolve_projection(&reference)
                    .await
                    .map_err(|err| err.to_string())?;
                *value = FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                    name, resolved, ref_json,
                ));
                Ok(true)
            }
            FlowValue::Tuple(values) => {
                let mut changed = false;
                let mut restored = values.iter().cloned().collect::<Vec<_>>();
                for value in restored.iter_mut() {
                    changed |=
                        rehydrate_projected_value(value, Arc::clone(&projection_resolver)).await?;
                }
                if changed {
                    *value = FlowValue::Tuple(restored.into());
                }
                Ok(changed)
            }
            FlowValue::List(values) => {
                let mut changed = false;
                let mut restored = values.iter().cloned().collect::<Vec<_>>();
                for value in restored.iter_mut() {
                    changed |=
                        rehydrate_projected_value(value, Arc::clone(&projection_resolver)).await?;
                }
                if changed {
                    *value = FlowValue::List(restored.into());
                }
                Ok(changed)
            }
            FlowValue::Record(record) => {
                let mut changed = false;
                let record = Arc::make_mut(record);
                let keys = record.keys().map(str::to_string).collect::<Vec<_>>();
                for key in keys {
                    if let Some(value) = record.get_mut(&key) {
                        changed |=
                            rehydrate_projected_value(value, Arc::clone(&projection_resolver))
                                .await?;
                    }
                }
                Ok(changed)
            }
            FlowValue::Null
            | FlowValue::Undefined
            | FlowValue::Bool(_)
            | FlowValue::Number(_)
            | FlowValue::String(_)
            | FlowValue::Resource(_)
            | FlowValue::Image(_) => Ok(false),
            FlowValue::Ref(_) => {
                unreachable!("VM heap references must be materialized before projection restore")
            }
        }
    })
}

fn json_map_to_image(map: &serde_json::Map<String, Value>) -> Option<ImageValue> {
    if map.get("type")?.as_str()? != "image" {
        return None;
    }
    Some(ImageValue::new(
        map.get("id")?.as_str()?.to_string(),
        lash_core::MediaType::parse(map.get("mime")?.as_str()?).ok()?,
        map.get("label")?.as_str()?.to_string(),
        map.get("size")?.as_u64()?,
        optional_json_u32(map.get("width")?)?,
        optional_json_u32(map.get("height")?)?,
    ))
}

fn optional_json_u32(value: &Value) -> Option<Option<u32>> {
    match value {
        Value::Null => Some(None),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some),
        _ => None,
    }
}

pub(crate) async fn format_output_value(value: &FlowValue) -> String {
    BudgetedJsonProjector::unbounded()
        .project(ValueProjectionContext::new(value))
        .await
}

pub(crate) fn projection_ref_from_seed_value(
    name: &str,
    value: &Value,
) -> Result<Option<ProjectionRef>, RlmProjectedSeedError> {
    let Some(inner) = lash_rlm_types::projection_ref_inner(value) else {
        return Ok(None);
    };
    serde_json::from_value::<ProjectionRef>(inner.clone())
        .map(Some)
        .map_err(|err| RlmProjectedSeedError::invalid_projection_ref(name, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn undefined_uses_json_stringify_container_rules() {
        let record = FlowRecord::from_iter([
            ("absent".to_string(), FlowValue::Undefined),
            ("present".to_string(), FlowValue::Null),
            (
                "items".to_string(),
                FlowValue::List(vec![FlowValue::Undefined, FlowValue::Null].into()),
            ),
        ]);

        assert_eq!(
            flow_record_to_json_value(&record).await,
            serde_json::json!({"present": null, "items": [null, null]})
        );
        assert_eq!(
            json_to_flow_value(serde_json::Value::Null),
            FlowValue::Null,
            "JSON has no representation that can manufacture undefined"
        );
    }
}
