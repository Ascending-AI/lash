use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rmcp::service::{RoleClient, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::sync::Notify;

use super::ManagedChildTransport;

struct ObservedRead<R> {
    inner: R,
    consumed: Arc<AtomicUsize>,
    expected: Arc<AtomicUsize>,
    waiting: Arc<Notify>,
}

impl<R: AsyncRead + Unpin> AsyncRead for ObservedRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        self.consumed
            .fetch_add(buf.filled().len() - before, Ordering::SeqCst);
        if result.is_pending()
            && self.consumed.load(Ordering::SeqCst) == self.expected.load(Ordering::SeqCst)
        {
            self.waiting.notify_one();
        }
        result
    }
}

async fn cancel_receive_on_send<R, W>(transport: &mut ManagedChildTransport<R, W>, waiting: &Notify)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let notification: TxJsonRpcMessage<RoleClient> = serde_json::from_value(json!({
        "jsonrpc": "2.0", "method": "notifications/roots/list_changed"
    }))
    .expect("client notification");
    let send = transport.send(notification);
    let send_completion = async {
        waiting.notified().await;
        send.await.expect("concurrent send completes");
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        // Match the SDK's receive/send-completion race, but order the polls so
        // the send cannot complete until receive consumed the partial line.
        tokio::select! {
            biased;
            message = transport.receive() => panic!("receive completed before the second chunk: {message:?}"),
            () = send_completion => {}
        }
    })
    .await
    .expect("receive consumed the first chunk before send completion");
}

async fn split_response(chunks: &[&[u8]], newline: bool) {
    let (client, server) = tokio::io::duplex(65_536);
    let (read, write) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let consumed = Arc::new(AtomicUsize::new(0));
    let expected = Arc::new(AtomicUsize::new(0));
    let waiting = Arc::new(Notify::new());
    let mut transport = ManagedChildTransport::new(
        ObservedRead {
            inner: read,
            consumed: consumed.clone(),
            expected: expected.clone(),
            waiting: waiting.clone(),
        },
        write,
    );
    let mut server_read = BufReader::new(server_read);
    let mut total = 0;
    for chunk in chunks {
        server_write.write_all(chunk).await.expect("response chunk");
        total += chunk.len();
        expected.store(total, Ordering::SeqCst);
        cancel_receive_on_send(&mut transport, &waiting).await;
        assert_eq!(consumed.load(Ordering::SeqCst), total);
        let mut sent = Vec::new();
        server_read
            .read_until(b'\n', &mut sent)
            .await
            .expect("sent notification");
        assert_eq!(
            serde_json::from_slice::<Value>(&sent).expect("sent JSON")["method"],
            "notifications/roots/list_changed"
        );
    }
    if newline {
        server_write
            .write_all(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n")
            .await
            .expect("newline and subsequent response");
    }
    server_write.shutdown().await.expect("server EOF");
    let response = tokio::time::timeout(Duration::from_secs(5), transport.receive())
        .await
        .expect("split response arrives")
        .expect("split response retained");
    let expected_response: Value = serde_json::from_slice(&chunks.concat()).expect("response JSON");
    assert_eq!(
        serde_json::to_value(response).expect("received JSON"),
        expected_response
    );
    if newline {
        let next = transport
            .receive()
            .await
            .expect("subsequent response retained");
        assert_eq!(
            serde_json::to_value(next).expect("next JSON"),
            json!({"jsonrpc":"2.0","id":2,"result":{}})
        );
    }
    assert!(
        transport.receive().await.is_none(),
        "no duplicated response"
    );
    transport.close().await.expect("transport closes");
    let notification = serde_json::from_value(json!({
        "jsonrpc":"2.0", "method":"notifications/roots/list_changed"
    }))
    .expect("notification");
    assert_eq!(
        transport
            .send(notification)
            .await
            .expect_err("closed sender")
            .kind(),
        std::io::ErrorKind::NotConnected
    );
}

#[tokio::test]
async fn split_response_survives_concurrent_send_completion() {
    let response = serde_json::to_vec(&json!({
        "jsonrpc":"2.0", "id":1, "result":{"text":"é".repeat(12_000)}
    }))
    .expect("large UTF-8 response");
    split_response(&[&response], true).await;
}

#[tokio::test]
async fn repeated_cancellation_preserves_split_utf8_response() {
    let response = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"text\":\"\xc3\xa9\"}}";
    let split = response
        .iter()
        .position(|byte| *byte == 0xa9)
        .expect("UTF-8 continuation");
    split_response(
        &[&response[..10], &response[10..split], &response[split..]],
        true,
    )
    .await;
}

#[tokio::test]
async fn cancelled_receive_preserves_partial_response_at_eof() {
    split_response(&[b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"], false).await;
}

#[tokio::test]
async fn framing_preserves_sdk_compatibility_and_parse_error_recovery() {
    let (client, mut server) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client);
    let mut transport = ManagedChildTransport::new(read, write);
    server.write_all(b"\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/vendor\"}\nnot json\n\xff\n\xef\xbb\xbf{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n")
        .await.expect("compatibility frames");
    server.shutdown().await.expect("server EOF");
    let vendor = transport
        .receive()
        .await
        .expect("vendor notification retained");
    assert_eq!(
        serde_json::to_value(vendor).expect("vendor JSON"),
        json!({"jsonrpc":"2.0","method":"notifications/vendor","params":null})
    );
    for id in [1, 2] {
        let message = transport
            .receive()
            .await
            .expect("valid response after invalid frame");
        assert_eq!(
            serde_json::to_value(message).expect("response JSON"),
            json!({"jsonrpc":"2.0","id":id,"result":{}})
        );
    }
    let mut server = BufReader::new(server);
    for _ in 0..2 {
        let mut error = Vec::new();
        server
            .read_until(b'\n', &mut error)
            .await
            .expect("parse error reply");
        assert_eq!(
            serde_json::from_slice::<Value>(&error).expect("error JSON"),
            json!({"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"}})
        );
    }
    assert!(transport.receive().await.is_none());
}
