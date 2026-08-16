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
    /// Accept the request and go silent before any output, forcing the
    /// client's idle timeout.
    IdleBeforeStart,
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

/// Handle to a running scripted server. Dropping it aborts the accept loop.
pub struct ScriptedWsServer {
    /// `ws://…` URL to point [`crate::codex::CodexProvider::with_endpoint_urls`] at.
    pub url: String,
    captured: Arc<Mutex<Vec<Value>>>,
    captured_raw: Arc<Mutex<Vec<Vec<u8>>>>,
    handshakes: CapturedHandshakes,
    close_frames: Arc<Mutex<u32>>,
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
}

impl Drop for ScriptedWsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind a local WebSocket server that answers successive requests with
/// `actions`, capturing every request payload and handshake headers.
pub async fn spawn_scripted_websocket(actions: Vec<ScriptedWsAction>) -> ScriptedWsServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind ws");
    let addr = listener.local_addr().expect("ws addr");
    let actions = Arc::new(Mutex::new(VecDeque::from(actions)));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_raw = Arc::new(Mutex::new(Vec::new()));
    let handshakes = Arc::new(Mutex::new(Vec::new()));
    let close_frames = Arc::new(Mutex::new(0u32));
    let task_actions = Arc::clone(&actions);
    let task_captured = Arc::clone(&captured);
    let task_captured_raw = Arc::clone(&captured_raw);
    let task_handshakes = Arc::clone(&handshakes);
    let task_close_frames = Arc::clone(&close_frames);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
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
                        ScriptedWsAction::IdleBeforeStart => {
                            tokio::time::sleep(Duration::from_secs(60)).await;
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
