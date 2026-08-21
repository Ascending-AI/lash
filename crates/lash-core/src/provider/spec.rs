use super::support::*;

/// Serialised form of a provider's host-owned configuration. Runtime
/// persistence stores only provider ids; this type is for app/user config
/// files that need credentials.
///
/// Wire shape is a flat JSON object: a `type` field plus the
/// provider-specific config keys, suitable for host-owned application config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSpec {
    pub kind: String,
    pub config: serde_json::Value,
}

impl Serialize for ProviderSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = match &self.config {
            serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
            serde_json::Value::Null => serde_json::Value::Object(serde_json::Map::new()),
            other => {
                return Err(serde::ser::Error::custom(format!(
                    "ProviderSpec.config must serialize to a JSON object, got {}",
                    other
                )));
            }
        };
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert(
                "type".to_string(),
                serde_json::Value::String(self.kind.clone()),
            );
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProviderSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let kind = if let serde_json::Value::Object(ref mut map) = value {
            let raw = map
                .remove("type")
                .ok_or_else(|| serde::de::Error::missing_field("type"))?;
            raw.as_str()
                .ok_or_else(|| serde::de::Error::custom("provider `type` must be a string"))?
                .to_string()
        } else {
            return Err(serde::de::Error::custom(
                "provider spec must be a JSON object",
            ));
        };
        Ok(Self {
            kind,
            config: value,
        })
    }
}
