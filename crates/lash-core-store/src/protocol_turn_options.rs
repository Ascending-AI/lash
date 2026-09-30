//! The session's protocol turn options: a view of the protocol plugin's
//! recorded configuration namespace (FIG-4379).
//!
//! The protocol's namespace is recorded in the session's
//! [`PluginConfig`](crate::PluginConfig) like every other owner's; this type
//! is how protocol code reads it, derived wherever it is consumed and never
//! stored beside it. It serializes as the bare namespace value, which is how
//! a run's overrides carry a change to it.

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ProtocolTurnOptions {
    pub payload: serde_json::Value,
}

impl Default for ProtocolTurnOptions {
    fn default() -> Self {
        Self::empty()
    }
}
impl ProtocolTurnOptions {
    pub const RENDER_OPTIONS_KEY: &'static str = "render";

    pub fn empty() -> Self {
        Self::from_payload(serde_json::Value::Object(serde_json::Map::new()))
    }

    pub fn from_payload(payload: serde_json::Value) -> Self {
        Self { payload }
    }

    /// Reports empty only for an empty JSON object so protocol implementors do not confuse scalar,
    /// list, or null payloads with absent options.
    pub fn is_empty(&self) -> bool {
        match &self.payload {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    /// Serializes typed protocol options for protocol implementors resolving
    /// their namespace.
    pub fn typed<T>(value: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self::from_payload(serde_json::to_value(value)?))
    }

    pub fn decode<T>(&self) -> Result<T, ProtocolTurnOptionsError>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.payload.clone()).map_err(ProtocolTurnOptionsError::Decode)
    }
}
impl facade_ops::ProtocolTurnOptionsFacadeOps for ProtocolTurnOptions {
    fn merged_with_override(&self, override_options: &Self) -> Self {
        self.merged_with(override_options)
    }
}
impl ProtocolTurnOptions {
    /// `override_options` over `self`, key by key when both are objects;
    /// otherwise the override replaces.
    pub(crate) fn merged_with(&self, override_options: &Self) -> Self {
        match (&self.payload, &override_options.payload) {
            (serde_json::Value::Object(base), serde_json::Value::Object(overrides)) => {
                let mut payload = base.clone();
                for (key, value) in overrides {
                    if key == Self::RENDER_OPTIONS_KEY {
                        let current = payload
                            .entry(key.clone())
                            .or_insert(serde_json::Value::Null);
                        merge_render_options(current, value);
                    } else {
                        payload.insert(key.clone(), value.clone());
                    }
                }
                Self {
                    payload: serde_json::Value::Object(payload),
                }
            }
            _ => override_options.clone(),
        }
    }
}

fn merge_render_options(base: &mut serde_json::Value, override_value: &serde_json::Value) {
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), override_value.as_object()) {
        for (key, value) in overrides {
            merge_render_options(
                base.entry(key.clone()).or_insert(serde_json::Value::Null),
                value,
            );
        }
    } else {
        *base = override_value.clone();
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolTurnOptionsError {
    #[error("failed to decode protocol turn options payload: {0}")]
    Decode(#[source] serde_json::Error),
}

pub mod facade_ops {

    /// Facade-internal operations for [`ProtocolTurnOptions`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ProtocolTurnOptionsFacadeOps {
        fn merged_with_override(&self, override_options: &Self) -> Self;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_layers_merge_recursively_without_changing_other_option_keys() {
        let session = ProtocolTurnOptions::from_payload(serde_json::json!({
            "mode": {"left": 1},
            "render": {
                "print": {"max_chars": 8000, "layout": "auto"},
                "per_tool": {"tool:a": {"value": {"max_depth": 4}}}
            }
        }));
        let turn = ProtocolTurnOptions::from_payload(serde_json::json!({
            "mode": {"right": 2},
            "render": {
                "print": {"layout": "compact"},
                "per_tool": {"tool:a": {"value": {"max_chars": 200}}, "tool:b": {"max_lines": 8}}
            }
        }));
        let merged = session.merged_with(&turn);
        assert_eq!(
            merged.payload,
            serde_json::json!({
                "mode": {"right": 2},
                "render": {
                    "print": {"max_chars": 8000, "layout": "compact"},
                    "per_tool": {
                        "tool:a": {"value": {"max_depth": 4, "max_chars": 200}},
                        "tool:b": {"max_lines": 8}
                    }
                }
            })
        );
        let reset = ProtocolTurnOptions::from_payload(serde_json::json!({
            "render": {"print": {"max_chars": null}}
        }));
        assert_eq!(
            merged.merged_with(&reset).payload["render"]["print"]["max_chars"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn options_serialize_as_the_bare_namespace_value() {
        let options = ProtocolTurnOptions::from_payload(serde_json::json!({ "mode": "test" }));
        let encoded = serde_json::to_string(&options).expect("serialize options");
        assert_eq!(encoded, r#"{"mode":"test"}"#);
        let round_tripped: ProtocolTurnOptions =
            serde_json::from_str(&encoded).expect("deserialize roundtrip");
        assert_eq!(round_tripped, options);
    }
}
