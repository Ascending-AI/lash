//! Google OAuth authorize-URL / code-exchange / refresh-token flow.
//! Public so Host Applications can drive interactive login.

use lash_provider_auth::{OAuthError, OAuthTokens, now_secs, oauth_send, url_form_encode};

use crate::GoogleOAuthClient;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";
const GOOGLE_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_PROMPT: &str = "consent select_account";

fn validate_client_credentials(oauth_client: &GoogleOAuthClient) -> Result<(), OAuthError> {
    if oauth_client.id.trim().is_empty() || oauth_client.secret.expose_secret().trim().is_empty() {
        Err(OAuthError::TokenExchange(
            "Google OAuth client id and client secret must both be non-empty.".to_string(),
        ))
    } else {
        Ok(())
    }
}

/// Build the Google OAuth authorization URL for manual code entry.
pub fn authorize_url(client_id: &str, challenge: &str) -> Result<String, OAuthError> {
    if client_id.trim().is_empty() {
        return Err(OAuthError::TokenExchange(
            "Google OAuth client id must be non-empty.".to_string(),
        ));
    }
    let state = uuid::Uuid::new_v4().to_string();
    Ok(build_authorize_url(client_id, challenge, &state))
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
/// `code=...`) for tokens using the host's named OAuth client credentials.
pub async fn exchange_code(
    oauth_client: &GoogleOAuthClient,
    code: &str,
    verifier: &str,
) -> Result<OAuthTokens, OAuthError> {
    validate_client_credentials(oauth_client)?;
    let auth_code = extract_auth_code(code);
    if auth_code.is_empty() {
        return Err(OAuthError::TokenExchange(
            "no authorization code found in pasted input".to_string(),
        ));
    }
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(GOOGLE_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(url_form_encode(&[
                ("grant_type", "authorization_code"),
                ("code", auth_code.as_str()),
                ("redirect_uri", GOOGLE_REDIRECT_URI),
                ("client_id", oauth_client.id.as_str()),
                ("client_secret", oauth_client.secret.expose_secret()),
                ("code_verifier", verifier),
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
    parse_token_response(&body)
}

/// Refresh Google OAuth tokens using the host's named OAuth client credentials.
pub async fn refresh_tokens(
    oauth_client: &GoogleOAuthClient,
    refresh: &str,
) -> Result<OAuthTokens, OAuthError> {
    refresh_tokens_at(oauth_client, GOOGLE_TOKEN_URL, refresh).await
}

/// The refresh exchange against an explicit token endpoint, so tests can
/// point the flow at a fake peer.
async fn refresh_tokens_at(
    oauth_client: &GoogleOAuthClient,
    token_url: &str,
    refresh: &str,
) -> Result<OAuthTokens, OAuthError> {
    validate_client_credentials(oauth_client)?;
    let (status, response_body) = oauth_send(
        reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(url_form_encode(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", oauth_client.id.as_str()),
                ("client_secret", oauth_client.secret.expose_secret()),
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
    Ok(OAuthTokens {
        access_token: body["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing access_token".into()))?
            .to_string()
            .into(),
        refresh_token: body["refresh_token"]
            .as_str()
            .unwrap_or(refresh)
            .to_string()
            .into(),
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
            .to_string()
            .into(),
        refresh_token: body["refresh_token"]
            .as_str()
            .ok_or_else(|| OAuthError::TokenExchange("missing refresh_token".into()))?
            .to_string()
            .into(),
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
    fn google_authorization_url_requires_explicit_client_id() {
        let error = authorize_url("   ", "challenge").expect_err("an empty client id is rejected");
        assert!(error.to_string().contains("client id must be non-empty"));

        let url = authorize_url("client-id", "challenge")
            .expect("an explicit client id builds an authorization URL");
        assert!(url.contains("client_id=client-id"));
        assert!(url.contains("code_challenge=challenge"));
    }

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

    fn test_oauth_client() -> GoogleOAuthClient {
        GoogleOAuthClient {
            id: "client-id".to_string(),
            secret: lash_provider_auth::Redacted::new("client-secret"),
        }
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
            refresh_tokens_at(
                &test_oauth_client(),
                &format!("{url}/token"),
                "refresh-token",
            ),
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
            refresh_tokens_at(
                &test_oauth_client(),
                &format!("{url}/token"),
                "refresh-token",
            ),
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
