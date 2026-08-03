use lash_core::{ProviderFailureKind, facade_support::LlmTransportError};
use lash_sansio::llm::types::{LlmEventSender, LlmStreamEvent, LlmUsage};

use std::time::Duration;

use crate::http::LlmHttpBody;

struct SseBuffer {
    /// Raw, not-yet-newline-terminated bytes. Held at the byte level so a
    /// multi-byte UTF-8 codepoint split across two network chunks is decoded
    /// once it is complete, never lossily decoded into U+FFFD per half.
    pending: Vec<u8>,
    event_data: String,
}

impl SseBuffer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            event_data: String::new(),
        }
    }

    fn push_chunk<F>(&mut self, chunk: &[u8], mut on_event: F) -> Result<(), LlmTransportError>
    where
        F: FnMut(&str) -> Result<(), LlmTransportError>,
    {
        self.pending.extend_from_slice(chunk);
        // Split on `\n` at the byte level and decode each complete line. A
        // partial trailing line (which may end mid-codepoint) stays in
        // `pending` until the rest of it arrives in a later chunk.
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line_bytes: Vec<u8> = self.pending.drain(..=pos).collect();
            line_bytes.pop(); // drop the trailing '\n'
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes);
            if self.consume_line(&line)? {
                self.flush_event(&mut on_event)?;
            }
        }
        Ok(())
    }

    /// Consume one complete SSE line, returning `true` when it terminates an
    /// event (a blank line) and the accumulated `event_data` should be flushed.
    fn consume_line(&mut self, line: &str) -> Result<bool, LlmTransportError> {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !self.event_data.is_empty() {
                self.event_data.push('\n');
            }
            self.event_data.push_str(data);
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
                if !self.event_data.is_empty() {
                    self.event_data.push('\n');
                }
                self.event_data.push_str(data);
            }
        }
        self.pending.clear();
        self.flush_event(&mut on_event)
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

pub async fn drive_sse_response<F>(
    body: LlmHttpBody,
    timeout: Duration,
    read_timeout_message: &str,
    mut on_event: F,
) -> Result<(), LlmTransportError>
where
    F: FnMut(&str) -> Result<(), LlmTransportError>,
{
    let mut buffer = SseBuffer::new();
    let mut stream = match body {
        LlmHttpBody::Buffered(bytes) => {
            buffer.push_chunk(&bytes, &mut on_event)?;
            return buffer.finish(on_event);
        }
        LlmHttpBody::Streamed(stream) => stream,
    };
    loop {
        let chunk_result = match tokio::time::timeout(timeout, stream.next_chunk()).await {
            Ok(result) => result,
            Err(_) => {
                // Read timed out: flush whatever event is already buffered so a
                // terminal event sent without a trailing blank line isn't lost,
                // then surface the timeout.
                let _ = buffer.finish(&mut on_event);
                return Err(LlmTransportError::new(read_timeout_message)
                    .with_kind(ProviderFailureKind::Timeout)
                    .with_code("timeout")
                    .retryable(true));
            }
        };
        let chunk_opt = match chunk_result {
            Ok(chunk_opt) => chunk_opt,
            Err(mut error) => {
                // Mid-stream disconnect: flush the buffered event before
                // propagating, so a final event delivered without a trailing
                // blank line still reaches the caller.
                let _ = buffer.finish(&mut on_event);
                if let Some(rest) = error.message.strip_prefix("HTTP response read failed: ") {
                    error.message = format!("Stream read failed: {rest}");
                }
                return Err(error);
            }
        };
        let Some(chunk) = chunk_opt else { break };
        buffer.push_chunk(&chunk, &mut on_event)?;
    }
    buffer.finish(on_event)
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

    fn collect_events(chunks: &[&[u8]]) -> Vec<String> {
        let mut buffer = SseBuffer::new();
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
            "stream chunk timed out",
            |_event| Ok(()),
        )
        .await;

        let err = result.expect_err("stream read should time out");
        assert_eq!(err.kind, ProviderFailureKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert!(err.retryable);
    }
}
