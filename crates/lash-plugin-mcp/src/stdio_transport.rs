use std::future::Future;
use std::sync::Arc;

use futures_util::SinkExt;
use lash_sansio::sync::MutexExt;
use rmcp::model::ErrorData;
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError, TransportWriter};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, FramedWrite};

/// Upper bound on one inbound stdio message. A line's bytes accumulate before
/// its JSON is decoded, so without a cap a malfunctioning child grows the
/// receive buffer without limit.
pub(crate) const MAX_STDIO_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Why reading the child's stdout ended. rmcp's `receive` reports only
/// `Option`, so the transport records the cause on [`StdioCloseCause`] for the
/// lifecycle actor to name when the service quits.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StdioReadError {
    #[error("MCP stdio read failed: {0}")]
    Io(#[from] std::io::Error),
    /// The child produced a message past [`MAX_STDIO_MESSAGE_BYTES`]; the
    /// transport closes rather than keep accumulating bytes it can never
    /// decode.
    #[error("MCP stdio inbound message exceeded the {limit}-byte limit")]
    MessageTooLarge { limit: usize },
}

/// Receive-side close cause shared between the transport (written once, on
/// the rmcp service task) and the lifecycle actor (read when the service's
/// `waiting` reports a quit).
pub(crate) type StdioCloseCause = Arc<std::sync::Mutex<Option<StdioReadError>>>;

/// A fresh, unwritten close-cause cell for one stdio transport.
pub(crate) fn stdio_close_cause() -> StdioCloseCause {
    Arc::new(std::sync::Mutex::new(None))
}

/// Partial bytes belong to the reader, so dropping a receive future cannot
/// discard them when the SDK selects a concurrent send completion.
struct LineReader<R> {
    read: BufReader<R>,
    line: BytesMut,
    max_line_bytes: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    async fn next_line(&mut self) -> Result<bool, StdioReadError> {
        loop {
            let available = self.read.fill_buf().await?;
            if available.is_empty() {
                return Ok(!self.line.is_empty());
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if self.line.len() + consumed > self.max_line_bytes {
                return Err(StdioReadError::MessageTooLarge {
                    limit: self.max_line_bytes,
                });
            }
            self.line.extend_from_slice(&available[..consumed]);
            self.read.consume(consumed);
            if newline.is_some() {
                return Ok(true);
            }
        }
    }
}

pub(crate) struct ManagedChildTransport<R: AsyncRead, W: AsyncWrite> {
    reader: LineReader<R>,
    writer: Arc<Mutex<Option<TransportWriter<RoleClient, W>>>>,
    close_cause: StdioCloseCause,
}

impl<R, W> ManagedChildTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    pub(crate) fn new(read: R, write: W, close_cause: StdioCloseCause) -> Self {
        Self {
            reader: LineReader {
                read: BufReader::new(read),
                line: BytesMut::new(),
                max_line_bytes: MAX_STDIO_MESSAGE_BYTES,
            },
            writer: Arc::new(Mutex::new(Some(FramedWrite::new(
                write,
                JsonRpcMessageCodec::default(),
            )))),
            close_cause,
        }
    }
}

impl<R, W> Transport<RoleClient> for ManagedChildTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = self.writer.clone();
        async move {
            let mut writer = writer.lock().await;
            match writer.as_mut() {
                Some(writer) => writer.send(item).await.map_err(Into::into),
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "Transport is closed",
                )),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            match self.reader.next_line().await {
                Ok(false) => return None,
                Ok(true) => {}
                Err(error) => {
                    tracing::error!(%error, "Error reading from MCP stdio");
                    *self.close_cause.lock_recover() = Some(error);
                    // Sever writes at once; rmcp only learns of this quit as
                    // `None`, so the cause above is what names it.
                    drop(self.writer.lock().await.take());
                    return None;
                }
            }
            let line = self
                .reader
                .line
                .strip_suffix(b"\n")
                .unwrap_or(&self.reader.line);
            if line.strip_suffix(b"\r").unwrap_or(line).is_empty() {
                self.reader.line.clear();
                continue;
            }
            // Reuse the SDK's BOM, CRLF and notification compatibility rules.
            // Decode only complete lines, including an unterminated EOF line.
            let parsed = JsonRpcMessageCodec::default().decode_eof(&mut self.reader.line);
            self.reader.line.clear();
            match parsed {
                Ok(Some(message)) => return Some(message),
                Ok(None) => continue,
                Err(JsonRpcMessageCodecError::Serde(error)) => {
                    tracing::debug!(%error, "Parse error on incoming MCP message");
                    let response = TxJsonRpcMessage::<RoleClient>::error(
                        ErrorData::parse_error("Parse error", None),
                        None,
                    );
                    if self.send(response).await.is_err() {
                        return None;
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "Error decoding MCP stdio");
                    return None;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        drop(self.writer.lock().await.take());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
