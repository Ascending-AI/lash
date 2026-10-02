//! Codex (OpenAI) device-code OAuth flow + token refresh. Public so
//! Host Applications can drive the interactive login.

use base64::Engine;

// The flow's failures are part of the host-facing login surface: hosts drive
// these functions and name the error (and its token-endpoint code) themselves.
pub use lash_provider_auth::{OAuthError, OAuthTokenErrorCode};
use lash_provider_auth::{now_secs, oauth_send, url_form_encode};
use lash_sansio::Redacted;

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
    pub access_token: Redacted,
    pub refresh_token: Redacted,
    pub expires_at: u64,
    pub account_id: Option<Redacted>,
}

/// Request a device code from OpenAI for the Codex auth flow.
pub async fn request_device_code() -> Result<DeviceCode, OAuthError> {
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(CODEX_DEVICE_CODE_URL)
            .header("User-Agent", codex_user_agent())
            .json(&serde_json::json!({ "client_id": CODEX_CLIENT_ID })),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&response_body)?;

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

/// Returns `Ok(Some((auth_code, code_verifier)))` when approved, `Ok(None)` when still
/// pending, `Err` on failure.
pub async fn poll_device_auth(
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<(String, String)>, OAuthError> {
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(CODEX_DEVICE_POLL_URL)
            .header("User-Agent", codex_user_agent())
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            })),
    )
    .await?;

    if status.is_success() {
        let body: serde_json::Value = serde_json::from_str(&response_body)?;
        let auth_code = body["authorization_code"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing authorization_code".into()))?
            .to_string();
        let code_verifier = body["code_verifier"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing code_verifier".into()))?
            .to_string();
        Ok(Some((auth_code, code_verifier)))
    } else if status.as_u16() == 403 || status.as_u16() == 404 {
        Ok(None)
    } else {
        let body: serde_json::Value = serde_json::from_str(&response_body).unwrap_or_default();
        let err = body["error"]
            .as_str()
            .unwrap_or("device auth polling failed");
        Err(OAuthError::TokenExchange(err.to_string()))
    }
}

/// Exchange the device authorization code for tokens. Uses
/// form-urlencoded as required by OpenAI's token endpoint.
pub async fn exchange_code(code: &str, code_verifier: &str) -> Result<CodexTokens, OAuthError> {
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(CODEX_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(url_form_encode(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", CODEX_DEVICE_CALLBACK),
                ("client_id", CODEX_CLIENT_ID),
                ("code_verifier", code_verifier),
            ])),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&response_body)?;

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
        access_token: Redacted::new(access_token),
        refresh_token: Redacted::new(refresh_token),
        expires_at: now + expires_in,
        account_id: account_id.map(Redacted::new),
    })
}

/// Refresh Codex OAuth tokens.
pub async fn refresh_tokens(refresh: &str) -> Result<CodexTokens, OAuthError> {
    refresh_tokens_at(CODEX_TOKEN_URL, refresh).await
}

/// The refresh exchange against an explicit token endpoint, so tests can
/// point the flow at a fake peer.
async fn refresh_tokens_at(token_url: &str, refresh: &str) -> Result<CodexTokens, OAuthError> {
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(url_form_encode(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CODEX_CLIENT_ID),
            ])),
    )
    .await?;

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
        access_token: Redacted::new(access_token),
        refresh_token: Redacted::new(refresh_token),
        expires_at: now + expires_in,
        account_id: account_id.map(Redacted::new),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve exactly one HTTP request on a loopback socket, then run
    /// `respond`. The fake peer may stall (hold the socket silent) or flood
    /// (write past the body cap); both are endpoint shapes the deadline and
    /// byte cap exist for (FIG-4708 P2).
    fn serve_once(
        respond: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            // The request body can arrive in the same segment as the headers,
            // so look for the terminator anywhere rather than at the tail.
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = std::io::Read::read(&mut stream, &mut buf).unwrap();
                if read == 0 {
                    return;
                }
                head.extend_from_slice(&buf[..read]);
            }
            respond(stream);
        });
        (url, server)
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_token_endpoint_fails_refresh_with_typed_timeout() {
        let (url, server) = serve_once(|mut stream| {
            // Accept the request then stay silent until the client gives up:
            // the refresh deadline must end the wait rather than hang the
            // credential refresh gate.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf = [0u8; 64];
            let _ = std::io::Read::read(&mut stream, &mut buf);
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            refresh_tokens_at(&format!("{url}/token"), "refresh-token"),
        )
        .await
        .expect("the refresh deadline bounds the wait")
        .expect_err("a stalled endpoint is a typed timeout");
        assert!(matches!(error, OAuthError::Timeout), "{error:?}");
        assert_eq!(
            lash_provider_auth::classify_oauth_refresh_error(error).kind,
            lash_provider_auth::CredentialErrorKind::Transient
        );
        // The deadline may fire before the fake peer's accept/read completes;
        // detach rather than join so the law cannot hang on its own fixture.
        drop(server);
    }

    #[tokio::test]
    async fn oversized_token_response_fails_refresh_with_typed_cause() {
        let padded = serde_json::json!({
            "access_token": "access",
            "refresh_token": "rotated",
            "expires_in": 3600,
            "pad": "x".repeat(256 * 1024),
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{padded}",
            padded.len()
        );
        let (url, server) = serve_once(move |mut stream| {
            use std::io::Write as _;
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
            let _ = stream.write_all(response.as_bytes());
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            refresh_tokens_at(&format!("{url}/token"), "refresh-token"),
        )
        .await
        .expect("the refresh deadline bounds the wait")
        .expect_err("an oversized body is a typed failure");
        assert!(
            matches!(error, OAuthError::ResponseTooLarge { .. }),
            "{error:?}"
        );
        let _ = server.join();
    }
}
