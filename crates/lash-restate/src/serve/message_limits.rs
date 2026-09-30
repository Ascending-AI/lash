use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame};

const HEADER_BYTES: usize = 8;
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Host-selected limits for incoming Restate service-protocol messages.
///
/// The message limit counts payload bytes; the pending limit includes the
/// eight-byte header. Both apply to each message, never to the complete replay
/// stream. A pending limit smaller than eight refuses every message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestateEndpointLimits {
    max_message_bytes: u32,
    max_pending_bytes: u64,
}

impl RestateEndpointLimits {
    pub(super) const fn max_pending_bytes(self) -> u64 {
        self.max_pending_bytes
    }

    /// Select the largest payload and incomplete framed message this host admits.
    /// A smaller pending budget tightens the effective message limit.
    pub const fn new(max_message_bytes: u32, max_pending_bytes: u64) -> Self {
        Self {
            max_message_bytes,
            max_pending_bytes,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Restate message declares {declared} payload bytes; host limits are {message_limit} payload bytes and {pending_limit} pending framed bytes"
)]
struct MessageLimitError {
    declared: u32,
    message_limit: u32,
    pending_limit: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("Restate message header state is inconsistent")]
struct HeaderStateError;

struct MessageHeaders {
    limits: RestateEndpointLimits,
    header: [u8; HEADER_BYTES],
    header_bytes: usize,
    payload_remaining: u32,
}

impl MessageHeaders {
    fn new(limits: RestateEndpointLimits) -> Self {
        Self {
            limits,
            header: [0; HEADER_BYTES],
            header_bytes: 0,
            payload_remaining: 0,
        }
    }

    fn check_length(&self, declared: u32) -> Result<(), MessageLimitError> {
        if declared > self.limits.max_message_bytes
            || u64::from(declared) + HEADER_BYTES as u64 > self.limits.max_pending_bytes
        {
            return Err(MessageLimitError {
                declared,
                message_limit: self.limits.max_message_bytes,
                pending_limit: self.limits.max_pending_bytes,
            });
        }
        Ok(())
    }

    fn scan(&mut self, mut chunk: &[u8]) -> Result<(), BoxError> {
        while !chunk.is_empty() {
            if self.payload_remaining != 0 {
                let consumed = chunk.len().min(self.payload_remaining as usize);
                self.payload_remaining -= consumed as u32;
                chunk = chunk.get(consumed..).ok_or(HeaderStateError)?;
                continue;
            }
            let available = HEADER_BYTES
                .checked_sub(self.header_bytes)
                .ok_or(HeaderStateError)?;
            let consumed = chunk.len().min(available);
            let (part, remaining) = chunk.split_at_checked(consumed).ok_or(HeaderStateError)?;
            self.header
                .get_mut(self.header_bytes..)
                .and_then(|header| header.get_mut(..consumed))
                .ok_or(HeaderStateError)?
                .copy_from_slice(part);
            self.header_bytes += consumed;
            chunk = remaining;
            if self.header_bytes == HEADER_BYTES {
                // The low 32 bits are the payload length in protocol V6 and V7.
                // The SDK remains the owner of message types, flags and protobuf.
                let declared = u32::from_be_bytes(
                    self.header
                        .get(4..)
                        .ok_or(HeaderStateError)?
                        .try_into()
                        .map_err(|_| HeaderStateError)?,
                );
                self.check_length(declared)?;
                self.payload_remaining = declared;
                self.header_bytes = 0;
            }
        }
        Ok(())
    }
}

/// Checks every header in a transport chunk before giving that chunk to the
/// SDK. Only a fixed eight-byte header and counters are retained here. The
/// SDK's incomplete-message buffer cannot grow beyond an admitted declaration.
/// Hyper separately bounds its HTTP/2 DATA frames and flow-control window.
pub(super) struct MessageBody<B> {
    inner: B,
    headers: MessageHeaders,
    finished: bool,
}

impl<B> MessageBody<B> {
    pub(super) fn new(inner: B, limits: RestateEndpointLimits) -> Self {
        Self {
            inner,
            headers: MessageHeaders::new(limits),
            finished: false,
        }
    }
}

impl<B> Body for MessageBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Err(error) = self.headers.check_length(0) {
            self.finished = true;
            return Poll::Ready(Some(Err(Box::new(error))));
        }
        match std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let Some(chunk) = frame.data_ref()
                    && let Err(error) = self.headers.scan(chunk)
                {
                    self.finished = true;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(error)) => {
                self.finished = true;
                Poll::Ready(Some(Err(error.into())))
            }
            None => {
                self.finished = true;
                if self.headers.header_bytes != 0 || self.headers.payload_remaining != 0 {
                    Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "incomplete Restate message",
                    )
                    .into())))
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }
}
