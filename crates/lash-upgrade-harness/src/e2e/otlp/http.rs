use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::{Mode, State, decode};

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;

pub(super) async fn serve(
    listener: TcpListener,
    mut mode: watch::Receiver<Mode>,
    state: Arc<State>,
) -> Result<()> {
    let mut connections = JoinSet::new();
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            changed = mode.changed() => {
                changed.context("collector control channel closed")?;
                if *mode.borrow() == Mode::Stopped { break; }
            }
            connection = listener.accept() => {
                let (stream, _) = connection.context("accept OTLP connection")?;
                connections.spawn(handle(stream, mode.clone(), state.clone()));
            }
            result = connections.join_next(), if !connections.is_empty() => {
                match result.context("collector connection set became empty")? {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => { failure.get_or_insert(error); }
                    Err(error) => { failure.get_or_insert(error.into()); }
                }
                state.changed.notify_waiters();
            }
        }
    }
    drop(listener);
    while let Some(result) = connections.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                failure.get_or_insert(error);
            }
            Err(error) => {
                failure.get_or_insert(error.into());
            }
        }
    }
    state.changed.notify_waiters();
    failure.map_or(Ok(()), Err)
}

async fn handle(
    mut stream: TcpStream,
    mut mode: watch::Receiver<Mode>,
    state: Arc<State>,
) -> Result<()> {
    if *mode.borrow_and_update() == Mode::Disconnected {
        disconnected(&state).await;
        return Ok(());
    }
    if *mode.borrow() == Mode::Stopped {
        return Ok(());
    }
    tokio::select! {
        changed = mode.changed() => {
            changed.context("collector control closed with a stream open")?;
            if *mode.borrow() == Mode::Disconnected { disconnected(&state).await; }
            Ok(())
        }
        result = tokio::time::timeout(Duration::from_secs(10), receive(&mut stream, &state)) => {
            result.context("OTLP request deadline")?
        }
    }
}

async fn disconnected(state: &State) {
    state.receipt.lock().await.disconnected_requests += 1;
    state.changed.notify_waiters();
}

async fn receive(stream: &mut TcpStream, state: &State) -> Result<()> {
    let mut bytes = Vec::new();
    let mut block = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        ensure!(
            bytes.len() < MAX_HEADERS,
            "OTLP headers exceed fixture limit"
        );
        let read = stream.read(&mut block).await?;
        ensure!(read > 0, "OTLP peer closed before sending headers");
        bytes.extend_from_slice(&block[..read]);
    };
    let headers =
        std::str::from_utf8(&bytes[..header_end]).context("OTLP HTTP headers are not UTF-8")?;
    let mut lines = headers.split("\r\n");
    ensure!(
        lines.next() == Some("POST /v1/traces HTTP/1.1"),
        "unexpected OTLP request path/method"
    );
    let mut length = None;
    let mut json = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').context("malformed OTLP HTTP header")?;
        if key.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate OTLP content length");
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("OTLP content length")?,
            );
        }
        if key.eq_ignore_ascii_case("content-type") {
            json = value.trim() == "application/json";
        }
        ensure!(
            !key.eq_ignore_ascii_case("transfer-encoding"),
            "fixture requires a bounded content-length body"
        );
    }
    ensure!(json, "OTLP request must use application/json");
    let length = length.context("OTLP content length missing")?;
    ensure!(length <= MAX_BODY, "OTLP body exceeds fixture limit");
    while bytes.len() < header_end + length {
        let read = stream.read(&mut block).await?;
        ensure!(read > 0, "OTLP peer closed with an incomplete body");
        bytes.extend_from_slice(&block[..read]);
    }
    ensure!(
        bytes.len() == header_end + length,
        "unexpected bytes after OTLP body"
    );
    let spans = decode(&bytes[header_end..])?;
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
    stream.shutdown().await?;
    let mut receipt = state.receipt.lock().await;
    receipt.acknowledged_requests += 1;
    receipt.spans.extend(spans);
    drop(receipt);
    state.changed.notify_waiters();
    Ok(())
}
