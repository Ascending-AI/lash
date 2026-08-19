//! Codex (OpenAI) device-code OAuth flow + token refresh. Public so
//! Host Applications can drive the interactive login.

use base64::Engine;

use lash_provider_auth::{OAuthError, now_secs, url_form_encode};

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const CODEX_DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const CODEX_DEVICE_CALLBACK: &str = "https://auth.openai.com/deviceauth/callback";

/// URL to show the user during interactive login. Setup UIs open this
/// in the browser and then poll `poll_device_auth`.
pub const CODEX_DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";

fn codex_user_agent() -> String {
    format!(
        "lash/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[derive(Debug)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

#[derive(Debug)]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: Option<String>,
}

/// Request a device code from OpenAI for the Codex auth flow.
pub async fn request_device_code() -> Result<DeviceCode, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_DEVICE_CODE_URL)
        .header("User-Agent", codex_user_agent())
        .json(&serde_json::json!({ "client_id": CODEX_CLIENT_ID }))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error"]
            .as_str()
            .unwrap_or("failed to initiate device authorization");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    Ok(DeviceCode {
        device_auth_id: body["device_auth_id"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing device_auth_id".into()))?
            .to_string(),
        user_code: body["user_code"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing user_code".into()))?
            .to_string(),
        interval: body["interval"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or(body["interval"].as_u64())
            .map(|v| v.max(1))
            .unwrap_or(5),
    })
}

/// Poll the device auth endpoint. Returns `Ok(Some((auth_code,
/// code_verifier)))` when approved, `Ok(None)` when still pending,
/// `Err` on failure.
pub async fn poll_device_auth(
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<(String, String)>, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_DEVICE_POLL_URL)
        .header("User-Agent", codex_user_agent())
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .await?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await?;
        let auth_code = body["authorization_code"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing authorization_code".into()))?
            .to_string();
        let code_verifier = body["code_verifier"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing code_verifier".into()))?
            .to_string();
        Ok(Some((auth_code, code_verifier)))
    } else if resp.status().as_u16() == 403 || resp.status().as_u16() == 404 {
        Ok(None)
    } else {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let err = body["error"]
            .as_str()
            .unwrap_or("device auth polling failed");
        Err(OAuthError::TokenExchange(err.to_string()))
    }
}

/// Exchange the device authorization code for tokens. Uses
/// form-urlencoded as required by OpenAI's token endpoint.
pub async fn exchange_code(code: &str, code_verifier: &str) -> Result<CodexTokens, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(url_form_encode(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", CODEX_DEVICE_CALLBACK),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", code_verifier),
        ]))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("token exchange failed");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
        .to_string();
    let refresh_token = body["refresh_token"]
        .as_str()
        .ok_or_else(|| OAuthError::TokenExchange("missing refresh_token".into()))?
        .to_string();

    let account_id = body["id_token"]
        .as_str()
        .and_then(extract_account_id)
        .or_else(|| extract_account_id(&access_token));

    Ok(CodexTokens {
        access_token,
        refresh_token,
        expires_at: now + expires_in,
        account_id,
    })
}

/// Refresh Codex OAuth tokens.
pub async fn refresh_tokens(refresh: &str) -> Result<CodexTokens, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(url_form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CODEX_CLIENT_ID),
        ]))
        .send()
        .await?;

    let status = resp.status();
    let response_body = resp.text().await?;

    if !status.is_success() {
        return Err(OAuthError::token_endpoint(
            status.as_u16(),
            &response_body,
            "token refresh failed",
        ));
    }
    let body: serde_json::Value = serde_json::from_str(&response_body)?;

    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
        .to_string();
    let refresh_token = body["refresh_token"]
        .as_str()
        .unwrap_or(refresh)
        .to_string();
    let account_id = body["id_token"]
        .as_str()
        .and_then(extract_account_id)
        .or_else(|| extract_account_id(&access_token));

    Ok(CodexTokens {
        access_token,
        refresh_token,
        expires_at: now + expires_in,
        account_id,
    })
}

/// Extract the ChatGPT account ID from a JWT token (no crypto
/// verification needed — we're only reading claims on a token we
/// just received from OpenAI's token endpoint).
fn extract_account_id(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    if let Some(id) = claims["chatgpt_account_id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    if let Some(id) = claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    if let Some(orgs) = claims["organizations"].as_array()
        && let Some(org) = orgs.first()
        && let Some(id) = org["id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    None
}
