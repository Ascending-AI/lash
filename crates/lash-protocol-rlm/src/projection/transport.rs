use lash_core::{SessionAppendNode, ToolArgumentProjectionPolicy};
use lash_rlm_types::{CodeModeProjectedSeedEntry, PROJECTED_JSON_TAG};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectionTransportError {
    #[error("non-canonical `{PROJECTED_JSON_TAG}` wrapper: {reason}")]
    NonCanonicalWrapper { reason: String },
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RlmSeed {
    pub projected: lash_rlm_types::CodeModeProjectedSeedSnapshot,
    pub globals: serde_json::Map<String, Value>,
    /// The saved functions the session is created with, by the binding
    /// each is called through: each value is a kernel saved function
    /// (`lash_kernel_dialect::SavedFunction`) as JSON. Each is checked
    /// against what the new session offers when a cell first uses it.
    pub functions: serde_json::Map<String, Value>,
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
                let CodeModeProjectedSeedEntry::Materialized(value) = entry;
                out.projected.push(
                    name,
                    CodeModeProjectedSeedEntry::Materialized(decode_escaped_json(value)?),
                );
            } else {
                out.globals
                    .insert(name, decode_escaped_json(value.clone())?);
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.projected.is_empty() && self.functions.is_empty()
    }

    pub fn into_event_body(self) -> lash_rlm_types::RlmSeedPluginBody {
        lash_rlm_types::RlmSeedPluginBody {
            globals: self.globals,
            projected: self.projected,
            functions: self.functions,
        }
    }
}

pub fn rlm_seed_initial_nodes(
    seed: RlmSeed,
    fleet: lash_core::FleetFormat,
) -> Vec<SessionAppendNode> {
    if seed.is_empty() {
        return Vec::new();
    }
    vec![SessionAppendNode::protocol_event(
        super::context::rlm_protocol_event(
            lash_rlm_types::RlmProtocolEvent::RlmSeed(seed.into_event_body()),
            fleet.writer_version(lash_core::surface_format!(
                crate::RLM_PROTOCOL_EVENT_VERSION
            )),
        ),
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
            let CodeModeProjectedSeedEntry::Materialized(value) = entry;
            projected_wrapper(CodeModeProjectedSeedEntry::Materialized(
                materialize_projected_json_preserving_escapes(value)?,
            ))
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
        let CodeModeProjectedSeedEntry::Materialized(value) = entry;
        return materialize_projected_json_with_keys(value, key_mode);
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
) -> Result<Option<CodeModeProjectedSeedEntry>, ProjectionTransportError> {
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

fn projected_wrapper(entry: CodeModeProjectedSeedEntry) -> Value {
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

/// A value a cell computed, as the tool-argument transport carries plain
/// data: every record key in the reserved prefix is escaped, so nothing a
/// cell writes reads as a projected-value wrapper on the host side.
pub(crate) fn plain_json_for_transport(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(plain_json_for_transport).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (escape_projected_key(&key), plain_json_for_transport(value)))
                .collect(),
        ),
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_projection_key_round_trips_as_plain_record() {
        let already_prefixed = format!("{PROJECTED_JSON_TAG}{PROJECTED_JSON_TAG}");
        let plain = serde_json::json!({
            PROJECTED_JSON_TAG: "plain data",
            already_prefixed: "also plain data",
        });

        let host_value = normalize_tool_args_for_projection(
            plain_json_for_transport(plain.clone()),
            &ToolArgumentProjectionPolicy::MaterializeProjectedValues,
        )
        .expect("escaped plain record should decode");

        assert_eq!(
            host_value, plain,
            "plain reserved-key records must survive cell-to-host transport"
        );
    }

    #[test]
    fn reserved_projection_key_survives_in_plain_seed_data() {
        let args = serde_json::json!({
            "seed": { "data": { PROJECTED_JSON_TAG: "plain seed data" } },
        });

        let host_args = normalize_tool_args_for_projection(
            plain_json_for_transport(args),
            &ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        )
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
