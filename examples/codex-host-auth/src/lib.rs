//! A host-owned ChatGPT login for lash's Codex provider.
//!
//! Lash asks a [`TokenSource`](lash::provider::TokenSource) for a token before
//! every model-call attempt. [`CodexHostAuth`] is the host's answer: it keeps
//! the refresh token, refreshes it with OpenAI, and persists the rotation
//! itself. [`login`] runs the device-code login that seeds its file.

mod login;
mod source;

pub use login::{DeviceCode, login};
pub use source::{CodexHostAuth, StoredLogin};

use std::sync::Arc;

/// The Codex provider over the host's stored login.
pub fn codex_provider(auth: Arc<CodexHostAuth>) -> lash::openai::CodexProvider {
    lash::openai::CodexProvider::new(auth)
}

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_ENDPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The tokens an OpenAI token-endpoint response carries. A refresh response
/// may omit the refresh token, which then stays `previous_refresh`.
fn stored_login(
    body: &serde_json::Value,
    previous_refresh: Option<&str>,
) -> anyhow::Result<StoredLogin> {
    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("token response has no access_token"))?
        .to_string();
    let refresh_token = body["refresh_token"]
        .as_str()
        .or(previous_refresh)
        .ok_or_else(|| anyhow::anyhow!("token response has no refresh_token"))?
        .to_string();
    let account_id = body["id_token"]
        .as_str()
        .and_then(account_id_claim)
        .or_else(|| account_id_claim(&access_token));
    Ok(StoredLogin {
        expires_at: now_secs() + body["expires_in"].as_u64().unwrap_or(3600),
        access_token,
        refresh_token,
        account_id,
    })
}

/// The ChatGPT account id claim of a JWT the token endpoint just issued
/// (read, not verified).
fn account_id_claim(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    [
        &claims["chatgpt_account_id"],
        &claims["https://api.openai.com/auth"]["chatgpt_account_id"],
        &claims["organizations"][0]["id"],
    ]
    .into_iter()
    .find_map(|id| id.as_str().filter(|id| !id.is_empty()).map(str::to_string))
}
