//! FIG-2877: the uploaded-attachment cache must be bounded, expire entries,
//! and evict a URI the API rejects so the next request re-uploads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::llm::types::LlmMessage;
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::sync::MutexExt;

use crate::config::{UploadedAttachmentCache, UploadedAttachmentCacheKey, UploadedAttachmentRef};
use crate::support::*;

const STORED_BYTES: &[u8] = b"fig2877-stored-attachment-bytes";

fn cache_key(tag: &str) -> UploadedAttachmentCacheKey {
    UploadedAttachmentCacheKey {
        provider: "google_oauth",
        credential_scope: format!("scope-{tag}"),
        mime: "image/png".to_string(),
        hash: format!("hash-{tag}"),
    }
}

fn entry(tag: &str, uploaded_at: Instant) -> UploadedAttachmentRef {
    UploadedAttachmentRef {
        uri: format!("https://files.example/{tag}"),
        uploaded_at,
    }
}

#[test]
fn cache_entry_is_served_until_the_ttl_then_expires() {
    let mut cache = UploadedAttachmentCache::default();
    let now = Instant::now();
    cache.insert(cache_key("a"), entry("file-a", now), now);

    assert_eq!(
        cache.get(&cache_key("a"), now + Duration::from_secs(60)),
        Some(entry("file-a", now)),
    );
    assert_eq!(
        cache.get(&cache_key("a"), now + Duration::from_secs(24 * 60 * 60 + 1)),
        None,
        "past-TTL entry must expire"
    );
    assert_eq!(
        cache.get(&cache_key("a"), now + Duration::from_secs(60)),
        None,
        "expired entry is removed, not retained"
    );
}

#[test]
fn cache_is_bounded_and_evicts_the_oldest_entry() {
    let mut cache = UploadedAttachmentCache::default();
    let now = Instant::now();
    for index in 0..1024 {
        cache.insert(
            cache_key(&index.to_string()),
            entry(
                &format!("file-{index}"),
                now + Duration::from_secs(index as u64),
            ),
            now + Duration::from_secs(index as u64),
        );
    }
    cache.insert(
        cache_key("overflow"),
        entry("file-overflow", now + Duration::from_secs(2000)),
        now + Duration::from_secs(2000),
    );

    assert_eq!(
        cache.get(&cache_key("0"), now + Duration::from_secs(2000)),
        None,
        "oldest entry is evicted once the capacity is exceeded"
    );
    assert_eq!(
        cache.get(&cache_key("1023"), now + Duration::from_secs(2000)),
        Some(entry("file-1023", now + Duration::from_secs(1023))),
    );
    assert_eq!(
        cache.get(&cache_key("overflow"), now + Duration::from_secs(2000)),
        Some(entry("file-overflow", now + Duration::from_secs(2000))),
    );
}

/// Scripts the loadCodeAssist + resumable-upload + generateContent exchange.
/// The first finalized upload (`file-1`) answers as a dead URI; every later
/// upload serves a live URI.
#[derive(Debug, Default)]
struct GeminiFilesTransport {
    finalize_calls: AtomicUsize,
    generate_bodies: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LlmHttpTransport for GeminiFilesTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let upload_command = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-goog-upload-command"))
            .map(|(_, value)| value.as_str());
        if request.url.ends_with(":loadCodeAssist") {
            return Ok(LlmHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: LlmHttpBody::buffered(r#"{"cloudaicompanionProject":"resolved-project"}"#),
            });
        }
        if upload_command == Some("start") {
            return Ok(LlmHttpResponse {
                status: 200,
                headers: vec![(
                    "x-goog-upload-url".to_string(),
                    "https://uploads.example/session".to_string(),
                )],
                body: LlmHttpBody::buffered("{}"),
            });
        }
        if upload_command.is_some_and(|command| command.contains("finalize")) {
            let ordinal = self.finalize_calls.fetch_add(1, Ordering::SeqCst) + 1;
            return Ok(LlmHttpResponse {
                status: 200,
                headers: vec![("x-goog-upload-status".to_string(), "final".to_string())],
                body: LlmHttpBody::buffered(format!(
                    r#"{{"file":{{"uri":"https://files.example/file-{ordinal}"}}}}"#
                )),
            });
        }
        if request.url.ends_with(":generateContent") {
            let body = String::from_utf8_lossy(&request.body).into_owned();
            self.generate_bodies.lock_recover().push(body.clone());
            if body.contains("file-1") {
                return Ok(LlmHttpResponse {
                    status: 404,
                    headers: Vec::new(),
                    body: LlmHttpBody::buffered(
                        r#"{"error":{"message":"fileUri https://files.example/file-1 not found"}}"#,
                    ),
                });
            }
            return Ok(LlmHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: LlmHttpBody::buffered(
                    r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}"#,
                ),
            });
        }
        panic!("unexpected provider request {}", request.url);
    }
}

fn stored_attachment_request() -> LlmRequest {
    let attachment_id =
        lash_core::AttachmentId::parse("fig2877-cached-upload").expect("valid attachment id");
    let source = AttachmentSource::stored(lash_core::AttachmentRef {
        id: attachment_id.clone(),
        media_type: lash_core::MediaType::parse("image/png").expect("fixture MIME"),
        byte_len: STORED_BYTES.len() as u64,
        type_metadata: None,
        label: None,
    });
    LlmRequest {
        instructions: None,
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                source: Box::new(source),
            }],
        )],
        resolved_stored: [(attachment_id, STORED_BYTES.to_vec())]
            .into_iter()
            .collect(),
        tools: Default::default(),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: crate::attachment_test_capability(),
        scope: lash_core::LlmRequestScope::new(
            "fig2877-session",
            "fig2877-session:frame",
            "fig2877-session:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}

/// Cached URI → provider rejects it as a dead file → the entry is evicted →
/// the next request re-uploads instead of re-attempting the dead reference.
#[tokio::test]
async fn rejected_uploaded_uri_is_evicted_and_next_request_reuploads() {
    let transport = Arc::new(GeminiFilesTransport::default());
    let mut provider = crate::GoogleOAuthProvider::for_test().with_transport(transport.clone());

    let response = provider
        .complete(stored_attachment_request())
        .await
        .expect("inline fallback completes");
    assert_eq!(response.full_text(), "done");
    assert_eq!(
        transport.finalize_calls.load(Ordering::SeqCst),
        1,
        "first request uploads once"
    );
    {
        let bodies = transport.generate_bodies.lock_recover();
        assert_eq!(bodies.len(), 2, "fileData attempt then inline retry");
        assert!(
            bodies[0].contains("\"fileData\"") && bodies[0].contains("file-1"),
            "first generateContent uses the uploaded URI"
        );
        assert!(
            bodies[1].contains("\"inlineData\""),
            "retry falls back to inline bytes"
        );
    }

    let response = provider
        .complete(stored_attachment_request())
        .await
        .expect("re-upload completes");
    assert_eq!(response.full_text(), "done");
    assert_eq!(
        transport.finalize_calls.load(Ordering::SeqCst),
        2,
        "evicted entry forces a fresh upload rather than re-attempting file-1"
    );
    assert!(
        transport.generate_bodies.lock_recover()[2].contains("file-2"),
        "second request serves the fresh upload URI"
    );
}
