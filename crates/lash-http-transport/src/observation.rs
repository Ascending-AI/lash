//! Content-free, opt-in HTTP phase observation at the transport seam.
//!
//! Give each logical provider call its own ledger, then join the 1-based HTTP
//! ordinal to that call's sealed attempt records. This seam sees built requests,
//! headers and body delivery, not DNS/TLS internals or visible model tokens.
use crate::{
    ByteStream, HttpRequest, HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyEnd {
    Eof,
    Failed,
    Aborted,
}

/// Point samples in nanoseconds since the ledger's monotonic epoch, in its
/// observer process. Byte totals are body bytes offered/successfully delivered,
/// excluding HTTP headers, framing, compression on the wire and TLS.
#[derive(Clone, Debug)]
pub struct HttpPhaseRecord {
    pub ordinal: usize,
    pub request_built_ns: u64,
    pub headers_received_ns: Option<u64>,
    /// First nonempty transport chunk delivered to the body consumer.
    pub first_chunk_ns: Option<u64>,
    pub end_ns: Option<u64>,
    pub request_body_bytes: usize,
    pub response_body_bytes: usize,
    pub status: Option<u16>,
    pub end: Option<BodyEnd>,
}

#[derive(Debug)]
struct State {
    rows: Vec<HttpPhaseRecord>,
    dropped: usize,
    next: usize,
}

#[derive(Clone, Debug)]
pub struct HttpPhaseLedger {
    epoch: Instant,
    capacity: usize,
    state: Arc<Mutex<State>>,
}
impl HttpPhaseLedger {
    pub fn new(capacity: usize) -> Self {
        Self::with_epoch(capacity).0
    }
    /// Returns the exact epoch so a host can translate samples to its own
    /// process-local monotonic window without matching separate clock reads.
    pub fn with_epoch(capacity: usize) -> (Self, Instant) {
        let epoch = Instant::now();
        (
            Self {
                epoch,
                capacity,
                state: Arc::new(Mutex::new(State {
                    rows: Vec::new(),
                    dropped: 0,
                    next: 1,
                })),
            },
            epoch,
        )
    }
    pub fn snapshot(&self) -> (Vec<HttpPhaseRecord>, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.rows.clone(), state.dropped)
    }
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    }
    fn begin(&self, request: &HttpRequest) -> Recording {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ordinal = state.next;
        state.next += 1;
        let index = if state.rows.len() < self.capacity {
            let index = state.rows.len();
            state.rows.push(HttpPhaseRecord {
                ordinal,
                request_built_ns: self.now(),
                headers_received_ns: None,
                first_chunk_ns: None,
                end_ns: None,
                request_body_bytes: request.body.len(),
                response_body_bytes: 0,
                status: None,
                end: None,
            });
            Some(index)
        } else {
            state.dropped += 1;
            None
        };
        Recording {
            ledger: self.clone(),
            index,
        }
    }
}

struct Recording {
    ledger: HttpPhaseLedger,
    index: Option<usize>,
}
impl fmt::Debug for Recording {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HTTP phase recording")
    }
}
impl Recording {
    fn update(&self, update: impl FnOnce(&mut HttpPhaseRecord, u64)) {
        if let Some(index) = self.index {
            let mut state = self
                .ledger
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let row = &mut state.rows[index];
            if row.end.is_none() {
                update(row, self.ledger.now());
            }
        }
    }
    fn finish(&self, end: BodyEnd) {
        self.update(|row, now| {
            row.end = Some(end);
            row.end_ns = Some(now);
        });
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        self.finish(BodyEnd::Aborted);
    }
}

#[derive(Debug)]
pub struct ObservedHttpTransport {
    inner: Arc<dyn HttpTransport>,
    ledger: HttpPhaseLedger,
}
impl ObservedHttpTransport {
    pub fn new(inner: Arc<dyn HttpTransport>, ledger: HttpPhaseLedger) -> Self {
        Self { inner, ledger }
    }
}
#[async_trait]
impl HttpTransport for ObservedHttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let recording = self.ledger.begin(&request);
        let mut response = match self.inner.send(request, timeout).await {
            Ok(response) => response,
            Err(error) => {
                recording.finish(BodyEnd::Failed);
                return Err(error);
            }
        };
        recording.update(|row, now| {
            row.headers_received_ns = Some(now);
            row.status = Some(response.status);
        });
        let inner: Box<dyn ByteStream> = match response.body {
            HttpResponseBody::Streamed(stream) => stream,
            HttpResponseBody::Buffered(bytes) => Box::new(BufferedStream(Some(bytes))),
        };
        response.body = HttpResponseBody::streamed(ObservedStream { inner, recording });
        Ok(response)
    }
}
#[derive(Debug)]
struct BufferedStream(Option<Bytes>);
#[async_trait]
impl ByteStream for BufferedStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        Ok(self.0.take())
    }
}
#[derive(Debug)]
struct ObservedStream {
    inner: Box<dyn ByteStream>,
    recording: Recording,
}
#[async_trait]
impl ByteStream for ObservedStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        let result = self.inner.next_chunk().await;
        match &result {
            Ok(Some(bytes)) => self.recording.update(|row, now| {
                if !bytes.is_empty() {
                    row.first_chunk_ns.get_or_insert(now);
                }
                row.response_body_bytes = row.response_body_bytes.saturating_add(bytes.len());
            }),
            Ok(None) => self.recording.finish(BodyEnd::Eof),
            Err(_) => self.recording.finish(BodyEnd::Failed),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct Fixture;
    #[async_trait]
    impl HttpTransport for Fixture {
        async fn send(
            &self,
            _: HttpRequest,
            _: Option<Duration>,
        ) -> Result<HttpResponse, LlmTransportError> {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: HttpResponseBody::buffered("hello"),
            })
        }
    }
    #[tokio::test]
    async fn http_phases_are_ordered_and_partial_bodies_stay_partial() {
        let ledger = HttpPhaseLedger::new(3);
        let transport = ObservedHttpTransport::new(Arc::new(Fixture), ledger.clone());
        let response = transport
            .send(HttpRequest::post("http://fixture", "abc"), None)
            .await
            .unwrap();
        let body = crate::read_http_body_bytes(response.body, 5, None, "fixture")
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"hello");
        let response = transport
            .send(HttpRequest::post("http://fixture", "abc"), None)
            .await
            .unwrap();
        // This exercises the existing body budget, with observation attached.
        assert!(
            crate::read_http_body_bytes(response.body, 4, None, "fixture")
                .await
                .is_err()
        );
        let response = transport
            .send(HttpRequest::post("http://fixture", "abc"), None)
            .await
            .unwrap();
        drop(response);
        let (rows, dropped) = ledger.snapshot();
        assert_eq!(dropped, 0);
        let row = &rows[0];
        assert_eq!(row.ordinal, 1);
        assert_eq!(row.request_body_bytes, 3);
        assert_eq!(row.response_body_bytes, 5);
        assert_eq!(row.end, Some(BodyEnd::Eof));
        assert!(row.request_built_ns <= row.headers_received_ns.unwrap());
        assert!(row.headers_received_ns.unwrap() <= row.first_chunk_ns.unwrap());
        assert!(row.first_chunk_ns.unwrap() <= row.end_ns.unwrap());
        assert_eq!(rows[1].end, Some(BodyEnd::Aborted));
        assert_eq!(rows[2].end, Some(BodyEnd::Aborted));
        assert!(rows[2].first_chunk_ns.is_none());
        let response = transport
            .send(HttpRequest::post("http://fixture", "abc"), None)
            .await
            .unwrap();
        drop(response);
        assert_eq!(ledger.snapshot().1, 1);
    }
}
