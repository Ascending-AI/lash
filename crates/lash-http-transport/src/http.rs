use std::fmt;
use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use lash_sansio::llm::types::ProviderFailureKind;

use crate::HttpTransportError;

#[async_trait]
pub trait HttpTransport: Send + Sync + fmt::Debug {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError>;
}

#[async_trait]
pub trait ByteStream: Send + fmt::Debug {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, HttpTransportError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Patch => reqwest::Method::PATCH,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub body_for_error: Option<String>,
    pub response_start_timeout_message: Option<String>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: body.into(),
            body_for_error: None,
            response_start_timeout_message: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self::new(HttpMethod::Post, url, body)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.headers.extend(
            headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into())),
        );
        self
    }

    pub fn with_body_for_error(mut self, body: impl Into<String>) -> Self {
        self.body_for_error = Some(body.into());
        self
    }

    pub fn with_response_start_timeout_message(mut self, message: impl Into<String>) -> Self {
        self.response_start_timeout_message = Some(message.into());
        self
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: HttpResponseBody,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .finish()
    }
}

pub enum HttpResponseBody {
    Buffered(Bytes),
    Streamed(Box<dyn ByteStream>),
}

impl HttpResponseBody {
    pub fn buffered(body: impl Into<Bytes>) -> Self {
        Self::Buffered(body.into())
    }

    pub fn streamed(stream: impl ByteStream + 'static) -> Self {
        Self::Streamed(Box::new(stream))
    }

    pub fn from_reqwest_response(response: reqwest::Response) -> Self {
        Self::streamed(ReqwestByteStream::new(response))
    }
}

impl fmt::Debug for HttpResponseBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffered(bytes) => f
                .debug_tuple("Buffered")
                .field(&format_args!("{} bytes", bytes.len()))
                .finish(),
            Self::Streamed(_) => f.write_str("Streamed(<byte stream>)"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl Default for ReqwestHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestHttpTransport {
    pub fn new() -> Self {
        Self {
            client: build_http_client(),
        }
    }

    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

#[async_trait]
impl HttpTransport for ReqwestHttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        let mut http = self
            .client
            .request(request.method.as_reqwest(), &request.url);
        for (name, value) in request.headers {
            http = http.header(name.as_str(), value.as_str());
        }
        http = http.body(request.body);

        let body_for_error = request.body_for_error;
        let timeout_message = request
            .response_start_timeout_message
            .unwrap_or_else(|| "HTTP response start timed out".to_string());

        run_with_timeout(
            async move {
                http.send()
                    .await
                    .map(|response| HttpResponse {
                        status: response.status().as_u16(),
                        headers: header_pairs(response.headers()),
                        body: HttpResponseBody::from_reqwest_response(response),
                    })
                    .map_err(|err| {
                        let error = HttpTransportError::new(format!("HTTP request failed: {err}"))
                            .with_kind(ProviderFailureKind::Transport)
                            .retryable(reqwest_error_is_retryable(&err));
                        if let Some(body) = body_for_error {
                            error.with_request_body(body)
                        } else {
                            error
                        }
                    })
            },
            timeout,
            &timeout_message,
        )
        .await
    }
}

#[derive(Debug)]
pub struct ReqwestByteStream {
    response: reqwest::Response,
}

impl ReqwestByteStream {
    pub fn new(response: reqwest::Response) -> Self {
        Self { response }
    }
}

#[async_trait]
impl ByteStream for ReqwestByteStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, HttpTransportError> {
        self.response.chunk().await.map_err(|err| {
            HttpTransportError::new(format!("HTTP response read failed: {err}"))
                .with_kind(ProviderFailureKind::Transport)
                .retryable(reqwest_error_is_retryable(&err))
        })
    }
}

pub async fn read_http_body_bytes(
    body: HttpResponseBody,
    timeout: Option<Duration>,
    timeout_message: &str,
) -> Result<Bytes, HttpTransportError> {
    match body {
        HttpResponseBody::Buffered(bytes) => Ok(bytes),
        HttpResponseBody::Streamed(mut stream) => {
            run_with_timeout(
                async move {
                    let mut body = BytesMut::new();
                    while let Some(chunk) = stream.next_chunk().await? {
                        body.extend_from_slice(&chunk);
                    }
                    Ok(body.freeze())
                },
                timeout,
                timeout_message,
            )
            .await
        }
    }
}

pub async fn read_http_body_text(
    body: HttpResponseBody,
    timeout: Option<Duration>,
    timeout_message: &str,
) -> Result<String, HttpTransportError> {
    let body = read_http_body_bytes(body, timeout, timeout_message).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

pub fn header_contains(headers: &[(String, String)], name: &str, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    headers.iter().any(|(header_name, value)| {
        header_name.eq_ignore_ascii_case(name) && value.to_ascii_lowercase().contains(&needle)
    })
}

pub fn first_header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub fn build_http_client() -> reqwest::Client {
    http_client_builder()
        .build()
        .expect("failed to build reqwest HTTP client")
}

/// Build a reqwest client with Lash's shared connection safeguards while
/// leaving authentication and other host policy configurable.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(60))
}

pub fn header_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn reqwest_error_is_retryable(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_body() || error.is_decode()
}

pub async fn run_with_timeout<T, F>(
    future: F,
    timeout: Option<Duration>,
    timeout_message: &str,
) -> Result<T, HttpTransportError>
where
    F: Future<Output = Result<T, HttpTransportError>>,
{
    match timeout {
        Some(duration) => tokio::time::timeout(duration, future).await.map_err(|_| {
            HttpTransportError::new(timeout_message)
                .with_kind(ProviderFailureKind::Timeout)
                .retryable(true)
                .with_code("timeout")
        })?,
        None => future.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn buffered_response_preserves_status_headers_and_body() {
        let response = HttpResponse {
            status: 429,
            headers: vec![
                ("retry-after".to_string(), "3".to_string()),
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: HttpResponseBody::buffered(r#"{"error":"rate limit"}"#),
        };

        assert_eq!(response.status, 429);
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .count(),
            2
        );
        assert!(header_contains(
            &response.headers,
            "content-type",
            "application/json"
        ));

        let text = read_http_body_text(response.body, Some(Duration::from_secs(1)), "timed out")
            .await
            .expect("buffered body");
        assert_eq!(text, r#"{"error":"rate limit"}"#);
    }

    #[tokio::test]
    async fn run_with_timeout_returns_timeout_error() {
        let result = run_with_timeout(
            async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok::<_, HttpTransportError>(())
            },
            Some(Duration::from_millis(5)),
            "request timed out",
        )
        .await;

        let err = result.expect_err("expected timeout");
        assert_eq!(err.message, "request timed out");
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert!(err.retryable);
    }
}
