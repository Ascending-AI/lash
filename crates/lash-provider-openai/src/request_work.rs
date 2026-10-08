//! Allocation and scheduling policy for OpenAI request construction.
use base64::Engine;
use std::io::{self, Write};

mod raw_budget;

/// Scheduling policy for OpenAI-compatible request construction and decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestWorkPolicy {
    /// Source/encoded bytes prepared inline before offloading to the blocking pool.
    /// Zero offloads all nonempty work. This is a scheduling threshold, not a refusal limit.
    pub inline_bytes: usize,
}
impl RequestWorkPolicy {
    /// Prepare at most 64 KiB inline. This historical scheduling threshold has
    /// no workload measurement; small work avoids a blocking-task hop.
    pub fn standard() -> Self {
        Self {
            inline_bytes: 64 * 1024,
        }
    }
}
impl Default for RequestWorkPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// Optional provider error excerpts, independent of response admission ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestDiagnosticLimits {
    /// Bytes retained before the original-length annotation. Zero keeps only the annotation.
    pub excerpt_bytes: usize,
}
impl RequestDiagnosticLimits {
    /// Standard preset: 4096 diagnostic bytes. No workload measurement supports this value.
    pub const fn standard() -> Self {
        Self {
            excerpt_bytes: 4096,
        }
    }
}
impl Default for RequestDiagnosticLimits {
    fn default() -> Self {
        Self::standard()
    }
}
#[cfg(test)]
const EXCERPT_BYTES: usize = RequestDiagnosticLimits::standard().excerpt_bytes;

pub(crate) fn attachment_data_url(media_type: &str, bytes: &[u8]) -> String {
    #[expect(
        clippy::expect_used,
        reason = "`encoded_len` only declines when the encoded length overflows `usize`, \
                  which cannot happen for a slice already held in memory"
    )]
    let encoded_len = base64::encoded_len(bytes.len(), true).expect("attachment fits in memory");
    let mut url = String::with_capacity(5 + media_type.len() + 8 + encoded_len);
    url.push_str("data:");
    url.push_str(media_type);
    url.push_str(";base64,");
    // Append directly to the final allocation, including padding. Never build
    // a standalone base64 String and then copy it into a data URL.
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut url);
    url
}

struct SizeProbe {
    remaining: usize,
}

impl Write for SizeProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("request exceeds inline work budget"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn needs_blocking(
    request: &crate::support::LlmRequest,
    policy: RequestWorkPolicy,
) -> bool {
    // The traversal charges every node and stops at the same budget, including for many empty
    // fields or large byte sequences.
    if !raw_budget::RawBudget::fits(request, policy.inline_bytes) {
        return true;
    }
    // Only bounded, small raw fields reach the escaping JSON writer. Keep
    // its aggregate check for punctuation and escape expansion.
    serde_json::to_writer(
        &mut SizeProbe {
            remaining: policy.inline_bytes,
        },
        request,
    )
    .is_err()
}

pub(crate) async fn run<T: Send + 'static>(
    blocking: bool,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, crate::support::LlmTransportError> {
    if blocking {
        tokio::task::spawn_blocking(work).await.map_err(|error| {
            crate::support::LlmTransportError::new(format!("OpenAI request work failed: {error}"))
        })
    } else {
        Ok(work())
    }
}

impl RequestDiagnosticLimits {
    /// Preserve small diagnostic JSON verbatim. Large diagnostics retain a
    /// UTF-8-safe prefix plus the original byte count, never the full body.
    pub(crate) fn body_excerpt(self, body: &str) -> String {
        if body.len() <= self.excerpt_bytes {
            return body.to_owned();
        }
        format!("{}\n[body bytes: {}]", self.body_prefix(body), body.len())
    }

    pub(crate) fn body_prefix(self, body: &str) -> &str {
        let mut end = body.len().min(self.excerpt_bytes);
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        &body[..end]
    }

    /// Serialize diagnostics into a bounded writer rather than materializing a
    /// full JSON String solely to truncate it on an error path.
    #[expect(
        clippy::expect_used,
        reason = "serializing a `serde_json::Value` into an in-memory writer cannot fail, \
              and the UTF-8 prefix is re-split at a boundary `valid_up_to` just reported"
    )]
    pub(crate) fn json_excerpt(self, value: &serde_json::Value) -> String {
        struct ExcerptWriter {
            prefix: Vec<u8>,
            total: usize,
            limit: usize,
        }
        impl Write for ExcerptWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.total += bytes.len();
                let keep = bytes.len().min(self.limit - self.prefix.len());
                self.prefix.extend_from_slice(&bytes[..keep]);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ExcerptWriter {
            prefix: Vec::new(),
            total: 0,
            limit: self.excerpt_bytes,
        };
        // Callers supply JSON Values, whose serialization cannot fail.
        serde_json::to_writer(&mut writer, value).expect("JSON diagnostic serialization");
        let prefix = match std::str::from_utf8(&writer.prefix) {
            Ok(prefix) => prefix,
            Err(error) => {
                std::str::from_utf8(&writer.prefix[..error.valid_up_to()]).expect("UTF-8 prefix")
            }
        };
        if writer.total <= self.excerpt_bytes {
            prefix.to_owned()
        } else {
            format!("{prefix}\n[body bytes: {}]", writer.total)
        }
    }

    pub(crate) fn diagnostic_message(self, message: &str) -> String {
        if message.len() > self.excerpt_bytes {
            self.body_excerpt(message)
        } else {
            message.to_owned()
        }
    }

    // Preserve the shared envelope's message and retry-delay classification even
    // when those fields occur after a large echoed payload. Only this small
    // projection is serialized for that classifier, never the original tree.
    pub(crate) fn error_metadata(self, value: &serde_json::Value) -> Option<String> {
        use serde_json::{Value, json};
        fn retry_delay(value: &Value) -> Option<&str> {
            match value {
                Value::Object(fields) => fields
                    .get("retryDelay")
                    .and_then(Value::as_str)
                    .or_else(|| fields.values().find_map(retry_delay)),
                Value::Array(items) => items.iter().find_map(retry_delay),
                _ => None,
            }
        }
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str);
        let delay = retry_delay(value);
        if message.is_none() && delay.is_none() {
            return None;
        }
        let mut metadata = json!({});
        if let Some(message) = message {
            metadata["error"] = json!({"message": self.diagnostic_message(message)});
        }
        if let Some(delay) = delay {
            metadata["retryDelay"] = Value::String(delay.to_owned());
        }
        Some(metadata.to_string())
    }
}

pub(crate) fn bytes_need_blocking(len: usize, policy: RequestWorkPolicy) -> bool {
    len > policy.inline_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_encodes_into_one_final_allocation() {
        for (bytes, expected) in [
            (b"".as_slice(), ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
            (b"\x00\xff\xfe".as_slice(), "AP/+"),
        ] {
            let url = attachment_data_url("image/png", bytes);
            assert_eq!(url, format!("data:image/png;base64,{expected}"));
            assert_eq!(url.capacity(), url.len());
        }
        let bytes = vec![255; RequestWorkPolicy::standard().inline_bytes * 2];
        let url = attachment_data_url("application/pdf", &bytes);
        assert_eq!(url.capacity(), url.len());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(url.split_once(',').unwrap().1)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn error_excerpt_is_bounded_and_counts_original_bytes() {
        assert_eq!(RequestDiagnosticLimits::standard().body_excerpt("{}"), "{}");
        assert_eq!(
            RequestDiagnosticLimits::standard().body_excerpt(&"x".repeat(EXCERPT_BYTES)),
            "x".repeat(EXCERPT_BYTES)
        );
        let body = "€".repeat(EXCERPT_BYTES);
        let error = crate::support::LlmTransportError::new("invalid response")
            .with_raw(RequestDiagnosticLimits::standard().body_excerpt(&body));
        let raw = error.raw.as_deref().unwrap();
        assert!(raw.len() <= EXCERPT_BYTES + 40);
        assert!(raw.ends_with("[body bytes: 12288]"));
        assert!(raw.starts_with('€'));
        let value = serde_json::json!({"diagnostic": "€"});
        let whole = RequestDiagnosticLimits {
            excerpt_bytes: usize::MAX,
        }
        .json_excerpt(&value);
        assert_eq!(whole, value.to_string());
        assert_eq!(
            RequestDiagnosticLimits { excerpt_bytes: 0 }.json_excerpt(&value),
            format!("\n[body bytes: {}]", whole.len())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_work_leaves_the_async_worker_and_small_work_stays_inline() {
        let worker = std::thread::current().id();
        assert_eq!(
            run(
                bytes_need_blocking(4, RequestWorkPolicy { inline_bytes: 8 }),
                || std::thread::current().id()
            )
            .await
            .unwrap(),
            worker
        );
        assert_ne!(
            run(
                bytes_need_blocking(4, RequestWorkPolicy { inline_bytes: 1 }),
                || std::thread::current().id()
            )
            .await
            .unwrap(),
            worker
        );
    }
}
