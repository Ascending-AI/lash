use crate::support::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiCompatibleProviderConfig {
    api_key: String,
    base_url: String,
    #[serde(default)]
    options: ProviderOptions,
    #[serde(default)]
    compat: OpenAiCompat,
    #[serde(default)]
    wire: OpenAiWireConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiProviderConfig {
    api_key: String,
    #[serde(default)]
    options: ProviderOptions,
}

pub struct OpenAiCompatibleProviderFactory;
pub struct OpenAiProviderFactory;

impl ProviderFactory for OpenAiProviderFactory {
    fn kind(&self) -> &'static str {
        "openai"
    }

    fn deserialize(&self, config: serde_json::Value) -> Result<ProviderComponents, String> {
        let cfg: OpenAiProviderConfig =
            serde_json::from_value(config).map_err(|err| err.to_string())?;
        Ok(OpenAiProvider {
            inner: OpenAiCompatibleProvider {
                api_key: cfg.api_key,
                base_url: OPENAI_BASE_URL.to_string(),
                options: cfg.options,
                compat: OpenAiCompat {
                    prompt_cache_key: Some(true),
                    prompt_cache_retention: Some(true),
                    ..OpenAiCompat::default()
                },
                wire: OpenAiWireConfig::default(),
                transport: DEFAULT_HTTP_TRANSPORT.clone(),
            },
        }
        .into_components())
    }
}

impl ProviderFactory for OpenAiCompatibleProviderFactory {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn deserialize(&self, config: serde_json::Value) -> Result<ProviderComponents, String> {
        let cfg: OpenAiCompatibleProviderConfig =
            serde_json::from_value(config).map_err(|err| err.to_string())?;
        Ok(OpenAiCompatibleProvider {
            api_key: cfg.api_key,
            base_url: cfg.base_url,
            options: cfg.options,
            compat: cfg.compat,
            wire: cfg.wire,
            transport: DEFAULT_HTTP_TRANSPORT.clone(),
        }
        .into_components())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_provider_wire_config_round_trips_through_factory() {
        let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
            .with_wire_config(OpenAiWireConfig {
                auth_header_name: "api-key".to_string(),
                auth_value_prefix: "Token ".to_string(),
                query_params: vec![("api-version".to_string(), "2026-08-25".to_string())],
            });
        let config = provider.serialize_config();

        let round_trip = OpenAiCompatibleProviderFactory
            .deserialize(config.clone())
            .expect("non-default wire config round trip");

        assert_eq!(round_trip.provider.serialize_config(), config);
    }
}
