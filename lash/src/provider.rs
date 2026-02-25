use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::llm::factory::adapter_for;
use crate::oauth::{self, OAuthError};

fn default_base_url() -> String {
    "https://openrouter.ai/api/v1".to_string()
}

/// User-overridable model names for agent_call intelligence tiers.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AgentModels {
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "quick")]
    pub low: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "balanced")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "thorough")]
    pub high: Option<String>,
}

/// Auxiliary service secrets that are independent of LLM provider auth.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AuxiliarySecrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tavily_api_key: Option<String>,
}

impl AuxiliarySecrets {
    fn is_empty(&self) -> bool {
        self.tavily_api_key.is_none()
    }
}

/// Stored configuration: provider credentials + service API keys.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LashConfig {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "AuxiliarySecrets::is_empty")]
    pub auxiliary_secrets: AuxiliarySecrets,
    /// Legacy field (deserialize-only). Migrated into `auxiliary_secrets`.
    #[serde(default, skip_serializing, rename = "tavily_api_key")]
    legacy_tavily_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_models: Option<AgentModels>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Provider {
    OpenRouter {
        api_key: String,
        #[serde(default = "default_base_url")]
        base_url: String,
    },
    Claude {
        access_token: String,
        refresh_token: String,
        expires_at: u64,
    },
    Codex {
        access_token: String,
        refresh_token: String,
        expires_at: u64,
        account_id: Option<String>,
    },
    GoogleOAuth {
        access_token: String,
        refresh_token: String,
        expires_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
}

impl Provider {
    /// Default model for this provider.
    pub fn default_model(&self) -> &str {
        adapter_for(self).default_root_model()
    }

    /// Recommended reasoning effort for a specific model on this provider.
    pub fn reasoning_effort_for_model(&self, model: &str) -> Option<&str> {
        adapter_for(self).reasoning_effort_for_model(model)
    }

    /// Built-in model for an agent intelligence tier. Returns (model_name, optional_reasoning_effort).
    pub fn default_agent_model(&self, tier: &str) -> Option<(&str, Option<&str>)> {
        adapter_for(self)
            .default_agent_model(tier)
            .map(|m| (m.model, m.reasoning_effort))
    }

    /// Resolve model name: strip "anthropic/" prefix for direct Claude API.
    pub fn resolve_model(&self, model: &str) -> String {
        adapter_for(self).normalize_model(model)
    }

    /// Canonical model ID to use for context-window lookup.
    pub fn context_lookup_model(&self, model: &str) -> String {
        adapter_for(self).context_lookup_model(model)
    }

    /// Context window for a model under this provider.
    pub fn context_window(&self, model: &str) -> Option<u64> {
        let lookup = self.context_lookup_model(model);
        crate::model_info::context_window(&lookup).or_else(|| {
            if lookup != model {
                crate::model_info::context_window(model)
            } else {
                None
            }
        })
    }

    /// Validate model syntax and local catalog availability for this provider.
    pub fn validate_model(&self, model: &str) -> Result<(), String> {
        let m = model.trim();
        if m.is_empty() {
            return Err("model cannot be empty".to_string());
        }
        if m.contains(char::is_whitespace) {
            return Err("model cannot contain whitespace".to_string());
        }
        let allow_unknown = std::env::var("LASH_ALLOW_UNKNOWN_MODELS").ok().as_deref() == Some("1");
        if self.context_window(m).is_none() && !allow_unknown {
            return Err(format!(
                "model `{}` is not in local catalog for this provider (set LASH_ALLOW_UNKNOWN_MODELS=1 to bypass)",
                m
            ));
        }
        Ok(())
    }

    /// Refresh OAuth tokens if needed. No-op for OpenRouter.
    /// Returns `true` if tokens were updated (caller should persist).
    pub async fn ensure_fresh(&mut self) -> Result<bool, OAuthError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        match self {
            Provider::Claude {
                access_token,
                refresh_token,
                expires_at,
            } => {
                if now + 300 >= *expires_at {
                    let tokens = oauth::refresh_tokens(refresh_token).await?;
                    *access_token = tokens.access_token;
                    *refresh_token = tokens.refresh_token;
                    *expires_at = tokens.expires_at;
                    return Ok(true);
                }
            }
            Provider::Codex {
                access_token,
                refresh_token,
                expires_at,
                ..
            } => {
                if now + 300 >= *expires_at {
                    let tokens = oauth::codex_refresh_tokens(refresh_token).await?;
                    *access_token = tokens.access_token;
                    *refresh_token = tokens.refresh_token;
                    *expires_at = tokens.expires_at;
                    return Ok(true);
                }
            }
            Provider::GoogleOAuth {
                access_token,
                refresh_token,
                expires_at,
                ..
            } => {
                if now + 300 >= *expires_at {
                    let tokens = oauth::google_refresh_tokens(refresh_token).await?;
                    *access_token = tokens.access_token;
                    *refresh_token = tokens.refresh_token;
                    *expires_at = tokens.expires_at;
                    return Ok(true);
                }
            }
            Provider::OpenRouter { .. } => {}
        }
        Ok(false)
    }
}

impl LashConfig {
    fn migrate_legacy_fields(&mut self) {
        if self.auxiliary_secrets.tavily_api_key.is_none() && self.legacy_tavily_api_key.is_some() {
            self.auxiliary_secrets.tavily_api_key = self.legacy_tavily_api_key.take();
        }
    }

    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            auxiliary_secrets: AuxiliarySecrets::default(),
            legacy_tavily_api_key: None,
            agent_models: None,
        }
    }

    fn config_path() -> PathBuf {
        crate::lash_home().join("config.json")
    }

    /// Load from ~/.lash/config.json.
    pub fn load() -> Option<Self> {
        let path = Self::config_path();
        if let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(mut config) = serde_json::from_str::<Self>(&data)
        {
            config.migrate_legacy_fields();
            return Some(config);
        }

        None
    }

    /// Save to ~/.lash/config.json (mode 0o600)
    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, &data)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    pub fn tavily_api_key(&self) -> Option<&str> {
        self.auxiliary_secrets.tavily_api_key.as_deref()
    }

    pub fn set_tavily_api_key(&mut self, key: Option<String>) {
        self.auxiliary_secrets.tavily_api_key = key;
    }

    /// Delete ~/.lash/config.json
    pub fn clear() -> Result<(), std::io::Error> {
        let path = Self::config_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

/// Save just the provider portion (preserves other config fields like API keys).
/// Used by the agent loop after token refresh.
pub fn save_provider(provider: &Provider) -> Result<(), std::io::Error> {
    let mut config = LashConfig::load().unwrap_or_else(|| LashConfig {
        provider: provider.clone(),
        auxiliary_secrets: AuxiliarySecrets::default(),
        legacy_tavily_api_key: None,
        agent_models: None,
    });
    config.provider = provider.clone();
    config.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openrouter() -> Provider {
        Provider::OpenRouter {
            api_key: "test-key".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
        }
    }

    fn claude() -> Provider {
        Provider::Claude {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: u64::MAX,
        }
    }

    fn codex() -> Provider {
        Provider::Codex {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: u64::MAX,
            account_id: Some("acct".into()),
        }
    }

    fn google_oauth() -> Provider {
        Provider::GoogleOAuth {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: u64::MAX,
            project_id: Some("test-proj".into()),
        }
    }

    #[test]
    fn default_model() {
        assert_eq!(openrouter().default_model(), "anthropic/claude-sonnet-4.6");
        assert_eq!(claude().default_model(), "claude-opus-4-6");
        assert_eq!(codex().default_model(), "gpt-5.3-codex");
        assert_eq!(google_oauth().default_model(), "gemini-3.1-pro-preview");
    }

    #[test]
    fn reasoning_effort_for_model() {
        assert_eq!(
            codex().reasoning_effort_for_model("gpt-5.3-codex"),
            Some("high")
        );
        assert_eq!(codex().reasoning_effort_for_model("gpt-5.1-codex"), None);
        assert_eq!(
            openrouter().reasoning_effort_for_model("anthropic/claude-sonnet-4.6"),
            None
        );
    }

    #[test]
    fn default_agent_model_claude() {
        let p = claude();
        assert_eq!(
            p.default_agent_model("low"),
            Some(("claude-haiku-4-5", None))
        );
        assert_eq!(
            p.default_agent_model("medium"),
            Some(("claude-sonnet-4-6", None))
        );
        assert_eq!(
            p.default_agent_model("high"),
            Some(("claude-sonnet-4-6", None))
        );
    }

    #[test]
    fn default_agent_model_openrouter() {
        let p = openrouter();
        assert!(p.default_agent_model("low").is_some());
        assert!(p.default_agent_model("medium").is_some());
        assert!(p.default_agent_model("high").is_some());
    }

    #[test]
    fn default_agent_model_codex() {
        let p = codex();
        let (m, re) = p.default_agent_model("low").unwrap();
        assert_eq!(m, "gpt-5.3-codex-spark");
        assert_eq!(re, None);
        let (m, re) = p.default_agent_model("medium").unwrap();
        assert_eq!(m, "gpt-5.3-codex");
        assert_eq!(re, Some("medium"));
        let (m, re) = p.default_agent_model("high").unwrap();
        assert_eq!(m, "gpt-5.3-codex");
        assert_eq!(re, Some("high"));
    }

    #[test]
    fn default_agent_model_unknown_tier() {
        assert!(claude().default_agent_model("unknown").is_none());
        assert!(openrouter().default_agent_model("").is_none());
        assert!(codex().default_agent_model("extreme").is_none());
        assert!(google_oauth().default_agent_model("extreme").is_none());
    }

    #[test]
    fn resolve_model_claude_strips_prefix() {
        let p = claude();
        assert_eq!(
            p.resolve_model("anthropic/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(p.resolve_model("claude-sonnet-4-6"), "claude-sonnet-4-6");
    }

    #[test]
    fn resolve_model_openrouter_passthrough() {
        let p = openrouter();
        assert_eq!(
            p.resolve_model("anthropic/claude-sonnet-4.6"),
            "anthropic/claude-sonnet-4.6"
        );
    }

    #[test]
    fn resolve_model_codex_passthrough() {
        let p = codex();
        assert_eq!(p.resolve_model("gpt-5.1-codex"), "gpt-5.1-codex");
    }

    #[test]
    fn resolve_model_google_passthrough() {
        let p = google_oauth();
        assert_eq!(
            p.resolve_model("gemini-3-pro-preview"),
            "gemini-3-pro-preview"
        );
    }

    #[test]
    fn context_lookup_model_claude_adds_prefix() {
        let p = claude();
        assert_eq!(
            p.context_lookup_model("claude-opus-4-6"),
            "anthropic/claude-opus-4-6"
        );
        assert_eq!(
            p.context_lookup_model("anthropic/claude-opus-4-6"),
            "anthropic/claude-opus-4-6"
        );
    }

    #[test]
    fn context_lookup_model_google_adds_prefix() {
        let p = google_oauth();
        assert_eq!(
            p.context_lookup_model("gemini-3-pro-preview"),
            "google/gemini-3-pro-preview"
        );
        assert_eq!(
            p.context_lookup_model("google/gemini-3-pro-preview"),
            "google/gemini-3-pro-preview"
        );
    }

    #[test]
    fn context_lookup_model_codex_adds_prefix() {
        let p = codex();
        assert_eq!(
            p.context_lookup_model("gpt-5.3-codex"),
            "openai/gpt-5.3-codex"
        );
        assert_eq!(
            p.context_lookup_model("openai/gpt-5.3-codex"),
            "openai/gpt-5.3-codex"
        );
    }

    #[test]
    fn default_agent_model_google_oauth_tiers() {
        let p = google_oauth();
        assert_eq!(
            p.default_agent_model("low"),
            Some(("gemini-3-flash-preview", None))
        );
        assert_eq!(
            p.default_agent_model("medium"),
            Some(("gemini-3.1-pro-preview", None))
        );
        assert_eq!(
            p.default_agent_model("high"),
            Some(("gemini-3.1-pro-preview", None))
        );
    }

    #[test]
    fn validate_model_rejects_empty_and_whitespace() {
        let p = codex();
        assert!(p.validate_model("").is_err());
        assert!(p.validate_model("   ").is_err());
        assert!(p.validate_model("gpt 5.3").is_err());
    }

    #[test]
    fn validate_model_accepts_known_default_model() {
        let p = codex();
        assert!(p.validate_model(p.default_model()).is_ok());
    }

    #[test]
    fn validate_model_rejects_unknown_model() {
        let p = openrouter();
        assert!(
            p.validate_model("this-model-does-not-exist-xyz-123")
                .is_err()
        );
    }

    #[test]
    fn legacy_tavily_field_migrates_to_auxiliary_secrets() {
        let raw = serde_json::json!({
            "provider": {
                "type": "OpenRouter",
                "api_key": "k",
                "base_url": "https://openrouter.ai/api/v1"
            },
            "tavily_api_key": "legacy-key"
        });

        let mut cfg: LashConfig = serde_json::from_value(raw).expect("valid config json");
        cfg.migrate_legacy_fields();
        assert_eq!(cfg.tavily_api_key(), Some("legacy-key"));
    }

    #[test]
    fn modern_auxiliary_secrets_preserved() {
        let raw = serde_json::json!({
            "provider": {
                "type": "OpenRouter",
                "api_key": "k",
                "base_url": "https://openrouter.ai/api/v1"
            },
            "auxiliary_secrets": {
                "tavily_api_key": "new-key"
            },
            "tavily_api_key": "legacy-key"
        });

        let mut cfg: LashConfig = serde_json::from_value(raw).expect("valid config json");
        cfg.migrate_legacy_fields();
        assert_eq!(cfg.tavily_api_key(), Some("new-key"));
    }
}
