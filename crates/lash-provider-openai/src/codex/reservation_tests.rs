//! A reusable WebSocket cache slot is owned from the moment it is reserved until
//! its lease is released: a dropped acquisition frees it, and a stale owner never
//! removes or overwrites a reservation that replaced it.

use super::*;
use lash_core::llm::types::ResponseContext;
use lash_llm_transport::TokenLease;

/// Accepts every TCP connection and completes its WebSocket handshake.
struct HandshakeServer {
    url: String,
    _accept_loop: JoinHandle<()>,
}

impl HandshakeServer {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind handshake listener");
        let url = format!("ws://{}", listener.local_addr().expect("listener address"));
        let accept_loop = tokio::spawn(async move {
            let mut held_sockets = Vec::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    held_sockets.push(socket);
                }
            }
        });
        Self {
            url,
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

/// Ownership at the claim (FIG-6852): a reusable acquisition dropped at each
/// await of its handshake, or completed and its lease dropped, leaves no
/// `Reserved` slot behind, and the next same-scope call owns a reusable slot.
#[tokio::test]
async fn a_websocket_acquisition_dropped_at_each_await_frees_its_reservation() {
    let server = HandshakeServer::spawn().await;
    let call =
        ResponseContext::of_request(&request(vec![LlmMessage::text(LlmRole::User, "hello")]));
    let scope_key = call.continuation_key();
    lash_core::cancel_sweep::sweep_drops(
        async || {
            let provider = websocket_test_provider(
                CodexTransport::WebsocketCached,
                "http://127.0.0.1:9/unused".to_string(),
                server.url.clone(),
            );
            let (acquirer, call) = (provider.clone(), call.clone());
            let acquiring = async move { acquire(&acquirer, &call, 0).await };
            (provider, acquiring)
        },
        async |provider, cut| {
            assert_eq!(
                slot(&provider, &scope_key),
                None,
                "no reservation remains once the acquisition is {cut}"
            );
            let next = acquire(&provider, &call, 0).await;
            assert!(
                next.reusable(),
                "the next call owns the scope once the acquisition is {cut}"
            );
        },
    )
    .await;
}

/// A stale owner never removes or overwrites a reservation that replaced it.
#[tokio::test]
async fn codex_websocket_reservation_is_owned_from_insertion_until_release() {
    let server = HandshakeServer::spawn().await;
    let provider = websocket_test_provider(
        CodexTransport::WebsocketCached,
        "http://127.0.0.1:9/unused".to_string(),
        server.url.clone(),
    );
    let call =
        ResponseContext::of_request(&request(vec![LlmMessage::text(LlmRole::User, "hello")]));
    let scope_key = call.continuation_key();

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

fn hello() -> LlmRequest {
    request(vec![LlmMessage::text(LlmRole::User, "hello")])
}

async fn complete_once(
    provider: &CodexProvider,
) -> Result<LlmResponse, lash_transport_failure::LlmTransportError> {
    let mut running = provider.clone();
    running
        .complete(
            hello(),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
}

/// Ownership at the claim: a streaming attempt owns its lease from the moment
/// it is acquired, so a completion dropped at each await, connecting, sending
/// or mid-stream, closes its connection and frees its slot instead of caching
/// a half-read socket; only a completed stream is cached for reuse, and the
/// next call completes.
#[tokio::test]
async fn a_websocket_completion_dropped_at_each_await_never_caches_its_connection() {
    let complete = || ScriptedWsAction::Complete {
        response_id: "resp_swept",
        message_id: "msg_swept",
        text: "swept",
    };
    lash_core::cancel_sweep::sweep_drops(
        async || {
            let server = spawn_scripted_websocket(vec![complete(), complete(), complete()]).await;
            let provider = websocket_test_provider(
                CodexTransport::WebsocketCached,
                "http://127.0.0.1:9/unused".to_string(),
                server.url.clone(),
            );
            let running = provider.clone();
            let completing = async move {
                complete_once(&running).await.expect("the swept completion");
            };
            ((server, provider), completing)
        },
        async |(server, provider), cut| {
            let scope_key = ResponseContext::of_request(&hello()).continuation_key();
            let cached = slot(&provider, &scope_key);
            if cut.completed {
                assert!(
                    cached
                        .as_deref()
                        .is_some_and(|slot| slot.starts_with("idle:")),
                    "a completed stream is cached for reuse: {cached:?}"
                );
            } else {
                assert_eq!(
                    cached, None,
                    "nothing is cached or reserved once the completion is {cut}"
                );
            }
            complete_once(&provider).await.unwrap_or_else(|error| {
                panic!(
                    "the next call completes once the completion is {cut}: {error:?} ({:?})",
                    server.accept_failure()
                )
            });
        },
    )
    .await;
}
