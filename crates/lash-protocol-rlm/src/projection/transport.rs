use std::sync::Arc;

use lash_core::{SessionAppendNode, ToolArgumentProjectionPolicy};
use lash_rlm_types::{PROJECTED_JSON_TAG, RlmProjectedSeedEntry};
use lashlang::{
    BudgetedJsonProjector, ImageValue, ProjectedFuture, ProjectedValue, Record as FlowRecord,
    State as FlowState, Value as FlowValue, ValueProjectionContext, ValueProjector,
};
use serde_json::Value;

use super::bindings::{ProjectionRef, ProjectionResolver};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectionTransportError {
    #[error("non-canonical `{PROJECTED_JSON_TAG}` wrapper: {reason}")]
    NonCanonicalWrapper { reason: String },
}

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
            let name = decode_seed_name(name, &out)?;
            if let Some(entry) = projected_entry(value).map_err(|error| error.to_string())? {
                let entry = match entry {
                    RlmProjectedSeedEntry::Materialized(value) => {
                        RlmProjectedSeedEntry::Materialized(decode_escaped_json(value)?)
                    }
                    RlmProjectedSeedEntry::Ref(reference) => RlmProjectedSeedEntry::Ref(reference),
                };
                out.projected.push(name, entry);
            } else {
                out.globals
                    .insert(name, decode_escaped_json(value.clone())?);
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
) -> Result<Value, ProjectionTransportError> {
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
) -> Result<Value, ProjectionTransportError> {
    normalize_tool_args_for_projection(flow_record_to_json_value(record).await, policy)
}

fn normalize_seed_preserving_tool_args(
    args: Value,
    field: &str,
) -> Result<Value, ProjectionTransportError> {
    let Value::Object(args) = args else {
        return materialize_projected_json(args);
    };
    let mut normalized = serde_json::Map::with_capacity(args.len());
    for (key, value) in args {
        let key = unescape_projected_key(key);
        if normalized.contains_key(&key) {
            return Err(ProjectionTransportError::NonCanonicalWrapper {
                reason: format!("escaped key `{key}` collides with another object key"),
            });
        }
        let value = if key == field {
            normalize_projected_seed(value)?
        } else {
            materialize_projected_json(value)?
        };
        normalized.insert(key, value);
    }
    Ok(Value::Object(normalized))
}

fn normalize_projected_seed(seed: Value) -> Result<Value, ProjectionTransportError> {
    let Value::Object(seed) = seed else {
        return materialize_projected_json(seed);
    };
    let mut normalized = serde_json::Map::with_capacity(seed.len());
    for (key, value) in seed {
        let value = if let Some(entry) = projected_entry(&value)? {
            let entry = match entry {
                RlmProjectedSeedEntry::Materialized(value) => RlmProjectedSeedEntry::Materialized(
                    materialize_projected_json_preserving_escapes(value)?,
                ),
                RlmProjectedSeedEntry::Ref(reference) => RlmProjectedSeedEntry::Ref(reference),
            };
            projected_wrapper(entry)
        } else {
            materialize_projected_json_preserving_escapes(value)?
        };
        normalized.insert(key, value);
    }
    Ok(Value::Object(normalized))
}

fn materialize_projected_json(value: Value) -> Result<Value, ProjectionTransportError> {
    materialize_projected_json_with_keys(value, TransportKeyMode::DecodeEscapes)
}

fn materialize_projected_json_preserving_escapes(
    value: Value,
) -> Result<Value, ProjectionTransportError> {
    materialize_projected_json_with_keys(value, TransportKeyMode::PreserveEscapes)
}

#[derive(Clone, Copy)]
enum TransportKeyMode {
    DecodeEscapes,
    PreserveEscapes,
}

fn materialize_projected_json_with_keys(
    value: Value,
    key_mode: TransportKeyMode,
) -> Result<Value, ProjectionTransportError> {
    if let Some(entry) = projected_entry(&value)? {
        return match entry {
            RlmProjectedSeedEntry::Materialized(value) => {
                materialize_projected_json_with_keys(value, key_mode)
            }
            RlmProjectedSeedEntry::Ref(reference) => {
                serde_json::to_value(reference).map_err(|error| {
                    ProjectionTransportError::NonCanonicalWrapper {
                        reason: format!("projection reference did not serialize: {error}"),
                    }
                })
            }
        };
    }
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|value| materialize_projected_json_with_keys(value, key_mode))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut decoded = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let key = match key_mode {
                    TransportKeyMode::DecodeEscapes => unescape_projected_key(key),
                    TransportKeyMode::PreserveEscapes => key,
                };
                if decoded.contains_key(&key) {
                    return Err(ProjectionTransportError::NonCanonicalWrapper {
                        reason: format!("escaped key `{key}` collides with another object key"),
                    });
                }
                decoded.insert(key, materialize_projected_json_with_keys(value, key_mode)?);
            }
            Ok(Value::Object(decoded))
        }
        value => Ok(value),
    }
}

fn decode_escaped_json(value: Value) -> Result<Value, String> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(decode_escaped_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut decoded = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let key = unescape_projected_key(key);
                if decoded.contains_key(&key) {
                    return Err(format!(
                        "non-canonical `{PROJECTED_JSON_TAG}` escape: decoded key `{key}` collides with another object key"
                    ));
                }
                decoded.insert(key, decode_escaped_json(value)?);
            }
            Ok(Value::Object(decoded))
        }
        value => Ok(value),
    }
}

fn decode_seed_name(name: &str, seed: &RlmSeed) -> Result<String, String> {
    let name = unescape_projected_key(name.to_string());
    if seed.globals.contains_key(&name)
        || seed
            .projected
            .entries
            .iter()
            .any(|(existing, _)| existing == &name)
    {
        return Err(format!(
            "non-canonical `{PROJECTED_JSON_TAG}` escape: decoded seed name `{name}` collides with another seed name"
        ));
    }
    Ok(name)
}

fn projected_entry(
    value: &Value,
) -> Result<Option<RlmProjectedSeedEntry>, ProjectionTransportError> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let Some(payload) = object.get(PROJECTED_JSON_TAG) else {
        return Ok(None);
    };
    if object.len() != 1 {
        return Err(ProjectionTransportError::NonCanonicalWrapper {
            reason: "reserved key must be the only object key".to_string(),
        });
    }
    serde_json::from_value(payload.clone())
        .map(Some)
        .map_err(|error| ProjectionTransportError::NonCanonicalWrapper {
            reason: error.to_string(),
        })
}

fn projected_wrapper(entry: RlmProjectedSeedEntry) -> Value {
    serde_json::json!({ PROJECTED_JSON_TAG: entry })
}

fn escape_projected_key(key: &str) -> String {
    if key.starts_with(PROJECTED_JSON_TAG) {
        format!("{PROJECTED_JSON_TAG}{key}")
    } else {
        key.to_string()
    }
}

fn unescape_projected_key(key: String) -> String {
    match key.strip_prefix(PROJECTED_JSON_TAG) {
        Some(rest) if rest.starts_with(PROJECTED_JSON_TAG) => rest.to_string(),
        _ => key,
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
                let entry = if let Some(reference) = value.projection_ref() {
                    let reference = serde_json::from_value::<ProjectionRef>(reference.clone())
                        .expect("projected values must carry a valid projection reference");
                    RlmProjectedSeedEntry::Ref(reference)
                } else {
                    RlmProjectedSeedEntry::Materialized(
                        flow_to_json_value(&value.materialize_async().await).await,
                    )
                };
                projected_wrapper(entry)
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
        object.insert(escape_projected_key(key), flow_to_json_value(value).await);
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
/// Result of restoring process-local projected host references.
pub(crate) struct ProjectedGlobalRehydration {
    pub(crate) degraded_bindings: Vec<lash_core::DegradedBinding>,
}

/// The successfully rehydrated bindings are committed as one batch: the turn's
/// whole rehydration costs one heap copy and one collection rather than one of
/// each per key. A reference failure leaves only its top-level binding in the
/// loudly unavailable state installed by snapshot restore.
pub(crate) async fn rehydrate_projected_globals(
    rlm: &mut FlowState,
    projection_resolver: Arc<dyn ProjectionResolver>,
) -> Result<ProjectedGlobalRehydration, String> {
    let keys = rlm.globals().keys().map(str::to_string).collect::<Vec<_>>();
    let mut patch = Vec::new();
    let mut degraded_bindings = Vec::new();
    for key in keys {
        if let Some(mut value) = rlm.globals().get(&key).cloned() {
            match rehydrate_projected_value(&mut value, Arc::clone(&projection_resolver)).await {
                Ok(true) => patch.push(lashlang::GlobalPatch::Insert { name: key, value }),
                Ok(false) => {}
                Err(reason) => {
                    degraded_bindings.push(lash_core::DegradedBinding { name: key, reason })
                }
            }
        }
    }
    rlm.patch_globals(patch)
        .map_err(|error| error.to_string())?;
    Ok(ProjectedGlobalRehydration { degraded_bindings })
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

    #[tokio::test]
    async fn reserved_projection_key_round_trips_as_plain_record() {
        let already_prefixed = format!("{PROJECTED_JSON_TAG}{PROJECTED_JSON_TAG}");
        let plain = FlowValue::Record(Arc::new(FlowRecord::from_iter([
            (
                PROJECTED_JSON_TAG.to_string(),
                FlowValue::String("plain data".into()),
            ),
            (
                already_prefixed,
                FlowValue::String("also plain data".into()),
            ),
        ])));

        let encoded = flow_to_json_value(&plain).await;
        let host_value = normalize_tool_args_for_projection(
            encoded,
            &ToolArgumentProjectionPolicy::MaterializeProjectedValues,
        )
        .expect("escaped plain record should decode");
        let recovered = json_to_flow_value(host_value);

        assert_eq!(
            recovered, plain,
            "plain reserved-key records must survive lashlang-to-host-to-lashlang"
        );
    }

    #[tokio::test]
    async fn reserved_projection_key_survives_in_plain_seed_data() {
        let plain = FlowValue::Record(Arc::new(FlowRecord::from_iter([(
            PROJECTED_JSON_TAG.to_string(),
            FlowValue::String("plain seed data".into()),
        )])));
        let seed = FlowValue::Record(Arc::new(FlowRecord::from_iter([(
            "data".to_string(),
            plain,
        )])));
        let args = FlowRecord::from_iter([("seed".to_string(), seed)]);

        let host_args = flow_record_to_tool_args(
            &args,
            &ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        )
        .await
        .expect("escaped seed data should decode");
        let seed = RlmSeed::from_tool_args(&host_args).expect("seed should classify");

        assert_eq!(
            seed.globals.get("data"),
            Some(&serde_json::json!({ PROJECTED_JSON_TAG: "plain seed data" }))
        );
        assert!(seed.projected.is_empty());
    }

    #[test]
    fn non_canonical_projection_wrapper_errors_loudly() {
        let error = normalize_tool_args_for_projection(
            serde_json::json!({
                PROJECTED_JSON_TAG: {
                    "kind": "materialized",
                    "value": "forged",
                },
                "other": true,
            }),
            &ToolArgumentProjectionPolicy::MaterializeProjectedValues,
        )
        .expect_err("reserved key alongside another key must be rejected");

        assert_eq!(
            error.to_string(),
            "non-canonical `__projected__` wrapper: reserved key must be the only object key"
        );
    }
}
