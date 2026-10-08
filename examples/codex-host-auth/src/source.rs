//! The host's [`TokenSource`] for a stored ChatGPT login.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use lash::provider::{
    ProviderToken, TokenError, TokenErrorKind, TokenRequest, TokenRequestReason, TokenSource,
};
use serde::{Deserialize, Serialize};

use crate::{CODEX_CLIENT_ID, CODEX_TOKEN_URL, OAUTH_ENDPOINT_TIMEOUT, now_secs, stored_login};

/// How long before expiry the host refreshes on its own.
const REFRESH_BEFORE_SECS: u64 = 5 * 60;

/// The ChatGPT login the host persists. The host's file is the only copy of
/// the refresh token.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredLogin {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for StoredLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredLogin")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl StoredLogin {
    fn token(&self) -> ProviderToken {
        let token = ProviderToken::new(self.access_token.clone())
            .expiring_at(UNIX_EPOCH + Duration::from_secs(self.expires_at));
        match &self.account_id {
            Some(account) => token.with_account(account.clone()),
            None => token,
        }
    }
}

/// A [`TokenSource`] over a login file. One refresh runs at a time per
/// process, and a rotation is written to the file before lash sees it.
#[derive(Debug)]
pub struct CodexHostAuth {
    path: PathBuf,
    http: reqwest::Client,
    state: tokio::sync::Mutex<Option<Held>>,
}

/// The login in memory, and whether the file holds it yet.
#[derive(Debug)]
struct Held {
    login: StoredLogin,
    persisted: bool,
}

impl CodexHostAuth {
    /// The login stored at `path`, read on the first ask.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            http: reqwest::Client::new(),
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// Store `login` at `path`, as [`crate::login`] does after sign-in.
    pub async fn save(path: &Path, login: &StoredLogin) -> anyhow::Result<()> {
        let staged = path.with_extension("tmp");
        tokio::fs::write(&staged, serde_json::to_vec_pretty(login)?).await?;
        tokio::fs::rename(&staged, path).await?;
        Ok(())
    }

    async fn load(&self) -> Result<StoredLogin, TokenError> {
        let bytes = tokio::fs::read(&self.path).await.map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                TokenErrorKind::ReauthRequired
            } else {
                TokenErrorKind::Transient { retry_after: None }
            };
            TokenError::new(kind, "the ChatGPT login file cannot be read")
        })?;
        serde_json::from_slice(&bytes).map_err(|_| {
            TokenError::new(
                TokenErrorKind::ReauthRequired,
                "the ChatGPT login file is not a login",
            )
        })
    }

    async fn refresh(&self, current: &StoredLogin) -> Result<StoredLogin, TokenError> {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "refresh_token"),
                ("refresh_token", current.refresh_token.as_str()),
                ("client_id", CODEX_CLIENT_ID),
            ])
            .finish();
        let transient = |message: &str| {
            TokenError::new(TokenErrorKind::Transient { retry_after: None }, message)
        };
        let response = self
            .http
            .post(CODEX_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .timeout(OAUTH_ENDPOINT_TIMEOUT)
            .send()
            .await
            .map_err(|_| transient("the ChatGPT token endpoint is unreachable"))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|_| transient("the ChatGPT token endpoint response was cut off"))?;
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if !(200..300).contains(&status) {
            let code = body["error"]["code"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or_default();
            return Err(match (status, code) {
                (_, "invalid_grant" | "refresh_token_reused" | "refresh_token_expired") => {
                    TokenError::new(TokenErrorKind::ReauthRequired, "sign in to ChatGPT again")
                }
                (408 | 429 | 500..=599, _) => transient("the ChatGPT token endpoint is busy"),
                _ => TokenError::new(TokenErrorKind::Unavailable, "the ChatGPT refresh failed"),
            });
        }
        let mut next = stored_login(&body, Some(&current.refresh_token)).map_err(|_| {
            TokenError::new(
                TokenErrorKind::Unavailable,
                "the ChatGPT refresh was malformed",
            )
        })?;
        next.account_id = next.account_id.or_else(|| current.account_id.clone());
        Ok(next)
    }
}

#[async_trait::async_trait]
impl TokenSource for CodexHostAuth {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        let mut state = self.state.lock().await;
        let held = match state.take() {
            Some(held) => held,
            None => Held {
                login: self.load().await?,
                persisted: true,
            },
        };
        let held = state.insert(held);
        // Compare-and-refresh: a rejected or expiring token that is no longer
        // the current one was already replaced, by this process or another.
        let still_current = request
            .stale
            .is_none_or(|stale| stale.secret().expose_secret() == held.login.access_token);
        let must_refresh = match request.reason {
            TokenRequestReason::Current => {
                held.login.expires_at <= now_secs() + REFRESH_BEFORE_SECS
            }
            _ => still_current,
        };
        if must_refresh {
            held.login = self.refresh(&held.login).await?;
            held.persisted = false;
        }
        // The old refresh token is dead once the endpoint answered, so a
        // rotation reaches the file before any caller sees its token.
        if !held.persisted {
            Self::save(&self.path, &held.login).await.map_err(|_| {
                TokenError::new(
                    TokenErrorKind::Transient { retry_after: None },
                    "the rotated ChatGPT login could not be stored",
                )
            })?;
            held.persisted = true;
        }
        Ok(held.login.token())
    }
}
