use std::future::Future;
use std::sync::Arc;

use futures_util::SinkExt;
use rmcp::model::ErrorData;
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError, TransportWriter};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, FramedWrite};

/// Partial bytes belong to the reader, so dropping a receive future cannot
/// discard them when the SDK selects a concurrent send completion.
struct LineReader<R> {
    read: BufReader<R>,
    line: BytesMut,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    async fn next_line(&mut self) -> std::io::Result<bool> {
        loop {
            let available = self.read.fill_buf().await?;
            if available.is_empty() {
                return Ok(!self.line.is_empty());
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
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
}

impl<R, W> ManagedChildTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    pub(crate) fn new(read: R, write: W) -> Self {
        Self {
            reader: LineReader {
                read: BufReader::new(read),
                line: BytesMut::new(),
            },
            writer: Arc::new(Mutex::new(Some(FramedWrite::new(
                write,
                JsonRpcMessageCodec::default(),
            )))),
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
