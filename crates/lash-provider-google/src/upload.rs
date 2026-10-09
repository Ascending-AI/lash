//! Google's Files API uploader. Reuse and invalidation belong to the host store.
use crate::support::*;
use lash_core_store::attachments::provider_files::{ProviderFileUploader, UploadedProviderFile};
use lash_core_store::attachments::{AttachmentStoreError, AttachmentStoreFailureClass};
use lash_sansio::llm::attachment_delivery::DeliverySecret;

const GEMINI_FILES_UPLOAD_URL: &str =
    "https://generativelanguage.googleapis.com/upload/v1beta/files";

/// A scoped uploader the host registers with ProviderFileDelivery.
/// Original blobs, cached handles and their lifetime remain store-owned.
#[derive(Clone, Debug)]
pub struct GoogleFileUploader {
    provider: GoogleOAuthProvider,
    scope: ProviderFileScope,
}
impl GoogleOAuthProvider {
    /// Configure a credential scope (and project) before constructing this
    /// uploader, then register it on the host's attachment store.
    pub fn file_uploader(&self) -> Option<GoogleFileUploader> {
        self.file_scope().map(|scope| GoogleFileUploader {
            provider: self.clone(),
            scope,
        })
    }
    async fn upload_attachment(
        &self,
        access_token: &str,
        project_id: Option<&str>,
        media_type: &lash_core::MediaType,
        bytes: &[u8],
        filename: &str,
    ) -> Result<UploadedProviderFile, LlmTransportError> {
        let start_body = json!({
            "file": {
                "displayName": filename,
                "mimeType": media_type,
                "sizeBytes": bytes.len().to_string(),
            }
        });
        let start_body_bytes = serde_json::to_vec(&start_body).map_err(|err| {
            LlmTransportError::new(format!(
                "Failed to serialize Gemini Files upload body: {err}"
            ))
            .with_kind(lash_core::ProviderFailureKind::Validation)
        })?;
        let mut start = LlmHttpRequest::post(GEMINI_FILES_UPLOAD_URL, start_body_bytes)
            .with_header(
                "Authorization",
                lash_llm_transport::HttpHeaderValue::sensitive(format!("Bearer {access_token}")),
            )
            .with_header("Content-Type", "application/json")
            .with_header("X-Goog-Upload-Protocol", "resumable")
            .with_header("X-Goog-Upload-Command", "start")
            .with_header(
                "X-Goog-Upload-Header-Content-Length",
                bytes.len().to_string(),
            )
            .with_header("X-Goog-Upload-Header-Content-Type", media_type.as_str())
            .with_header("X-Goog-Upload-File-Name", filename)
            .with_response_start_timeout_message("Gemini Files upload start timed out");
        if let Some(project_id) = project_id.filter(|project_id| !project_id.trim().is_empty()) {
            start = start.with_header("x-goog-user-project", project_id);
        }

        let start_resp = self
            .transport
            .send(start, self.options.llm_timeouts().request_timeout)
            .await?;
        if !start_resp.is_success() {
            let status = start_resp.status;
            let headers = start_resp.headers;
            let body = read_http_body_text(
                start_resp.body,
                self.options.response_body_limit(),
                self.options.llm_timeouts().request_timeout,
                "Gemini Files upload start body timed out",
            )
            .await?;
            return Err(upload_http_error_envelope(
                format!("Gemini Files upload start failed with {}", status),
                status,
                headers,
                body,
            ));
        }

        let upload_url = first_header_value(&start_resp.headers, "x-goog-upload-url")
            .ok_or_else(|| {
                LlmTransportError::new(
                    "Gemini Files upload start response missing x-goog-upload-url header",
                )
                .with_retry_verdict(TransportRetryVerdict::NotRetryable)
            })?
            .to_string();

        // The resumable URL is untrusted response data. It cannot authorize
        // an upload of the original bytes and bearer to a different origin.
        let same_origin = reqwest::Url::parse(&upload_url)
            .ok()
            .zip(reqwest::Url::parse(GEMINI_FILES_UPLOAD_URL).ok())
            .is_some_and(|(upload, start)| {
                upload.scheme() == start.scheme()
                    && upload.host_str() == start.host_str()
                    && upload.port_or_known_default() == start.port_or_known_default()
                    && upload.username().is_empty()
                    && upload.password().is_none()
            });
        if !same_origin {
            return Err(LlmTransportError::new("Gemini Files upload URL refused")
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
                .with_retry_verdict(TransportRetryVerdict::Forbidden));
        }

        let mut finalize = LlmHttpRequest::post(upload_url, bytes.to_vec())
            .with_header(
                "Authorization",
                lash_llm_transport::HttpHeaderValue::sensitive(format!("Bearer {access_token}")),
            )
            .with_header("X-Goog-Upload-Command", "upload, finalize")
            .with_header("X-Goog-Upload-Offset", "0")
            .with_header("Content-Length", bytes.len().to_string())
            .with_response_start_timeout_message("Gemini Files upload finalize timed out");
        if let Some(project_id) = project_id.filter(|project_id| !project_id.trim().is_empty()) {
            finalize = finalize.with_header("x-goog-user-project", project_id);
        }

        let finalize_resp = self
            .transport
            .send(finalize, self.options.llm_timeouts().request_timeout)
            .await?;
        if !finalize_resp.is_success() {
            let status = finalize_resp.status;
            let headers = finalize_resp.headers;
            let body = read_http_body_text(
                finalize_resp.body,
                self.options.response_body_limit(),
                self.options.llm_timeouts().request_timeout,
                "Gemini Files upload finalize body timed out",
            )
            .await?;
            return Err(upload_http_error_envelope(
                format!("Gemini Files upload finalize failed with {}", status),
                status,
                headers,
                body,
            ));
        }

        let upload_status =
            first_header_value(&finalize_resp.headers, "x-goog-upload-status").map(str::to_string);
        let body = read_http_body_text(
            finalize_resp.body,
            self.options.response_body_limit(),
            self.options.llm_timeouts().request_timeout,
            "Gemini Files upload finalize body timed out",
        )
        .await?;
        if upload_status
            .as_deref()
            .is_some_and(|status| status != "final")
        {
            return Err(LlmTransportError::new(
                "Gemini Files upload finalize returned an unexpected status",
            ));
        }

        let value: Value = serde_json::from_str(&body).map_err(|err| {
            LlmTransportError::new(format!("Invalid Gemini Files upload JSON: {err}"))
        })?;
        let file = value.get("file").unwrap_or(&value);
        let uri = if let Some(uri) = file.get("uri").and_then(|value| value.as_str()) {
            uri.to_string()
        } else if let Some(name) = file.get("name").and_then(|value| value.as_str()) {
            format!("https://generativelanguage.googleapis.com/v1beta/{name}")
        } else {
            return Err(LlmTransportError::new(
                "Gemini Files upload response missing file uri",
            ));
        };

        let valid_until_ms = file
            .get("expirationTime")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .and_then(|expiry| u64::try_from(expiry.timestamp_millis()).ok());
        Ok(UploadedProviderFile {
            id: DeliverySecret::new(uri),
            valid_until_ms,
        })
    }
}

fn upload_http_error_envelope(
    _message: impl Into<String>,
    status: u16,
    _headers: Vec<(String, String)>,
    _body: impl Into<String>,
) -> LlmTransportError {
    LlmTransportError::new("Google Files upload failed").with_http_status(status)
}
fn safe_upload_error(error: LlmTransportError) -> AttachmentStoreError {
    let class = if matches!(error.http_status, Some(401 | 403))
        || error.kind == ProviderFailureKind::Auth
    {
        AttachmentStoreFailureClass::Credentials
    } else if error.is_retryable() {
        AttachmentStoreFailureClass::Transient
    } else {
        AttachmentStoreFailureClass::Terminal
    };
    // Keep the actionable typed cause without exposing upload credentials,
    // file locators or a provider's echoed body in store diagnostics.
    let mut cause = LlmTransportError::new("Google Files upload failed")
        .with_kind(error.kind)
        .with_retry_verdict(error.retry_verdict);
    if let Some(code) = error.code {
        cause = cause.with_code(code);
    }
    if let Some(status) = error.http_status {
        cause = cause.with_http_status(status);
    }
    AttachmentStoreError::Backend {
        operation: "provider file upload",
        class,
        source: Box::new(cause),
    }
}
#[async_trait]
impl ProviderFileUploader for GoogleFileUploader {
    fn scope(&self) -> &ProviderFileScope {
        &self.scope
    }
    async fn upload(
        &self,
        reference: &AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedProviderFile, AttachmentStoreError> {
        let route = self.provider.route_identity_for_model("");
        let lease = self
            .provider
            .tokens
            .current(&route)
            .await
            .map_err(safe_upload_error)?;
        let filename = format!("lash-{}", reference.id.as_str());
        self.provider
            .upload_attachment(
                lease.token.secret().expose_secret(),
                self.provider.project_id.as_deref(),
                &reference.media_type,
                bytes,
                &filename,
            )
            .await
            .map_err(safe_upload_error)
    }
}
