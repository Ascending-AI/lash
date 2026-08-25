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
            wire: OpenAiWireConfig::default(),
            transport: DEFAULT_HTTP_TRANSPORT.clone(),
        }
        .into_components())
    }
}
