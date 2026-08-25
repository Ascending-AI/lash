use lash_core::provider::ProviderOptions;
use lash_core::{
    ProviderFailureKind, facade_support::LlmTransportError, llm::transport::TransportRetryVerdict,
};
use lash_sansio::llm::types::{LlmEventSender, LlmStreamEvent, LlmUsage};

use std::time::Duration;
use tokio::time::Instant;

use crate::http::LlmHttpBody;
use crate::response_metadata::ResponseMetadataCapture;

pub const DEFAULT_SSE_EVENT_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_SSE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct SseStreamBounds {
    absolute_deadline: Option<Instant>,
    event_bytes: usize,
    total_bytes: usize,
}

impl SseStreamBounds {
    /// Capture the whole-request deadline before the HTTP send. The returned
    /// value must be threaded unchanged into [`drive_sse_response`].
    pub fn new(request_timeout: Option<Duration>, options: &ProviderOptions) -> Self {
        Self {
            absolute_deadline: request_timeout.map(|timeout| Instant::now() + timeout),
            event_bytes: configured_limit(options.sse_event_bytes, DEFAULT_SSE_EVENT_BYTES),
            total_bytes: configured_limit(options.sse_total_bytes, DEFAULT_SSE_TOTAL_BYTES),
        }
    }
}

fn configured_limit(configured: Option<u64>, default: u64) -> usize {
    let value = configured.filter(|value| *value > 0).unwrap_or(default);
    usize::try_from(value).unwrap_or(usize::MAX)
}

struct SseBuffer {
    /// Raw, not-yet-newline-terminated bytes. Held at the byte level so a
    /// multi-byte UTF-8 codepoint split across two network chunks is decoded
    /// once it is complete, never lossily decoded into U+FFFD per half.
    pending: Vec<u8>,
    event_data: String,
    event_bytes: usize,
    total_bytes: usize,
    received_bytes: usize,
}

impl SseBuffer {
    fn new(bounds: SseStreamBounds) -> Self {
        Self {
            pending: Vec::new(),
            event_data: String::new(),
            event_bytes: bounds.event_bytes,
            total_bytes: bounds.total_bytes,
            received_bytes: 0,
        }
    }

    fn push_chunk<F>(&mut self, chunk: &[u8], mut on_event: F) -> Result<(), LlmTransportError>
    where
        F: FnMut(&str) -> Result<(), LlmTransportError>,
    {
        self.received_bytes = self
            .received_bytes
            .checked_add(chunk.len())
            .filter(|received| *received <= self.total_bytes)
            .ok_or_else(total_limit_error)?;

        // Split on `\n` at the byte level before extending `pending`, so one
        // large network chunk containing many small events never requires an
        // equally large accumulation buffer. A partial trailing line (which
        // may end mid-codepoint) stays pending until the next chunk.
        let mut remaining = chunk;
        while let Some(pos) = remaining.iter().position(|&b| b == b'\n') {
            let (line, rest) = remaining.split_at(pos + 1);
            self.extend_pending(line)?;
            let mut line_bytes = std::mem::take(&mut self.pending);
            line_bytes.pop(); // drop the trailing '\n'
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes);
            if self.consume_line(&line)? {
                self.flush_event(&mut on_event)?;
            }
            remaining = rest;
        }
        self.extend_pending(remaining)?;
        Ok(())
    }

    fn extend_pending(&mut self, bytes: &[u8]) -> Result<(), LlmTransportError> {
        if self
            .pending
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.event_bytes)
        {
            return Err(event_limit_error());
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    /// Consume one complete SSE line, returning `true` when it terminates an
    /// event (a blank line) and the accumulated `event_data` should be flushed.
    fn consume_line(&mut self, line: &str) -> Result<bool, LlmTransportError> {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            self.append_event_data(data)?;
            Ok(false)
        } else if line.starts_with("event:") {
            Ok(false)
        } else {
            Ok(line.trim().is_empty())
        }
    }

    fn finish<F>(&mut self, mut on_event: F) -> Result<(), LlmTransportError>
    where
        F: FnMut(&str) -> Result<(), LlmTransportError>,
    {
        {
            let pending = String::from_utf8_lossy(&self.pending);
            let pending = pending.trim();
            if !pending.is_empty()
                && let Some(data) = pending.strip_prefix("data:")
            {
                let data = data.trim();
                let data = data.to_string();
                self.append_event_data(&data)?;
            }
        }
        self.pending.clear();
        self.flush_event(&mut on_event)
    }

    fn append_event_data(&mut self, data: &str) -> Result<(), LlmTransportError> {
        let separator_bytes = usize::from(!self.event_data.is_empty());
        if self
            .event_data
            .len()
            .checked_add(separator_bytes)
            .and_then(|len| len.checked_add(data.len()))
            .is_none_or(|len| len > self.event_bytes)
        {
            return Err(event_limit_error());
        }
        if separator_bytes > 0 {
            self.event_data.push('\n');
        }
        self.event_data.push_str(data);
        Ok(())
    }

    fn flush_event<F>(&mut self, on_event: &mut F) -> Result<(), LlmTransportError>
    where
        F: FnMut(&str) -> Result<(), LlmTransportError>,
    {
        if self.event_data.is_empty() {
            return Ok(());
        }
        let result = on_event(self.event_data.as_str());
        self.event_data.clear();
        result
    }
}

fn event_limit_error() -> LlmTransportError {
    LlmTransportError::new("SSE event exceeded the configured byte limit")
        .with_kind(ProviderFailureKind::Stream)
        .with_code("sse_event_too_large")
        .with_retry_verdict(TransportRetryVerdict::NotRetryable)
}

fn total_limit_error() -> LlmTransportError {
    LlmTransportError::new("SSE response exceeded the configured total byte limit")
        .with_kind(ProviderFailureKind::Stream)
        .with_code("sse_response_too_large")
        .with_retry_verdict(TransportRetryVerdict::NotRetryable)
}

fn timeout_error(message: &str) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Timeout)
        .with_code("timeout")
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
}

pub async fn drive_sse_response<F>(
    body: LlmHttpBody,
    chunk_timeout: Duration,
    bounds: SseStreamBounds,
    read_timeout_message: &str,
    request_timeout_message: &str,
    response_metadata: &mut ResponseMetadataCapture,
    mut on_event: F,
) -> Result<(), LlmTransportError>
where
    F: FnMut(&str) -> Result<(), LlmTransportError>,
{
    let mut capture_then_emit = |event: &str| {
        response_metadata.capture_sse_event(event);
        on_event(event)
    };
    let mut buffer = SseBuffer::new(bounds);
    let mut stream = match body {
        LlmHttpBody::Buffered(bytes) => {
            if bounds
                .absolute_deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                return Err(timeout_error(request_timeout_message));
            }
            buffer.push_chunk(&bytes, &mut capture_then_emit)?;
            return buffer.finish(capture_then_emit);
        }
        LlmHttpBody::Streamed(stream) => stream,
    };
    loop {
        let chunk_deadline = Instant::now() + chunk_timeout;
        let (read_deadline, absolute_deadline_wins) = match bounds.absolute_deadline {
            Some(absolute_deadline) if absolute_deadline <= chunk_deadline => {
                (absolute_deadline, true)
            }
            _ => (chunk_deadline, false),
        };
        let chunk_result = match tokio::time::timeout_at(read_deadline, stream.next_chunk()).await {
            Ok(result) => result,
            Err(_) => {
                // Read timed out: flush whatever event is already buffered so a
                // terminal event sent without a trailing blank line isn't lost,
                // then surface the timeout.
                let _ = buffer.finish(&mut capture_then_emit);
                let message = if absolute_deadline_wins {
                    request_timeout_message
                } else {
                    read_timeout_message
                };
                return Err(timeout_error(message));
            }
        };
        let chunk_opt = match chunk_result {
            Ok(chunk_opt) => chunk_opt,
            Err(mut error) => {
                // Mid-stream disconnect: flush the buffered event before
                // propagating, so a final event delivered without a trailing
                // blank line still reaches the caller.
                let _ = buffer.finish(&mut capture_then_emit);
                if let Some(rest) = error.message.strip_prefix("HTTP response read failed: ") {
                    error.message = format!("Stream read failed: {rest}");
                }
                return Err(error);
            }
        };
        let Some(chunk) = chunk_opt else { break };
        buffer.push_chunk(&chunk, &mut capture_then_emit)?;
    }
    buffer.finish(capture_then_emit)
}

pub fn emit_stream_progress(
    tx: Option<&LlmEventSender>,
    added_deltas: impl IntoIterator<Item = String>,
    usage: &LlmUsage,
    prev_usage: &LlmUsage,
) {
    let Some(tx) = tx else {
        return;
    };
    if usage != prev_usage && usage != &LlmUsage::default() {
        tx.send(LlmStreamEvent::Usage(usage.clone()));
    }
    for piece in added_deltas {
        tx.send(LlmStreamEvent::Delta(piece));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(event_bytes: usize, total_bytes: usize) -> SseStreamBounds {
        SseStreamBounds {
            absolute_deadline: None,
            event_bytes,
            total_bytes,
        }
    }

    fn collect_events(chunks: &[&[u8]]) -> Vec<String> {
        let mut buffer = SseBuffer::new(bounds(
            DEFAULT_SSE_EVENT_BYTES as usize,
            DEFAULT_SSE_TOTAL_BYTES as usize,
        ));
        let mut events: Vec<String> = Vec::new();
        for chunk in chunks {
            buffer
                .push_chunk(chunk, |event| {
                    events.push(event.to_string());
                    Ok(())
                })
                .unwrap();
        }
        buffer
            .finish(|event| {
                events.push(event.to_string());
                Ok(())
            })
            .unwrap();
        events
    }

    #[test]
    fn multibyte_codepoint_split_across_chunks_is_not_corrupted() {
        // "data: é\n\n" where the two UTF-8 bytes of 'é' (0xC3 0xA9) straddle
        // the chunk boundary. The old per-chunk from_utf8_lossy turned each
        // half into U+FFFD; the byte-level buffer must reassemble it intact.
        let full = "data: é\n\n".as_bytes();
        let split = full.len() - 4; // between 0xC3 and 0xA9 of 'é'
        let events = collect_events(&[&full[..split], &full[split..]]);
        assert_eq!(events, vec!["é".to_string()]);
        assert!(
            !events.iter().any(|e| e.contains('\u{FFFD}')),
            "no replacement char expected, got {events:?}"
        );
    }

    #[test]
    fn multibyte_codepoint_split_in_data_payload_round_trips() {
        // A 4-byte emoji (😀 = 0xF0 0x9F 0x98 0x80) split mid-codepoint.
        let full = "data: hi 😀 there\n\n".as_bytes();
        let split = 10; // lands inside the emoji's byte sequence
        let events = collect_events(&[&full[..split], &full[split..]]);
        assert_eq!(events, vec!["hi 😀 there".to_string()]);
    }

    #[test]
    fn complete_event_in_single_chunk_still_flushes() {
        let events = collect_events(&[b"data: hello\n\n"]);
        assert_eq!(events, vec!["hello".to_string()]);
    }

    #[test]
    fn finish_flushes_trailing_event_without_blank_line() {
        // Terminal event delivered without the trailing blank line: finish()
        // must still surface it (covers the mid-stream-disconnect flush path).
        let events = collect_events(&[b"data: done"]);
        assert_eq!(events, vec!["done".to_string()]);
    }

    #[test]
    fn multiline_data_fields_join_with_newline() {
        let events = collect_events(&[b"data: a\ndata: b\n\n"]);
        assert_eq!(events, vec!["a\nb".to_string()]);
    }

    #[derive(Debug)]
    struct PendingStream;

    #[async_trait::async_trait]
    impl crate::http::LlmByteStream for PendingStream {
        async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn stream_chunk_timeout_uses_timeout_envelope() {
        let result = drive_sse_response(
            LlmHttpBody::streamed(PendingStream),
            Duration::from_millis(1),
            bounds(1024, 4096),
            "stream chunk timed out",
            "request timed out",
            &mut ResponseMetadataCapture::default(),
            |_event| Ok(()),
        )
        .await;

        let err = result.expect_err("stream read should time out");
        assert_eq!(err.kind, ProviderFailureKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert!(err.is_retryable());
    }

    #[derive(Debug)]
    struct SlowKeepaliveStream {
        remaining: usize,
    }

    #[async_trait::async_trait]
    impl crate::http::LlmByteStream for SlowKeepaliveStream {
        async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            tokio::time::sleep(Duration::from_secs(119)).await;
            Ok(Some(bytes::Bytes::from_static(b": keepalive\n\n")))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_keepalives_cannot_extend_the_absolute_request_deadline() {
        let mut stream_bounds = bounds(1024, 4096);
        stream_bounds.absolute_deadline = Some(Instant::now() + Duration::from_secs(300));
        let result = drive_sse_response(
            LlmHttpBody::streamed(SlowKeepaliveStream { remaining: 4 }),
            Duration::from_secs(120),
            stream_bounds,
            "stream chunk timed out",
            "request timed out",
            &mut ResponseMetadataCapture::default(),
            |_event| Ok(()),
        )
        .await;

        let err = result.expect_err("absolute request deadline must win");
        assert_eq!(err.kind, ProviderFailureKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert_eq!(err.message, "request timed out");
        assert!(err.is_retryable());
    }

    #[test]
    fn newline_free_body_hits_the_event_buffer_cap() {
        let mut buffer = SseBuffer::new(bounds(16, 64));
        let err = buffer
            .push_chunk(b"12345678901234567", |_event| Ok(()))
            .expect_err("unterminated input must be capped");

        assert_eq!(err.kind, ProviderFailureKind::Stream);
        assert_eq!(err.code.as_deref(), Some("sse_event_too_large"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn multiline_event_data_hits_the_event_buffer_cap() {
        let mut buffer = SseBuffer::new(bounds(16, 64));
        let err = buffer
            .push_chunk(b"data: 12345678\ndata: 12345678\n\n", |_event| Ok(()))
            .expect_err("assembled event data must be capped");

        assert_eq!(err.kind, ProviderFailureKind::Stream);
        assert_eq!(err.code.as_deref(), Some("sse_event_too_large"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn response_total_hits_the_total_byte_cap() {
        let mut buffer = SseBuffer::new(bounds(64, 16));
        let err = buffer
            .push_chunk(b"data: 12345678901", |_event| Ok(()))
            .expect_err("total response input must be capped");

        assert_eq!(err.kind, ProviderFailureKind::Stream);
        assert_eq!(err.code.as_deref(), Some("sse_response_too_large"));
        assert!(!err.is_retryable());
    }
}
