use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::providers::BenchmarkStreamProfile;

/// Configured pacing and capacity, never measured delay or capacity.
#[derive(Clone, Debug)]
pub(crate) struct HttpFixturePlan {
    pub first_chunk_delay: Duration,
    pub chunk_delay: Duration,
    pub chunk_bytes: usize,
    pub server_parallel: usize,
}
impl Default for HttpFixturePlan {
    fn default() -> Self {
        Self {
            first_chunk_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
            chunk_bytes: usize::MAX,
            server_parallel: 64,
        }
    }
}

pub(crate) struct OpenAiCompatBenchServer {
    pub(crate) base_url: String,
    shutdown: CancellationToken,
    accept_task: tokio::task::JoinHandle<()>,
}

impl OpenAiCompatBenchServer {
    pub(crate) async fn start(profile: BenchmarkStreamProfile) -> anyhow::Result<Self> {
        Self::start_paced(profile, HttpFixturePlan::default()).await
    }

    pub(crate) async fn start_paced(
        profile: BenchmarkStreamProfile,
        plan: HttpFixturePlan,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            plan.chunk_bytes > 0 && plan.server_parallel > 0,
            "empty HTTP fixture capacity"
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind local OpenAI-compatible perf server")?;
        let address = listener
            .local_addr()
            .context("resolve local OpenAI-compatible perf server address")?;
        let response_body = Arc::new(openai_compat_sse_body(&profile));
        let shutdown = CancellationToken::new();
        let accept_shutdown = shutdown.clone();
        let slots = Arc::new(tokio::sync::Semaphore::new(plan.server_parallel));
        let retried = Arc::new(Mutex::new(HashSet::new()));
        let accept_task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = accept_shutdown.cancelled() => break,
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        let body = Arc::clone(&response_body);
                        let plan = plan.clone();
                        let slots = slots.clone();
                        let retried = retried.clone();
                        connections.spawn(async move {
                            let _ = serve_openai_compat_connection(stream, body, plan, slots, retried).await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{address}/v1"),
            shutdown,
            accept_task,
        })
    }
}

impl Drop for OpenAiCompatBenchServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.accept_task.abort();
    }
}

pub(crate) fn openai_compat_sse_body(profile: &BenchmarkStreamProfile) -> Vec<u8> {
    let mut body = String::new();
    for delta in &profile.deltas {
        body.push_str("data: ");
        body.push_str(
            &serde_json::json!({
                "id": "chatcmpl-runtime-perf",
                "object": "chat.completion.chunk",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": delta,
                    },
                    "finish_reason": null,
                }],
            })
            .to_string(),
        );
        body.push_str("\n\n");
    }
    body.push_str("data: ");
    body.push_str(
        &serde_json::json!({
            "id": "chatcmpl-runtime-perf",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }],
        })
        .to_string(),
    );
    body.push_str("\n\n");
    body.push_str("data: ");
    body.push_str(
        &serde_json::json!({
            "id": "chatcmpl-runtime-perf",
            "object": "chat.completion.chunk",
            "choices": [],
            "usage": {
                "prompt_tokens": 1024,
                "completion_tokens": 64,
                "prompt_tokens_details": {
                    "cached_tokens": 512,
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 48,
                }
            },
        })
        .to_string(),
    );
    body.push_str("\n\n");
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

async fn serve_openai_compat_connection(
    mut stream: tokio::net::TcpStream,
    response_body: Arc<Vec<u8>>,
    plan: HttpFixturePlan,
    slots: Arc<tokio::sync::Semaphore>,
    retried: Arc<Mutex<HashSet<String>>>,
) -> io::Result<()> {
    let headers = drain_http_request(&mut stream).await?;
    let retry_key = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-lash-perf-retry")
            .then(|| value.trim().to_owned())
    });
    let retry = retry_key.is_some_and(|key| {
        retried
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
    });
    let _slot = slots.acquire().await.map_err(io::Error::other)?;
    let failure = b"{\"error\":{\"message\":\"scripted transient refusal\"}}";
    let body = if retry {
        failure.as_slice()
    } else {
        response_body.as_slice()
    };
    let status = if retry {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    let content_type = if retry {
        "application/json"
    } else {
        "text/event-stream"
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !plan.first_chunk_delay.is_zero() {
        tokio::time::sleep(plan.first_chunk_delay).await;
    }
    let mut chunks = body.chunks(plan.chunk_bytes).peekable();
    while let Some(chunk) = chunks.next() {
        stream.write_all(chunk).await?;
        if chunks.peek().is_some() && !plan.chunk_delay.is_zero() {
            tokio::time::sleep(plan.chunk_delay).await;
        }
    }
    stream.shutdown().await
}

async fn drain_http_request(stream: &mut tokio::net::TcpStream) -> io::Result<String> {
    let mut headers = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut content_length = None;
    let mut header_len = None;
    let mut total_read = 0usize;
    let mut request_headers = String::new();
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete HTTP request",
            ));
        }
        total_read += read;
        if header_len.is_none() {
            headers.extend_from_slice(&chunk[..read]);
        }
        if header_len.is_none()
            && let Some(index) = find_bytes(&headers, b"\r\n\r\n")
        {
            let end = index + 4;
            let header_text = String::from_utf8_lossy(&headers);
            header_len = Some(end);
            content_length = Some(parse_content_length(&header_text).unwrap_or(0));
            request_headers = header_text[..end].to_owned();
            headers.clear();
        }
        if let (Some(header_len), Some(content_length)) = (header_len, content_length)
            && total_read >= header_len + content_length
        {
            return Ok(request_headers);
        }
    }
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
