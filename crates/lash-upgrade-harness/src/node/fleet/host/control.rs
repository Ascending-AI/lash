//! The private public-facade control channel is independent of Restate traffic.
use crate::e2e::host::HostCommand;
use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(super) async fn read_command(
    mut input: impl AsyncBufRead + Unpin,
) -> Result<Option<HostCommand>> {
    let mut line = String::new();
    let read = input
        .read_line(&mut line)
        .await
        .context("read fleet control request")?;
    if read == 0 {
        return Ok(None);
    }
    ensure!(
        line.len() < 1024 * 1024,
        "fleet control request exceeds limit"
    );
    serde_json::from_str(&line)
        .map(Some)
        .with_context(|| format!("decode fleet control request ({read} bytes): {line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    // A probe that disconnects without a command must not fail the serving
    // host's JoinSet; no public command has been submitted on this connection.
    #[tokio::test]
    async fn a_closed_control_probe_is_not_a_host_failure() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let client = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
        let (server, _) = listener.accept().await?;
        drop(client);
        let result = read_command(BufReader::new(server)).await;
        ensure!(
            matches!(result, Ok(None)),
            "disconnected control probe failed the host: {result:?}"
        );
        Ok(())
    }
}
