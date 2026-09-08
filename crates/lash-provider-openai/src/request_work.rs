//! Allocation and scheduling policy for OpenAI request construction.
use base64::Engine;
use serde::Serialize;
use std::io::{self, Write};

mod raw_budget;

// 64 KiB of source JSON (or resolved bytes) is enough to warrant a blocking
// task. The bounded sizing pass stops at this limit; small requests avoid the
// scheduling hop. Count JSON too so large text/tool schemas also leave Tokio.
const BLOCKING_THRESHOLD: usize = 64 * 1024;
const EXCERPT_BYTES: usize = 4096;

pub(crate) fn attachment_data_url(media_type: &str, bytes: &[u8]) -> String {
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

#[cfg(test)]
thread_local! {
    pub(crate) static PROBE_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct SizeProbe {
    remaining: usize,
}

impl Write for SizeProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        PROBE_WRITES.with(|writes| writes.set(writes.get() + 1));
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

pub(crate) fn needs_blocking(request: &crate::support::LlmRequest) -> bool {
    // Traverse raw lengths before JSON's escaping scanner can touch a large
    // string. The traversal charges every node and stops at the same budget,
    // including for many empty fields or large byte sequences.
    if !raw_budget::RawBudget::fits(request, BLOCKING_THRESHOLD) {
        return true;
    }
    let mut remaining = BLOCKING_THRESHOLD;
    for bytes in request.resolved_stored.values() {
        let Some(rest) = remaining.checked_sub(bytes.len().max(1)) else {
            return true;
        };
        remaining = rest;
    }
    // The cache is deduplicated; materialization is not. Charge each inline
    // or stored occurrence for its final data URL, without allocating it.
    // Raw traversal above bounds this message/block walk as well.
    for block in request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
    {
        if let crate::support::LlmContentBlock::Attachment { source } = block
            && let Some(bytes) = request.attachment_bytes(source)
        {
            let expanded = base64::encoded_len(bytes.len(), true)
                .and_then(|len| len.checked_add(13))
                .and_then(|len| {
                    len.checked_add(source.media_type().map_or(0, |mime| mime.as_str().len()))
                });
            let Some(rest) = expanded.and_then(|len| remaining.checked_sub(len)) else {
                return true;
            };
            remaining = rest;
        }
    }
    // Only bounded, small raw fields reach the escaping JSON writer. Keep
    // its aggregate check for punctuation and escape expansion.
    serde_json::to_writer(&mut SizeProbe { remaining }, request).is_err()
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

/// Preserve small diagnostic JSON verbatim. Large diagnostics retain a
/// UTF-8-safe prefix plus the original byte count, never the full body.
pub(crate) fn body_excerpt(body: &str) -> String {
    if body.len() <= EXCERPT_BYTES {
        return body.to_owned();
    }
    format!("{}\n[body bytes: {}]", body_prefix(body), body.len())
}

pub(crate) fn body_prefix(body: &str) -> &str {
    let mut end = body.len().min(EXCERPT_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// Serialize diagnostics into a bounded writer rather than materializing a
/// full JSON String solely to truncate it on an error path.
pub(crate) fn json_excerpt(value: &serde_json::Value) -> String {
    struct ExcerptWriter {
        prefix: Vec<u8>,
        total: usize,
    }
    impl Write for ExcerptWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.total += bytes.len();
            let keep = bytes.len().min(EXCERPT_BYTES - self.prefix.len());
            self.prefix.extend_from_slice(&bytes[..keep]);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = ExcerptWriter {
        prefix: Vec::with_capacity(EXCERPT_BYTES),
        total: 0,
    };
    // Callers supply JSON Values, whose serialization cannot fail.
    serde_json::to_writer(&mut writer, value).expect("JSON diagnostic serialization");
    let prefix = match std::str::from_utf8(&writer.prefix) {
        Ok(prefix) => prefix,
        Err(error) => {
            std::str::from_utf8(&writer.prefix[..error.valid_up_to()]).expect("UTF-8 prefix")
        }
    };
    if writer.total <= EXCERPT_BYTES {
        prefix.to_owned()
    } else {
        format!("{prefix}\n[body bytes: {}]", writer.total)
    }
}

pub(crate) fn diagnostic_message(message: &str) -> String {
    if message.len() > EXCERPT_BYTES {
        body_excerpt(message)
    } else {
        message.to_owned()
    }
}

// Preserve the shared envelope's message and retry-delay classification even
// when those fields occur after a large echoed payload. Only this small
// projection is serialized for that classifier, never the original tree.
pub(crate) fn error_metadata(value: &serde_json::Value) -> Option<String> {
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
        metadata["error"] = json!({"message": diagnostic_message(message)});
    }
    if let Some(delay) = delay {
        metadata["retryDelay"] = Value::String(diagnostic_message(delay));
    }
    Some(metadata.to_string())
}

pub(crate) fn bytes_need_blocking(len: usize) -> bool {
    len > BLOCKING_THRESHOLD
}

/// Serialize directly to the transport buffer. Sizing before writing avoids
/// growth reallocations of large bodies; there is no second Value conversion.
pub(crate) fn serialize_body(body: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut probe = SizeProbe {
        remaining: usize::MAX,
    };
    serde_json::to_writer(&mut probe, body)?;
    let mut bytes = Vec::with_capacity(usize::MAX - probe.remaining);
    serde_json::to_writer(&mut bytes, body)?;
    Ok(bytes)
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
        let bytes = vec![255; BLOCKING_THRESHOLD * 2];
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
        assert_eq!(body_excerpt("{}"), "{}");
        assert_eq!(
            body_excerpt(&"x".repeat(EXCERPT_BYTES)),
            "x".repeat(EXCERPT_BYTES)
        );
        let body = "€".repeat(EXCERPT_BYTES);
        let error = crate::support::LlmTransportError::new("invalid response")
            .with_raw(body_excerpt(&body));
        let raw = error.raw.as_deref().unwrap();
        assert!(raw.len() <= EXCERPT_BYTES + 40);
        assert!(raw.ends_with("[body bytes: 12288]"));
        assert!(raw.starts_with('€'));
    }

    #[test]
    fn json_excerpt_counts_escaped_bytes_without_a_full_output_buffer() {
        let value = serde_json::json!({"text": "€\n".repeat(EXCERPT_BYTES)});
        let wire = serde_json::to_string(&value).unwrap();
        assert_eq!(json_excerpt(&value), body_excerpt(&wire));
    }

    #[test]
    fn direct_writer_preserves_wire_bytes() {
        let body = serde_json::json!({"z": "line\nquote\"", "a": [1, null, true]});
        assert_eq!(
            serialize_body(&body).unwrap(),
            br#"{"a":[1,null,true],"z":"line\nquote\""}"#
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_work_leaves_the_async_worker_and_small_work_stays_inline() {
        let worker = std::thread::current().id();
        assert_eq!(
            run(false, || std::thread::current().id()).await.unwrap(),
            worker
        );
        assert_ne!(
            run(true, || std::thread::current().id()).await.unwrap(),
            worker
        );
    }
}
