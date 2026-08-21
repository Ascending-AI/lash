//! Gemini Files resumable upload: Lash-content-id caching of stored
//! attachments and the two-step (start / upload+finalize) upload protocol.

use crate::config::{UploadedAttachmentCacheKey, UploadedAttachmentRef};
use crate::support::*;
use sha2::{Digest, Sha256};

const GEMINI_FILES_UPLOAD_URL: &str =
    "https://generativelanguage.googleapis.com/upload/v1beta/files";

fn upload_http_error_envelope(
    message: impl Into<String>,
    status: u16,
    headers: Vec<(String, String)>,
    body: impl Into<String>,
) -> LlmTransportError {
    let failure = http_error_envelope(message, status, headers, body, None);
    if status == 413 {
        failure
            .with_kind(ProviderFailureKind::Validation)
            .retryable(false)
    } else {
        failure
    }
}

impl GoogleOAuthProvider {
    fn upload_cache_key(
        credential_scope_seed: &str,
        project_id: Option<&str>,
        media_type: &lash_core::MediaType,
        content_id: &lash_core::AttachmentId,
    ) -> UploadedAttachmentCacheKey {
        let credential_hash = Sha256::digest(credential_scope_seed.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        UploadedAttachmentCacheKey {
            provider: Self::PROVIDER_KIND,
            credential_scope: format!("{}:{}", credential_hash, project_id.unwrap_or_default()),
            mime: media_type.to_string(),
            hash: content_id.to_string(),
        }
    }

    fn uploaded_attachment_filename(key: &UploadedAttachmentCacheKey) -> String {
        let ext = match key.mime.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/jpg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/heic" => "heic",
            "image/heif" => "heif",
            "image/bmp" => "bmp",
            "image/tiff" => "tiff",
            _ => "bin",
        };
        format!("lash-{}.{}", &key.hash[..12], ext)
    }

    async fn upload_attachment_cached(
        &self,
        access_token: &str,
        credential_scope_seed: &str,
        project_id: Option<&str>,
        attachment_ref: &lash_core::AttachmentRef,
        bytes: &[u8],
    ) -> Result<UploadedAttachmentRef, LlmTransportError> {
        let key = Self::upload_cache_key(
            credential_scope_seed,
            project_id,
            &attachment_ref.media_type,
            &attachment_ref.id,
        );
        if let Some(existing) = Self::uploaded_attachment_cache()
            .lock()
            .await
            .get(&key)
            .cloned()
        {
            return Ok(existing);
        }

        let uploaded = self
            .upload_attachment(
                access_token,
                project_id,
                &attachment_ref.media_type,
                bytes,
                &Self::uploaded_attachment_filename(&key),
            )
            .await?;
        Self::uploaded_attachment_cache()
            .lock()
            .await
            .insert(key, uploaded.clone());
        Ok(uploaded)
    }

    async fn upload_attachment(
        &self,
        access_token: &str,
        project_id: Option<&str>,
        media_type: &lash_core::MediaType,
        bytes: &[u8],
        filename: &str,
    ) -> Result<UploadedAttachmentRef, LlmTransportError> {
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
            .with_header("Authorization", format!("Bearer {access_token}"))
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
                self.options.llm_timeouts().request_timeout,
                "Gemini Files upload start body timed out",
            )
            .await
            .unwrap_or_default();
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
                .retryable(false)
            })?
            .to_string();

        let mut finalize = LlmHttpRequest::post(upload_url, bytes.to_vec())
            .with_header("Authorization", format!("Bearer {access_token}"))
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
                self.options.llm_timeouts().request_timeout,
                "Gemini Files upload finalize body timed out",
            )
            .await
            .unwrap_or_default();
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
            self.options.llm_timeouts().request_timeout,
            "Gemini Files upload finalize body timed out",
        )
        .await?;
        if upload_status
            .as_deref()
            .is_some_and(|status| status != "final")
        {
            return Err(LlmTransportError::new(format!(
                "Gemini Files upload finalize returned unexpected status `{}`",
                upload_status.unwrap_or_default()
            ))
            .with_raw(body));
        }

        let value: Value = serde_json::from_str(&body).map_err(|err| {
            LlmTransportError::new(format!("Invalid Gemini Files upload JSON: {err}"))
                .with_raw(body.clone())
        })?;
        let file = value.get("file").unwrap_or(&value);
        let uri = if let Some(uri) = file.get("uri").and_then(|value| value.as_str()) {
            uri.to_string()
        } else if let Some(name) = file.get("name").and_then(|value| value.as_str()) {
            format!("https://generativelanguage.googleapis.com/v1beta/{name}")
        } else {
            return Err(
                LlmTransportError::new("Gemini Files upload response missing file uri")
                    .with_raw(body.clone()),
            );
        };

        Ok(UploadedAttachmentRef { uri })
    }

    pub(crate) async fn prepare_attachment_parts(
        &self,
        access_token: &str,
        credential_scope_seed: &str,
        project_id: Option<&str>,
        req: &LlmRequest,
    ) -> Result<(Vec<Value>, bool), LlmTransportError> {
        let mut parts = Vec::with_capacity(req.attachments.len());
        let mut used_uploaded_files = false;

        for source in &req.attachments {
            if let AttachmentSource::Stored { attachment_ref } = source {
                let bytes = req
                    .attachment_bytes(source)
                    .expect("stored attachment validated as resolved");
                match self
                    .upload_attachment_cached(
                        access_token,
                        credential_scope_seed,
                        project_id,
                        attachment_ref,
                        bytes,
                    )
                    .await
                {
                    Ok(uploaded) => {
                        used_uploaded_files = true;
                        parts.push(json!({
                            "fileData": {
                                "mimeType": attachment_ref.media_type,
                                "fileUri": uploaded.uri,
                            }
                        }));
                    }
                    Err(error) if error.status == Some(401) => return Err(error),
                    Err(_) => parts.push(Self::inline_attachment_part(req, source)),
                }
            } else {
                parts.push(Self::inline_attachment_part(req, source));
            }
        }

        Ok((parts, used_uploaded_files))
    }
}

#[cfg(test)]
mod error_detail_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use lash_core::provider::{DefaultProviderFailureClassifier, ProviderFailureClassifier};
    use lash_llm_transport::{LlmHttpBody, LlmHttpResponse};
    use lash_sansio::sync::MutexExt;

    #[derive(Debug)]
    struct ResponseQueue(Mutex<VecDeque<LlmHttpResponse>>);

    #[async_trait::async_trait]
    impl LlmHttpTransport for ResponseQueue {
        async fn send(
            &self,
            _request: LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<LlmHttpResponse, LlmTransportError> {
            Ok(self
                .0
                .lock_recover()
                .pop_front()
                .expect("scripted response"))
        }
    }

    fn response(
        status: u16,
        headers: Vec<(String, String)>,
        body: &'static str,
    ) -> LlmHttpResponse {
        LlmHttpResponse {
            status,
            headers,
            body: LlmHttpBody::buffered(body),
        }
    }

    async fn upload_with(responses: Vec<LlmHttpResponse>) -> LlmTransportError {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            u64::MAX,
            crate::GoogleOAuthClient {
                id: "oauth-client-id".into(),
                secret: "oauth-client-secret".into(),
            },
        )
        .with_transport(Arc::new(ResponseQueue(Mutex::new(responses.into()))));
        provider
            .upload_attachment(
                "access",
                None,
                &lash_core::MediaType::parse("image/png").expect("valid MIME"),
                b"png",
                "fixture.png",
            )
            .await
            .expect_err("fixture is an HTTP error")
    }

    #[tokio::test]
    async fn upload_start_error_surfaces_api_message() {
        let error = upload_with(vec![response(
            400,
            Vec::new(),
            r#"{"error":{"message":"upload start detail"}}"#,
        )])
        .await;
        assert!(error.message.contains("upload start detail"));
    }

    #[tokio::test]
    async fn upload_missing_url_is_explicitly_non_retryable() {
        let error = upload_with(vec![response(200, Vec::new(), "")]).await;

        assert!(!error.retryable);
        assert!(error.retryability_is_classified());
        let failure = DefaultProviderFailureClassifier.classify(error);
        assert!(!failure.retryable);
    }

    #[tokio::test]
    async fn upload_finalize_error_surfaces_api_message() {
        let error = upload_with(vec![
            response(
                200,
                vec![(
                    "x-goog-upload-url".to_string(),
                    "https://upload.example/session".to_string(),
                )],
                "",
            ),
            response(
                400,
                Vec::new(),
                r#"{"error":{"message":"upload finalize detail"}}"#,
            ),
        ])
        .await;
        assert!(error.message.contains("upload finalize detail"));
    }

    #[tokio::test]
    async fn upload_413_request_too_large_is_non_retryable_validation() {
        let error = upload_with(vec![response(
            413,
            Vec::new(),
            r#"{"error":{"message":"Request too large: attachment exceeds upload limit"}}"#,
        )])
        .await;

        let failure = DefaultProviderFailureClassifier.classify(error);

        assert_eq!(failure.kind, ProviderFailureKind::Validation);
        assert!(!failure.retryable);
        assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    }
}
