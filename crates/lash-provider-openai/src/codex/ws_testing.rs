//! Scripted local WebSocket server for Codex WebSocket tests.
//!
//! One harness, two consumers: the provider-layer unit tests in
//! [`crate::codex`] drive [`crate::codex::CodexProvider`] directly against it,
//! and the runtime-level test (`tests/codex_websocket_runtime.rs`) drives a
//! full facade turn (`LashCore` + `ProviderHandle`) over the same server via
//! [`crate::codex::CodexProvider::with_endpoint_urls`] and
//! [`crate::codex::CodexProvider::force_websocket_transport`].
//!
//! Compiled for unit tests and, behind the default-on `testing` feature, for
//! integration tests and downstream harnesses.

use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsHandshakeRequest, Response as WsHandshakeResponse,
};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

/// A Codex Responses assistant message item carrying `text`, as emitted in
/// `response.output_item.done` / `response.completed` payloads.
pub fn assistant_item(message_id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": message_id,
        "role": "assistant",
        "status": "completed",
        "phase": "final_answer",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

/// A Codex Responses `function_call` item, as emitted in
/// `response.output_item.done` / `response.completed` payloads.
pub fn function_call_item(call_id: &str, tool_name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call",
        "id": format!("fc_{call_id}"),
        "call_id": call_id,
        "name": tool_name,
        "arguments": arguments,
        "status": "completed"
    })
}

/// What the scripted server does in response to the next `response.create`
/// request it receives. Actions are consumed in order across connections.
#[derive(Clone, Debug)]
pub enum ScriptedWsAction {
    /// Send exact recorded provider frames. This is the fixture-matrix seam:
    /// the same Responses event payloads drive Codex SSE and WebSocket.
    RecordedFrames {
        frames: Vec<String>,
        close_after_frames: bool,
    },
    /// Accept `response.create`, then close before emitting any provider
    /// event. The outer retry owner must reset attempt-local accumulation.
    CloseBeforeStart,
    /// Stream a text delta, the completed message item, and
    /// `response.completed`.
    Complete {
        response_id: &'static str,
        message_id: &'static str,
        text: &'static str,
    },
    /// Like [`ScriptedWsAction::Complete`], then close the connection so a
    /// cached socket is dead on reuse.
    CompleteAndClose {
        response_id: &'static str,
        message_id: &'static str,
        text: &'static str,
    },
    /// Emit a completed `function_call` item and `response.completed`,
    /// terminating the turn iteration with a tool call.
    ToolCall {
        response_id: &'static str,
        call_id: &'static str,
        tool_name: &'static str,
        arguments: &'static str,
    },
    /// Terminal `response.completed` with `status: incomplete`.
    Incomplete {
        response_id: &'static str,
        message_id: &'static str,
        text: &'static str,
    },
    /// An `error` event before any output.
    Error { message: &'static str },
    /// Allocate an empty message item, then emit an error event.
    AllocationThenError {
        message_id: &'static str,
        message: &'static str,
    },
    /// Start streaming output, then emit an `error` event mid-stream.
    MidStreamError {
        message_id: &'static str,
        text: &'static str,
        message: &'static str,
    },
    /// Stream partial output and usage, then close cleanly without a terminal
    /// response event.
    CloseAfterStart {
        response_id: &'static str,
        message_id: &'static str,
        text: &'static str,
    },
    /// Accept the request, pause Tokio time, and signal the receiver before
    /// going silent. The receiver advances time only after the WebSocket
    /// request is captured, so connect cannot race the idle timeout.
    IdleBeforeStart { ready: Arc<Notify> },
    /// Stream partial output, then go silent, forcing the idle timeout after
    /// output started.
    IdleAfterStart {
        message_id: &'static str,
        text: &'static str,
    },
}

/// Captured request headers, one inner vec of `(name, value)` pairs per
/// WebSocket handshake the scripted server accepted.
pub type CapturedHandshakes = Arc<Mutex<Vec<Vec<(String, String)>>>>;

/// One synthetic `accept()` failure, injected into the scripted server's accept
/// loop in place of a real poll of the listener.
///
/// **Fault injection for [`spawn_scripted_websocket_with_injected_accept_faults`]
/// alone.** [`spawn_scripted_websocket`], the default path every other test
/// uses, injects nothing; a fault only ever exists because a test asked for one
/// by name. The synthetic failure yields no connection, exactly like a real
/// `accept()` that fails before handing one over. The property under test is
/// that the loop survives it and goes on serving.
#[derive(Clone, Copy, Debug)]
pub struct InjectedAcceptFault {
    /// How many connections the loop must already have accepted before this
    /// fault fires. `1` fires the fault in the gap between the first and the
    /// second handshake. A fault whose count is already past fires at the next
    /// poll, so it can never be stranded behind the connections it named.
    pub after_accepted_connections: usize,
    /// The error the synthetic `accept()` poll reports.
    pub kind: std::io::ErrorKind,
}

/// How many `accept()` failures in a row retire the scripted server's accept
/// loop.
///
/// No `accept()` error is treated as fatal on its own. Classifying one as fatal
/// is what produced FIG-1267: a loop that exits on a single error stops
/// answering handshakes for the rest of the test, and the failure then surfaces
/// as a client-side transport error on a later turn, saying nothing about the
/// harness. An allowlist of survivable errors does not fix that, it just moves
/// the trap — `accept(2)` reports pending-connection errnos (`EPROTO`,
/// `ENETDOWN`, `ENETUNREACH`, `EHOSTUNREACH`) that Rust maps to an
/// uncategorised `ErrorKind`, so any allowlist re-creates the same silent death
/// for them.
///
/// A count instead of a classification: only a listener that fails this many
/// times with no successful accept in between is treated as broken, and giving
/// up is recorded and panicked on so the failure names the harness. At the 1ms
/// retry pause this bounds the give-up at roughly 100ms — far inside the 5s
/// request timeout every consumer of this harness sets, so a genuinely dead
/// listener still fails the test promptly rather than hanging it.
pub const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 100;

/// Pause between accept retries, so a listener failing without blocking cannot
/// spin the runtime.
pub const ACCEPT_RETRY_PAUSE: Duration = Duration::from_millis(1);

/// Handle to a running scripted server. Dropping it aborts the accept loop.
pub struct ScriptedWsServer {
    /// `ws://…` URL to point [`crate::codex::CodexProvider::with_endpoint_urls`] at.
    pub url: String,
    captured: Arc<Mutex<Vec<Value>>>,
    captured_raw: Arc<Mutex<Vec<Vec<u8>>>>,
    handshakes: CapturedHandshakes,
    close_frames: Arc<Mutex<u32>>,
    accept_failure: Arc<Mutex<Option<String>>>,
    task: JoinHandle<()>,
}

impl ScriptedWsServer {
    /// Every JSON request the server received, in order.
    pub fn captured(&self) -> Vec<Value> {
        self.captured.lock_recover().clone()
    }

    /// Every request payload exactly as received from the WebSocket frame.
    pub fn captured_raw(&self) -> Vec<Vec<u8>> {
        self.captured_raw.lock_recover().clone()
    }

    /// Request headers per accepted WebSocket handshake, in order.
    pub fn handshakes(&self) -> Vec<Vec<(String, String)>> {
        self.handshakes.lock_recover().clone()
    }

    /// Number of WebSocket Close frames the server has received.
    pub fn close_frame_count(&self) -> u32 {
        *self.close_frames.lock_recover()
    }

    /// Why the accept loop gave up, if it did.
    ///
    /// `Some` means the server stopped listening after
    /// [`MAX_CONSECUTIVE_ACCEPT_FAILURES`] failures in a row, and every
    /// client-side failure after that point is a consequence of the dead
    /// harness rather than of the code under test. Assert on this before
    /// diagnosing a provider-side transport error.
    pub fn accept_failure(&self) -> Option<String> {
        self.accept_failure.lock_recover().clone()
    }
}

impl Drop for ScriptedWsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind a local WebSocket server that answers successive requests with
/// `actions`, capturing every request payload and handshake headers.
pub async fn spawn_scripted_websocket(actions: Vec<ScriptedWsAction>) -> ScriptedWsServer {
    spawn_scripted_websocket_with_injected_accept_faults(actions, Vec::new()).await
}

/// [`spawn_scripted_websocket`], with synthetic `accept()` failures injected into
/// the accept loop.
///
/// **For the accept-loop tests only.** Faults fire in the order given,
/// each at the point named by its
/// [`after_accepted_connections`](InjectedAcceptFault::after_accepted_connections);
/// pass an empty vec and the loop behaves exactly as
/// [`spawn_scripted_websocket`] does, which is what every other caller gets.
pub async fn spawn_scripted_websocket_with_injected_accept_faults(
    actions: Vec<ScriptedWsAction>,
    injected_accept_faults: Vec<InjectedAcceptFault>,
) -> ScriptedWsServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind ws");
    let addr = listener.local_addr().expect("ws addr");
    let actions = Arc::new(Mutex::new(VecDeque::from(actions)));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_raw = Arc::new(Mutex::new(Vec::new()));
    let handshakes = Arc::new(Mutex::new(Vec::new()));
    let close_frames = Arc::new(Mutex::new(0u32));
    let accept_failure = Arc::new(Mutex::new(None));
    let task_accept_failure = Arc::clone(&accept_failure);
    let task_actions = Arc::clone(&actions);
    let task_captured = Arc::clone(&captured);
    let task_captured_raw = Arc::clone(&captured_raw);
    let task_handshakes = Arc::clone(&handshakes);
    let task_close_frames = Arc::clone(&close_frames);
    let task = tokio::spawn(async move {
        let mut injected = VecDeque::from(injected_accept_faults);
        let mut accepted_connections = 0usize;
        let mut consecutive_failures = 0u32;
        loop {
            let due_fault = injected
                .front()
                .is_some_and(|fault| fault.after_accepted_connections <= accepted_connections)
                .then(|| injected.pop_front().expect("due fault"));
            let outcome = match due_fault {
                Some(fault) => Err(std::io::Error::from(fault.kind)),
                None => listener.accept().await,
            };
            let stream = match outcome {
                Ok((stream, _)) => stream,
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                        let reason = format!(
                            "scripted websocket server stopped listening: {consecutive_failures} \
                             consecutive accept failures, last was {error:?}. Any client failure \
                             after this point is the harness, not the code under test."
                        );
                        *task_accept_failure.lock_recover() = Some(reason.clone());
                        panic!("{reason}");
                    }
                    tokio::time::sleep(ACCEPT_RETRY_PAUSE).await;
                    continue;
                }
            };
            consecutive_failures = 0;
            accepted_connections += 1;
            let actions = Arc::clone(&task_actions);
            let captured = Arc::clone(&task_captured);
            let captured_raw = Arc::clone(&task_captured_raw);
            let handshakes = Arc::clone(&task_handshakes);
            let close_frames = Arc::clone(&task_close_frames);
            tokio::spawn(async move {
                #[expect(
                    clippy::result_large_err,
                    reason = "tungstenite fixes the handshake callback error to an HTTP response"
                )]
                let callback = move |request: &WsHandshakeRequest,
                                     response: WsHandshakeResponse| {
                    let headers = request
                        .headers()
                        .iter()
                        .filter_map(|(name, value)| {
                            value
                                .to_str()
                                .ok()
                                .map(|value| (name.as_str().to_string(), value.to_string()))
                        })
                        .collect::<Vec<_>>();
                    handshakes.lock_recover().push(headers);
                    Ok(response)
                };
                let Ok(mut ws) = accept_hdr_async(stream, callback).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    let text = match message {
                        WsMessage::Text(text) => text.to_string(),
                        WsMessage::Binary(bytes) => {
                            String::from_utf8(bytes.to_vec()).unwrap_or_default()
                        }
                        WsMessage::Close(_) => {
                            *close_frames.lock_recover() += 1;
                            break;
                        }
                        WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {
                            continue;
                        }
                    };
                    captured_raw.lock_recover().push(text.as_bytes().to_vec());
                    let request: Value = serde_json::from_str(&text).expect("ws request json");
                    captured.lock_recover().push(request);
                    let action = actions
                        .lock_recover()
                        .pop_front()
                        .expect("scripted ws action");
                    match action {
                        ScriptedWsAction::RecordedFrames {
                            frames,
                            close_after_frames,
                        } => {
                            for frame in frames {
                                ws.send(WsMessage::Text(frame.into()))
                                    .await
                                    .expect("send recorded ws frame");
                            }
                            if close_after_frames {
                                let _ = ws.close(None).await;
                                break;
                            }
                        }
                        ScriptedWsAction::CloseBeforeStart => {
                            let _ = ws.close(None).await;
                            break;
                        }
                        ScriptedWsAction::Complete {
                            response_id,
                            message_id,
                            text,
                        } => {
                            send_completed_ws_response(&mut ws, response_id, message_id, text)
                                .await;
                        }
                        ScriptedWsAction::CompleteAndClose {
                            response_id,
                            message_id,
                            text,
                        } => {
                            send_completed_ws_response(&mut ws, response_id, message_id, text)
                                .await;
                            let _ = ws.close(None).await;
                            break;
                        }
                        ScriptedWsAction::ToolCall {
                            response_id,
                            call_id,
                            tool_name,
                            arguments,
                        } => {
                            send_tool_call_ws_response(
                                &mut ws,
                                response_id,
                                call_id,
                                tool_name,
                                arguments,
                            )
                            .await;
                        }
                        ScriptedWsAction::Incomplete {
                            response_id,
                            message_id,
                            text,
                        } => {
                            send_incomplete_ws_response(&mut ws, response_id, message_id, text)
                                .await;
                        }
                        ScriptedWsAction::Error { message } => {
                            send_ws_json(
                                &mut ws,
                                json!({"type":"error","error":{"message": message}}),
                            )
                            .await;
                        }
                        ScriptedWsAction::AllocationThenError {
                            message_id,
                            message,
                        } => {
                            send_ws_json(
                                &mut ws,
                                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":message_id,"status":"in_progress","phase":"final_answer","content":[]}}),
                            )
                            .await;
                            send_ws_json(
                                &mut ws,
                                json!({"type":"error","error":{"message": message}}),
                            )
                            .await;
                        }
                        ScriptedWsAction::MidStreamError {
                            message_id,
                            text,
                            message,
                        } => {
                            send_ws_json(
                                &mut ws,
                                json!({"type":"response.output_text.delta","output_index":0,"item_id":message_id,"delta":text}),
                            )
                            .await;
                            send_ws_json(
                                &mut ws,
                                json!({"type":"error","error":{"message": message}}),
                            )
                            .await;
                        }
                        ScriptedWsAction::CloseAfterStart {
                            response_id,
                            message_id,
                            text,
                        } => {
                            send_ws_json(
                                &mut ws,
                                json!({"type":"response.created","response":{"id":response_id,"status":"in_progress","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}),
                            )
                            .await;
                            send_ws_json(
                                &mut ws,
                                json!({"type":"response.output_text.delta","output_index":0,"item_id":message_id,"delta":text}),
                            )
                            .await;
                            let _ = ws.close(None).await;
                            break;
                        }
                        ScriptedWsAction::IdleBeforeStart { ready } => {
                            tokio::time::pause();
                            ready.notify_one();
                            std::future::pending::<()>().await;
                        }
                        ScriptedWsAction::IdleAfterStart { message_id, text } => {
                            send_ws_json(
                                &mut ws,
                                json!({"type":"response.output_text.delta","output_index":0,"item_id":message_id,"delta":text}),
                            )
                            .await;
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }
                }
            });
        }
    });
    ScriptedWsServer {
        url: format!("ws://{addr}/codex/responses"),
        captured,
        captured_raw,
        handshakes,
        close_frames,
        accept_failure,
        task,
    }
}

async fn send_ws_json(ws: &mut WebSocketStream<TcpStream>, value: Value) {
    ws.send(WsMessage::Text(value.to_string().into()))
        .await
        .expect("send ws event");
}

async fn send_completed_ws_response(
    ws: &mut WebSocketStream<TcpStream>,
    response_id: &str,
    message_id: &str,
    text: &str,
) {
    let item = assistant_item(message_id, text);
    send_ws_json(
        ws,
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":message_id,"status":"in_progress","phase":"final_answer","content":[]}}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.output_text.delta","output_index":0,"item_id":message_id,"delta":text}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[assistant_item(message_id, text)],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    )
    .await;
}

async fn send_tool_call_ws_response(
    ws: &mut WebSocketStream<TcpStream>,
    response_id: &str,
    call_id: &str,
    tool_name: &str,
    arguments: &str,
) {
    let item = function_call_item(call_id, tool_name, arguments);
    send_ws_json(
        ws,
        json!({"type":"response.output_item.added","output_index":0,"item":item.clone()}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.output_item.done","output_index":0,"item":item.clone()}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    )
    .await;
}

async fn send_incomplete_ws_response(
    ws: &mut WebSocketStream<TcpStream>,
    response_id: &str,
    message_id: &str,
    text: &str,
) {
    let item = assistant_item(message_id, text);
    send_ws_json(
        ws,
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    )
    .await;
    send_ws_json(
        ws,
        json!({"type":"response.completed","response":{"id":response_id,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[assistant_item(message_id, text)],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    )
    .await;
}
