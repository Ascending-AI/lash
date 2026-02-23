use base64::Engine;
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

// ── Google OAuth (Gemini API via OAuth bearer) ────────────────────

const GOOGLE_CLIENT_ID_ENV: &str = "LASH_GOOGLE_CLIENT_ID";
const GOOGLE_CLIENT_SECRET_ENV: &str = "LASH_GOOGLE_CLIENT_SECRET";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";
const GOOGLE_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_PROMPT: &str = "consent select_account";

#[derive(Debug)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Token exchange failed: {0}")]
    TokenExchange(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Generate a PKCE code verifier and challenge pair.
pub fn generate_pkce() -> (String, String) {
    // PKCE verifier must come from a cryptographically secure RNG.
    // UUID v4 generation uses OS entropy; two UUIDs provide 32 random bytes.
    let mut verifier_bytes = Vec::with_capacity(32);
    verifier_bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    verifier_bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    (verifier, challenge)
}

/// Build the authorization URL for the browser.
/// The `verifier` is embedded in the `state` param (matches opencode's convention).
pub fn authorize_url(challenge: &str, verifier: &str) -> String {
    format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        AUTH_URL,
        CLIENT_ID,
        urlencoded(REDIRECT_URI),
        urlencoded(SCOPES),
        challenge,
        verifier,
    )
}

/// Build the Google OAuth authorization URL for manual code entry.
pub fn google_authorize_url(challenge: &str) -> Result<String, OAuthError> {
    let state = uuid::Uuid::new_v4().to_string();
    let (client_id, _) = google_client_credentials()?;
    Ok(format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&access_type=offline&prompt={}&code_challenge={}&code_challenge_method=S256&state={}",
        GOOGLE_AUTH_URL,
        client_id,
        urlencoded(GOOGLE_REDIRECT_URI),
        urlencoded(GOOGLE_SCOPES),
        urlencoded(GOOGLE_PROMPT),
        challenge,
        state,
    ))
}

/// Exchange an authorization code for tokens.
/// The pasted code may contain `code#state` — we split on `#`.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<OAuthTokens, OAuthError> {
    let parts: Vec<&str> = code.splitn(2, '#').collect();
    let auth_code = parts[0];
    let state = parts.get(1).copied().unwrap_or("");

    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": auth_code,
            "state": state,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        }))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("unknown error");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    parse_token_response(&body)
}

/// Refresh an expired access token.
pub async fn refresh_tokens(refresh: &str) -> Result<OAuthTokens, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh,
        }))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("unknown error");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    // Refresh response may not include a new refresh_token; keep the old one.
    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    Ok(OAuthTokens {
        access_token: body["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
            .to_string(),
        refresh_token: body["refresh_token"]
            .as_str()
            .unwrap_or(refresh)
            .to_string(),
        expires_at: now + expires_in,
    })
}

/// Exchange a Google OAuth authorization code for tokens.
/// Accepts either a raw code or a full redirect URL that contains `code=...`.
pub async fn google_exchange_code(code: &str, verifier: &str) -> Result<OAuthTokens, OAuthError> {
    let (client_id, client_secret) = google_client_credentials()?;
    let auth_code = extract_google_auth_code(code);
    let client = reqwest::Client::new();
    let resp = client
        .post(GOOGLE_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(url_form_encode(&[
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("redirect_uri", GOOGLE_REDIRECT_URI),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code_verifier", verifier),
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

    parse_token_response(&body)
}

/// Refresh Google OAuth tokens.
pub async fn google_refresh_tokens(refresh: &str) -> Result<OAuthTokens, OAuthError> {
    let (client_id, client_secret) = google_client_credentials()?;
    let client = reqwest::Client::new();
    let resp = client
        .post(GOOGLE_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(url_form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ]))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("token refresh failed");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    Ok(OAuthTokens {
        access_token: body["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
            .to_string(),
        refresh_token: body["refresh_token"]
            .as_str()
            .unwrap_or(refresh)
            .to_string(),
        expires_at: now + expires_in,
    })
}

fn parse_token_response(body: &serde_json::Value) -> Result<OAuthTokens, OAuthError> {
    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    Ok(OAuthTokens {
        access_token: body["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
            .to_string(),
        refresh_token: body["refresh_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing refresh_token".into()))?
            .to_string(),
        expires_at: now + expires_in,
    })
}

fn google_client_id() -> Option<String> {
    std::env::var(GOOGLE_CLIENT_ID_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

fn google_client_secret() -> Option<String> {
    std::env::var(GOOGLE_CLIENT_SECRET_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

fn google_client_credentials() -> Result<(String, String), OAuthError> {
    let env_client_id = google_client_id();
    let env_client_secret = google_client_secret();

    match (env_client_id, env_client_secret) {
        (Some(client_id), Some(client_secret)) => Ok((client_id, client_secret)),
        (None, None) => Err(OAuthError::TokenExchange(format!(
            "Missing Google OAuth env config: set both {} and {}.",
            GOOGLE_CLIENT_ID_ENV, GOOGLE_CLIENT_SECRET_ENV
        ))),
        (Some(_), None) => Err(OAuthError::TokenExchange(format!(
            "Invalid Google OAuth env config: set both {} and {}, or set neither.",
            GOOGLE_CLIENT_ID_ENV, GOOGLE_CLIENT_SECRET_ENV
        ))),
        (None, Some(_)) => Err(OAuthError::TokenExchange(format!(
            "Invalid Google OAuth env config: set both {} and {}, or set neither.",
            GOOGLE_CLIENT_ID_ENV, GOOGLE_CLIENT_SECRET_ENV
        ))),
    }
}

fn extract_google_auth_code(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(code) = extract_query_param(trimmed, "code") {
        return code;
    }
    if let Some(pos) = trimmed.find("code=") {
        let rest = &trimmed[pos + 5..];
        let end = rest
            .char_indices()
            .find_map(|(i, ch)| (ch == '&' || ch == '#').then_some(i))
            .unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    trimmed.to_string()
}

fn extract_query_param(url_or_query: &str, key: &str) -> Option<String> {
    let query = if let Some(idx) = url_or_query.find('?') {
        &url_or_query[idx + 1..]
    } else {
        url_or_query
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let k = parts.next().unwrap_or("");
        let v = parts.next().unwrap_or("");
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

// ── Codex (OpenAI) device-code OAuth ─────────────────────────────

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const CODEX_DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const CODEX_DEVICE_CALLBACK: &str = "https://auth.openai.com/deviceauth/callback";
pub const CODEX_DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";

#[derive(Debug)]
pub struct CodexDeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

#[derive(Debug)]
pub struct CodexOAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: Option<String>,
}

/// Request a device code from OpenAI for the Codex auth flow.
pub async fn codex_request_device_code() -> Result<CodexDeviceCode, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_DEVICE_CODE_URL)
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

    Ok(CodexDeviceCode {
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

/// Poll the device auth endpoint. Returns `Ok(Some((auth_code, code_verifier)))` when approved,
/// `Ok(None)` when still pending, `Err` on failure.
pub async fn codex_poll_device_auth(
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<(String, String)>, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(CODEX_DEVICE_POLL_URL)
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
        // Still pending
        Ok(None)
    } else {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let err = body["error"]
            .as_str()
            .unwrap_or("device auth polling failed");
        Err(OAuthError::TokenExchange(err.to_string()))
    }
}

/// Exchange the device authorization code for tokens.
/// Uses form-urlencoded (not JSON) as required by OpenAI's token endpoint.
pub async fn codex_exchange_code(
    code: &str,
    code_verifier: &str,
) -> Result<CodexOAuthTokens, OAuthError> {
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

    // Extract account ID from id_token or access_token JWT
    let account_id = body["id_token"]
        .as_str()
        .and_then(extract_codex_account_id)
        .or_else(|| extract_codex_account_id(&access_token));

    Ok(CodexOAuthTokens {
        access_token,
        refresh_token,
        expires_at: now + expires_in,
        account_id,
    })
}

/// Refresh Codex OAuth tokens. Uses form-urlencoded.
pub async fn codex_refresh_tokens(refresh: &str) -> Result<OAuthTokens, OAuthError> {
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
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let err = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("token refresh failed");
        return Err(OAuthError::TokenExchange(err.to_string()));
    }

    let now = now_secs();
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

    Ok(OAuthTokens {
        access_token: body["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
            .to_string(),
        refresh_token: body["refresh_token"]
            .as_str()
            .unwrap_or(refresh)
            .to_string(),
        expires_at: now + expires_in,
    })
}

/// Extract the ChatGPT account ID from a JWT token (no crypto verification needed).
fn extract_codex_account_id(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // JWT payload is base64url-encoded; try with and without padding
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    // Try direct field first
    if let Some(id) = claims["chatgpt_account_id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    // Try nested auth claim
    if let Some(id) = claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    // Fall back to first organization ID
    if let Some(orgs) = claims["organizations"].as_array()
        && let Some(org) = orgs.first()
        && let Some(id) = org["id"].as_str()
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    None
}

/// Simple form-urlencoded encoder.
fn url_form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_escape(k), form_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode a value for application/x-www-form-urlencoded.
fn form_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Minimal percent-encoding for URL query parameters.
fn urlencoded(s: &str) -> String {
    s.replace('%', "%25")
        .replace(' ', "%20")
        .replace(':', "%3A")
        .replace('/', "%2F")
}
