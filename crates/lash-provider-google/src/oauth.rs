//! Google OAuth authorize-URL / code-exchange / refresh-token flow.
//! Public so Host Applications can drive interactive login.

use lash_provider_auth::{OAuthError, OAuthTokens, now_secs, url_form_encode};

const GOOGLE_CLIENT_ID_ENV: &str = "LASH_GOOGLE_CLIENT_ID";
const GOOGLE_CLIENT_SECRET_ENV: &str = "LASH_GOOGLE_CLIENT_SECRET";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";
const GOOGLE_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_PROMPT: &str = "consent select_account";

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
    match (google_client_id(), google_client_secret()) {
        (Some(client_id), Some(client_secret)) => Ok((client_id, client_secret)),
        (None, None) => Err(OAuthError::TokenExchange(format!(
            "Missing Google OAuth env config: set both {} and {}.",
            GOOGLE_CLIENT_ID_ENV, GOOGLE_CLIENT_SECRET_ENV
        ))),
        _ => Err(OAuthError::TokenExchange(format!(
            "Invalid Google OAuth env config: set both {} and {}, or set neither.",
            GOOGLE_CLIENT_ID_ENV, GOOGLE_CLIENT_SECRET_ENV
        ))),
    }
}

/// Build the Google OAuth authorization URL for manual code entry.
pub fn authorize_url(challenge: &str) -> Result<String, OAuthError> {
    let state = uuid::Uuid::new_v4().to_string();
    let (client_id, _) = google_client_credentials()?;
    Ok(build_authorize_url(&client_id, challenge, &state))
}

fn build_authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    let query = url_form_encode(&[
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", GOOGLE_REDIRECT_URI),
        ("scope", GOOGLE_SCOPES),
        ("access_type", "offline"),
        ("prompt", GOOGLE_PROMPT),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    format!("{GOOGLE_AUTH_URL}?{query}")
}

/// Exchange an authorization code (or a redirect URL containing
/// `code=...`) for tokens.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<OAuthTokens, OAuthError> {
    let (client_id, client_secret) = google_client_credentials()?;
    let auth_code = extract_auth_code(code);
    if auth_code.is_empty() {
        return Err(OAuthError::TokenExchange(
            "no authorization code found in pasted input".to_string(),
        ));
    }
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
pub async fn refresh_tokens(refresh: &str) -> Result<OAuthTokens, OAuthError> {
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

fn extract_auth_code(input: &str) -> String {
    let trimmed = input.trim();
    // Query wins over fragment (probed first); a fragment-delivered or
    // prose-embedded `code=` is still extracted rather than posting the whole
    // pasted string to the token endpoint. Empty extractions fall through to
    // the raw input so `exchange_code` can reject them locally.
    let fragment = trimmed.split_once('#').map_or("", |(_, fragment)| fragment);
    [trimmed, fragment]
        .into_iter()
        .find_map(|candidate| {
            lash_provider_auth::extract_query_param(candidate, "code")
                .filter(|code| !code.is_empty())
        })
        .unwrap_or_else(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_matches_known_good_literal() {
        let url = build_authorize_url(
            "client&id=unexpected",
            "challenge/with+reserved?=&#%",
            "state=value&next",
        );

        assert_eq!(
            url,
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=client%26id%3Dunexpected&response_type=code&redirect_uri=https%3A%2F%2Fcodeassist.google.com%2Fauthcode&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.profile&access_type=offline&prompt=consent+select_account&code_challenge=challenge%2Fwith%2Breserved%3F%3D%26%23%25&code_challenge_method=S256&state=state%3Dvalue%26next"
        );
    }

    /// Documents the exact bytes sent to Google for production-shaped inputs.
    /// The only encoding difference from the pre-percent_encoding
    /// implementation is `+` (form-encoded space) instead of `%20` as the
    /// scope/prompt separator, which RFC 6749 §3.1 sanctions.
    #[test]
    fn authorization_url_matches_production_shaped_literal() {
        let url = build_authorize_url(
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        );

        assert_eq!(
            url,
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com&response_type=code&redirect_uri=https%3A%2F%2Fcodeassist.google.com%2Fauthcode&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.profile&access_type=offline&prompt=consent+select_account&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&state=3f2504e0-4f89-41d3-9a0c-0305e82c3301"
        );
    }

    #[test]
    fn pasted_redirect_url_extracts_code_without_fragment_or_parameter_confusion() {
        assert_eq!(
            extract_auth_code(
                "  https://localhost/callback?state=x&code=a%26b%3Dc%3Fd%2Be%25f%E9%9B%AA#code=wrong  "
            ),
            "a&b=c?d+e%f雪"
        );
        assert_eq!(
            extract_auth_code("https://localhost/callback?code=good%ZZ&code=wrong#fragment"),
            "good%ZZ"
        );
        // A fragment-delivered code is still extracted (query wins when both
        // are present, probed first).
        assert_eq!(
            extract_auth_code("https://localhost/callback#code=from-fragment"),
            "from-fragment"
        );
        // An empty extraction falls through to the raw input; exchange_code
        // rejects genuinely empty pastes locally.
        assert_eq!(extract_auth_code("code="), "code=");
        assert_eq!(extract_auth_code("plain-code"), "plain-code");
    }
}
