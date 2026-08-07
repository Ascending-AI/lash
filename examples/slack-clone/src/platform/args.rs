//! Argument extraction and the failure envelope for the Web API surface.

use std::collections::HashMap;

use axum::extract::{Form, FromRequest, Query, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, RequestExt as _};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::wire::ApiErrorBody;

/// Extractor accepting arguments the way Slack's Web API does.
///
/// Slack takes the same arguments as a query string, as an
/// `application/x-www-form-urlencoded` body, or as a JSON body, and normalizes
/// them to strings — which is why form callers must JSON-*encode* object
/// arguments like `metadata`. This extractor merges all three sources (later
/// sources win) and stringifies every scalar, so handlers see one shape and a
/// client written against real Slack works unmodified.
pub struct SlackArgs<T>(pub T);

impl<T, S> FromRequest<S> for SlackArgs<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let mut merged = serde_json::Map::new();
        if let Ok(Query(pairs)) = Query::<HashMap<String, String>>::try_from_uri(request.uri()) {
            for (key, value) in pairs {
                merged.insert(key, Value::String(value));
            }
        }
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("application/json") {
            let Json(body): Json<Value> = request
                .extract()
                .await
                .map_err(|_| ApiError::new("invalid_json"))?;
            if let Value::Object(object) = body {
                for (key, value) in object {
                    merged.insert(key, stringify(value));
                }
            }
        } else if content_type.starts_with("application/x-www-form-urlencoded") {
            let Form(pairs): Form<HashMap<String, String>> = request
                .extract()
                .await
                .map_err(|_| ApiError::new("invalid_form_data"))?;
            for (key, value) in pairs {
                merged.insert(key, Value::String(value));
            }
        }

        let args = serde_json::from_value(Value::Object(merged))
            .map_err(|_| ApiError::new("invalid_arguments"))?;
        Ok(Self(args))
    }
}

/// Collapse a JSON value to Slack's string-valued argument model.
fn stringify(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(text),
        Value::Null => Value::String(String::new()),
        other => Value::String(other.to_string()),
    }
}

/// Whether a Slack boolean argument is set. Slack accepts `true`/`1`.
pub fn flag(raw: Option<&String>) -> bool {
    matches!(raw.map(String::as_str), Some("true") | Some("1"))
}

/// Clamp a Slack `limit` argument.
pub fn limit(raw: Option<&String>, default: usize, max: usize) -> usize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// A Web API failure, rendered as Slack renders it.
#[derive(Clone, Debug)]
pub struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

impl ApiError {
    /// A failure with Slack's default transport shape: **HTTP 200** with
    /// `ok: false`.
    ///
    /// This is not a shortcut. Slack answers essentially every Web API failure
    /// with 200 and puts the error in the body, so a client that only checks
    /// the status code silently succeeds on `invalid_auth`. Mirroring it means
    /// the bot's client has to get the check right here too.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: ApiErrorBody::new(error),
        }
    }

    /// A failure carrying a non-200 status. Slack reserves this for transport
    /// conditions such as `429 Too Many Requests`.
    pub fn with_status(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiErrorBody::new(error),
        }
    }

    /// The Slack error code.
    pub fn code(&self) -> &str {
        &self.body.error
    }

    /// Map an internal failure onto Slack's opaque `internal_error`.
    ///
    /// The cause goes to stderr, not to the caller: leaking a SQL message into
    /// an API response is how product internals become someone's contract.
    pub fn internal(context: &str, error: impl std::fmt::Display) -> Self {
        eprintln!("slack-clone-platform: {context}: {error}");
        Self::new("internal_error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_normalize_to_slacks_string_arguments() {
        assert_eq!(stringify(Value::Bool(true)), Value::String("true".into()));
        assert_eq!(stringify(serde_json::json!(20)), Value::String("20".into()));
        assert_eq!(
            stringify(serde_json::json!({"event_type": "x"})),
            Value::String("{\"event_type\":\"x\"}".into())
        );
    }

    #[test]
    fn flags_and_limits_follow_slacks_argument_conventions() {
        assert!(flag(Some(&"true".to_string())));
        assert!(flag(Some(&"1".to_string())));
        assert!(!flag(Some(&"yes".to_string())));
        assert!(!flag(None));
        assert_eq!(limit(None, 100, 999), 100);
        assert_eq!(limit(Some(&"5000".to_string()), 100, 999), 999);
        assert_eq!(limit(Some(&"0".to_string()), 100, 999), 1);
    }
}
