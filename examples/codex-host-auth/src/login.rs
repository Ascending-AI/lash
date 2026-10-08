//! The ChatGPT device-code login, run by the host.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::source::{CodexHostAuth, StoredLogin};
use crate::{CODEX_CLIENT_ID, CODEX_TOKEN_URL, OAUTH_ENDPOINT_TIMEOUT, stored_login};

const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_CALLBACK: &str = "https://auth.openai.com/deviceauth/callback";
/// The page a person opens to enter the [`DeviceCode::user_code`].
pub const DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";

/// What the person signing in needs: open [`DEVICE_VERIFY_URL`] and enter
/// `user_code`.
#[derive(Debug)]
pub struct DeviceCode {
    pub user_code: String,
    pub verify_url: &'static str,
}

/// Run the device-code login, telling the person what to do through `prompt`,
/// and store the login at `path`.
pub async fn login(path: &Path, prompt: impl FnOnce(&DeviceCode)) -> anyhow::Result<StoredLogin> {
    let http = reqwest::Client::new();
    let started = post_json(
        &http,
        DEVICE_CODE_URL,
        serde_json::json!({ "client_id": CODEX_CLIENT_ID }),
    )
    .await?
    .context("device authorization was refused")?;
    let device_auth_id = text(&started, "device_auth_id")?;
    let user_code = text(&started, "user_code")?;
    let interval = started["interval"]
        .as_str()
        .and_then(|interval| interval.parse().ok())
        .or(started["interval"].as_u64())
        .unwrap_or(5)
        .max(1);
    prompt(&DeviceCode {
        user_code: user_code.clone(),
        verify_url: DEVICE_VERIFY_URL,
    });

    let approved = loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let poll = serde_json::json!({ "device_auth_id": device_auth_id, "user_code": user_code });
        if let Some(approved) = post_json(&http, DEVICE_POLL_URL, poll).await? {
            break approved;
        }
    };
    let form = form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "authorization_code"),
            ("code", text(&approved, "authorization_code")?.as_str()),
            ("redirect_uri", DEVICE_CALLBACK),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", text(&approved, "code_verifier")?.as_str()),
        ])
        .finish();
    let response = http
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .timeout(OAUTH_ENDPOINT_TIMEOUT)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "the ChatGPT token exchange failed with {}",
            response.status()
        );
    }
    let login = stored_login(&response.json().await?, None)?;
    CodexHostAuth::save(path, &login).await?;
    Ok(login)
}

/// POST `body`; `None` while the device authorization is still pending.
async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let response = http
        .post(url)
        .json(&body)
        .timeout(OAUTH_ENDPOINT_TIMEOUT)
        .send()
        .await?;
    match response.status().as_u16() {
        200..=299 => Ok(Some(response.json().await?)),
        403 | 404 => Ok(None),
        status => bail!("{url} answered {status}"),
    }
}

fn text(body: &serde_json::Value, field: &str) -> anyhow::Result<String> {
    body[field]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("the response has no {field}"))
}
