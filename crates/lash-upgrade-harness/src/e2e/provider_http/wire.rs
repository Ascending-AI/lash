//! Bounded HTTP/1 fixture plumbing, with real chunk framing and disconnects.

use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

const MAX_HEADER: usize = 32 * 1024;
const MAX_BODY: usize = 1024 * 1024;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// Routing diagnostics only: never matched, never recorded as evidence.
    pub host: Option<String>,
    pub user_agent: Option<String>,
    pub body: Value,
}

/// Reads one request. `tolerate_incomplete` belongs to the product-host
/// binding: gate infrastructure opens and drops listening sockets, so a bound
/// fixture treats a connection that ends before a complete head as no request
/// (`Ok(None)`). Literal callers pass `false`: an early EOF stays
/// "HTTP request ended before headers" and a mid-head read error propagates.
/// Once a head completes, malformed headers or a truncated body are real
/// errors either way.
pub(crate) async fn read(
    stream: &mut TcpStream,
    tolerate_incomplete: bool,
) -> Result<Option<Request>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let head_end = loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            ensure!(end <= MAX_HEADER, "HTTP header too large");
            break end + 4;
        }
        ensure!(bytes.len() <= MAX_HEADER, "HTTP header too large");
        match stream.read(&mut buffer).await {
            Ok(0) if tolerate_incomplete => return Ok(None),
            Ok(0) => return Err(anyhow!("HTTP request ended before headers")),
            Err(error) => {
                if tolerate_incomplete {
                    return Ok(None);
                }
                return Err(error.into());
            }
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end])?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    let method = start.next().unwrap_or_default().to_owned();
    let path = start.next().unwrap_or_default().to_owned();
    ensure!(
        start.next() == Some("HTTP/1.1") && start.next().is_none(),
        "invalid HTTP request line"
    );
    let mut length = None;
    let mut host = None;
    let mut user_agent = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid HTTP header"))?;
        ensure!(
            !name.eq_ignore_ascii_case("transfer-encoding"),
            "fixture requires a bounded Content-Length request"
        );
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        } else if name.eq_ignore_ascii_case("host") {
            host = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("user-agent") {
            user_agent = Some(value.trim().to_owned());
        }
    }
    let length = length.unwrap_or(0);
    ensure!(length <= MAX_BODY, "HTTP request body too large");
    while bytes.len() - head_end < length {
        let count = stream.read(&mut buffer).await?;
        ensure!(count != 0, "HTTP request body incomplete");
        bytes.extend_from_slice(&buffer[..count]);
    }
    ensure!(
        bytes.len() - head_end == length,
        "unexpected pipelined HTTP bytes"
    );
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[head_end..])?
    };
    // All other request headers stay out: neither credentials nor unrelated
    // host headers enter fixture evidence.
    Ok(Some(Request {
        method,
        path,
        host,
        user_agent,
        body,
    }))
}

pub(crate) async fn head(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
) -> Result<()> {
    let mut bytes =
        format!("HTTP/1.1 {status} Fixture\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n");
    for (name, value) in headers {
        bytes.push_str(&format!("{name}: {value}\r\n"));
    }
    bytes.push_str("\r\n");
    stream.write_all(bytes.as_bytes()).await?;
    Ok(())
}

pub(crate) async fn chunk(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    stream
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await?;
    stream.write_all(bytes).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await?;
    Ok(())
}

pub(crate) async fn end(stream: &mut TcpStream) -> Result<()> {
    stream.write_all(b"0\r\n\r\n").await?;
    stream.shutdown().await?;
    Ok(())
}

pub(crate) async fn json(stream: &mut TcpStream, status: u16, value: &Value) -> Result<()> {
    head(
        stream,
        status,
        &[("content-type".into(), "application/json".into())],
    )
    .await?;
    chunk(stream, &serde_json::to_vec(value)?).await?;
    end(stream).await
}
