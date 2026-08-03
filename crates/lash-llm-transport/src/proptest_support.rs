//! Reusable proptest strategies and scripted stream fakes for SSE property
//! tests. Compiled only with the `proptest-support` feature; provider crates
//! enable it as a dev-dependency feature to assert that parsing a valid SSE
//! byte stream is invariant under how the bytes are split into chunks.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lash_core::facade_support::LlmTransportError;
use proptest::prelude::*;

use crate::http::{LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};

/// Partition `bytes` into an arbitrary sequence of chunks that concatenates
/// back to the input, including splits mid-UTF-8-codepoint, mid-line, and
/// mid-frame. Duplicate cut points yield occasional empty chunks, which
/// [`crate::streaming::drive_sse_response`] tolerates: only a `None` chunk
/// terminates the stream, so an empty `Some` chunk simply carries no bytes.
pub fn chunk_partitions(bytes: Vec<u8>) -> impl Strategy<Value = Vec<Vec<u8>>> {
    let len = bytes.len();
    prop::collection::vec(0..=len, 0..8).prop_map(move |mut cuts| {
        cuts.sort_unstable();
        let mut chunks = Vec::with_capacity(cuts.len() + 1);
        let mut start = 0;
        for cut in cuts {
            chunks.push(bytes[start..cut].to_vec());
            start = cut;
        }
        chunks.push(bytes[start..].to_vec());
        chunks
    })
}

/// Single-line SSE payload text: printable ASCII plus multibyte UTF-8
/// codepoints (2-byte accents, 3-byte CJK, 4-byte emoji) so chunk splits can
/// land inside a codepoint. Never contains `\n` or `\r`, which would end the
/// line on the wire.
pub fn sse_data_payload() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            proptest::char::range(' ', '~'),
            Just('é'),
            Just('汉'),
            Just('字'),
            Just('日'),
            Just('本'),
            Just('😀'),
            Just('🚀'),
        ],
        0..24,
    )
    .prop_map(|chars: Vec<char>| chars.into_iter().collect())
}

fn sse_line_ending() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("\n"), Just("\r\n")]
}

fn sse_data_line() -> impl Strategy<Value = String> {
    (
        prop_oneof![Just("data: "), Just("data:")],
        sse_data_payload(),
        sse_line_ending(),
    )
        .prop_map(|(prefix, payload, eol)| format!("{prefix}{payload}{eol}"))
}

fn sse_comment_line() -> impl Strategy<Value = String> {
    (sse_data_payload(), sse_line_ending()).prop_map(|(payload, eol)| format!(": {payload}{eol}"))
}

fn sse_event_name_line() -> impl Strategy<Value = String> {
    ("[a-z_]{1,12}", sse_line_ending()).prop_map(|(name, eol)| format!("event: {name}{eol}"))
}

/// One complete SSE event: optional `event:` name and comment lines, one or
/// more `data:` lines (joined with `\n` when framed), and a terminating blank
/// line, mixing CRLF and LF line endings.
pub fn sse_event() -> impl Strategy<Value = String> {
    (
        prop::option::of(sse_comment_line()),
        prop::option::of(sse_event_name_line()),
        prop::collection::vec(sse_data_line(), 1..4),
        sse_line_ending(),
    )
        .prop_map(|(comment, event_name, data_lines, terminator)| {
            let mut event = String::new();
            if let Some(comment) = comment {
                event.push_str(&comment);
            }
            if let Some(event_name) = event_name {
                event.push_str(&event_name);
            }
            for line in data_lines {
                event.push_str(&line);
            }
            event.push_str(terminator);
            event
        })
}

/// A valid SSE byte stream: a sequence of complete events.
pub fn sse_stream() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(sse_event(), 0..5).prop_map(|events| events.concat().into_bytes())
}

/// [`LlmByteStream`] that replays a scripted chunk sequence and then ends.
#[derive(Debug)]
pub struct ScriptedByteStream {
    chunks: VecDeque<Bytes>,
}

impl ScriptedByteStream {
    pub fn new<I>(chunks: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Bytes>,
    {
        Self {
            chunks: chunks.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl LlmByteStream for ScriptedByteStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        Ok(self.chunks.pop_front())
    }
}

/// [`LlmHttpTransport`] that answers every request with a `200` whose body
/// streams a scripted chunk sequence. Lets provider property tests drive the
/// real `complete` pipeline over an arbitrarily re-chunked SSE stream.
#[derive(Debug)]
pub struct ScriptedSseTransport {
    chunks: Vec<Bytes>,
}

impl ScriptedSseTransport {
    pub fn new<I>(chunks: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Bytes>,
    {
        Self {
            chunks: chunks.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl LlmHttpTransport for ScriptedSseTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: LlmHttpBody::streamed(ScriptedByteStream::new(self.chunks.clone())),
        })
    }
}
