//! A reusable WebSocket cache slot is owned from the moment it is reserved until
//! its lease is released: a dropped acquisition frees it, and a stale owner never
//! removes or overwrites a reservation that replaced it.

use super::*;
use lash_core::llm::types::ResponseContext;
use lash_llm_transport::TokenLease;

/// Accepts every TCP connection; the first never completes its WebSocket
/// handshake, every later one does.
struct StallFirstHandshake {
    url: String,
    stalled: Arc<Notify>,
    _accept_loop: JoinHandle<()>,
}

impl StallFirstHandshake {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind handshake listener");
        let url = format!("ws://{}", listener.local_addr().expect("listener address"));
        let stalled = Arc::new(Notify::new());
        let notify = Arc::clone(&stalled);
        let accept_loop = tokio::spawn(async move {
            let mut held_streams = Vec::new();
            let mut held_sockets = Vec::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if held_streams.is_empty() {
                    held_streams.push(stream);
                    notify.notify_one();
                    continue;
                }
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    held_sockets.push(socket);
                }
            }
        });
        Self {
            url,
            stalled,
            _accept_loop: accept_loop,
        }
    }
}

fn slot(provider: &CodexProvider, scope_key: &str) -> Option<String> {
    let sessions = provider.websocket_sessions.inner.lock_recover();
    sessions.by_scope.get(scope_key).map(|entry| match entry {
        CodexWebsocketSessionEntry::Reserved { token_epoch, .. } => {
            format!("reserved:{token_epoch}")
        }
        CodexWebsocketSessionEntry::Idle { token_epoch, .. } => format!("idle:{token_epoch}"),
    })
}

fn token(epoch: u64) -> TokenLease {
    TokenLease {
        token: lash_core::provider::ProviderToken::new("access"),
        epoch,
    }
}

async fn acquire(
    provider: &CodexProvider,
    call: &ResponseContext,
    epoch: u64,
) -> CodexWebsocketLease {
    match provider.acquire_websocket(call, None, &token(epoch)).await {
        Ok(lease) => lease,
        Err(error) => panic!("acquire failed: {}", error.error.message),
    }
}

#[tokio::test]
async fn codex_websocket_reservation_is_owned_from_insertion_until_release() {
    let server = StallFirstHandshake::spawn().await;
    let provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        server.url.clone(),
    );
    let call =
        ResponseContext::of_request(&request(vec![LlmMessage::text(LlmRole::User, "hello")]));
    let scope_key = call.continuation_key();

    // Drop the acquisition while its reusable handshake is pending.
    {
        let token = token(0);
        let acquiring = provider.acquire_websocket(&call, None, &token);
        tokio::pin!(acquiring);
        tokio::select! {
            _ = &mut acquiring => panic!("the first handshake never completes"),
            () = server.stalled.notified() => {}
        }
        assert_eq!(slot(&provider, &scope_key).as_deref(), Some("reserved:0"));
    }
    assert_eq!(
        slot(&provider, &scope_key),
        None,
        "a dropped acquisition releases the reservation it made"
    );

    // The next same-scope call owns a reusable slot; a concurrent one stays ephemeral.
    let owner = acquire(&provider, &call, 0).await;
    assert!(owner.reusable(), "the freed scope is reusable again");
    let concurrent = acquire(&provider, &call, 0).await;
    assert!(
        !concurrent.reusable(),
        "a concurrent call never steals the slot"
    );
    provider.release_websocket_lease(concurrent, true, None);
    assert_eq!(slot(&provider, &scope_key).as_deref(), Some("reserved:0"));

    // A rotated token replaces the reservation; the old-token owner cannot touch it.
    let rotated = acquire(&provider, &call, 1).await;
    assert!(rotated.reusable());
    provider.release_websocket_lease(owner, true, None);
    assert_eq!(slot(&provider, &scope_key).as_deref(), Some("reserved:1"));

    // After a drain, a same-token replacement is not overwritten by the older owner.
    provider.close_websocket_sessions().await;
    let replacement = acquire(&provider, &call, 1).await;
    assert!(replacement.reusable());
    provider.release_websocket_lease(rotated, true, None);
    assert_eq!(
        slot(&provider, &scope_key).as_deref(),
        Some("reserved:1"),
        "an older same-token owner must not overwrite its replacement"
    );
    provider.release_websocket_lease(replacement, true, None);
    assert_eq!(slot(&provider, &scope_key).as_deref(), Some("idle:1"));
    let reused = acquire(&provider, &call, 1).await;
    assert!(reused.reused, "the released connection is reused");
}
