//! Per-connection evidence shared with the lifecycle owner, never logged.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt;
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use crate::config::{McpDiagnosticsPolicy, McpServerConfig, McpTransport};
use crate::error::{McpDiagnosticContent, McpExitObservation, McpPeer, McpStartupObservation};

#[derive(Clone)]
pub(crate) struct StartupDiagnostics {
    pub(crate) observation: Arc<Mutex<McpStartupObservation>>,
    pub(crate) policy: McpDiagnosticsPolicy,
}

impl StartupDiagnostics {
    pub(crate) fn new(config: &McpServerConfig) -> Self {
        Self {
            policy: config.diagnostics,
            observation: Arc::new(Mutex::new(McpStartupObservation {
                peer: match &config.transport {
                    McpTransport::Stdio(peer) => McpPeer::Stdio {
                        command: peer.command.clone(),
                    },
                    McpTransport::StreamableHttp(_) => McpPeer::StreamableHttp,
                },
                deadline: tokio::time::Instant::now() + config.startup_timeout(),
                timeout_ms: config.startup_timeout_ms,
                exit: McpExitObservation::Unavailable,
                stderr: if config.diagnostics.stderr_bytes == 0 {
                    McpDiagnosticContent::Omitted
                } else {
                    McpDiagnosticContent::Unavailable
                },
                invalid_line: None,
            })),
        }
    }

    pub(crate) fn snapshot(&self) -> Box<McpStartupObservation> {
        Box::new(self.observation.lock_recover().clone())
    }
}

/// Reads continuously even after the ring is full. Aborting closes the pipe;
/// a descendant retaining stderr cannot leave an orphaned reader behind.
pub(crate) struct StderrCapture {
    task: JoinHandle<()>,
}
impl StderrCapture {
    pub(crate) fn start(
        stderr: std::process::ChildStderr,
        diagnostics: StartupDiagnostics,
    ) -> std::io::Result<Self> {
        let mut stderr = tokio::process::ChildStderr::from_std(stderr)?;
        let limit = diagnostics.policy.stderr_bytes;
        diagnostics.observation.lock_recover().stderr = McpDiagnosticContent::Captured {
            bytes: Vec::new(),
            truncated: false,
            complete: false,
        };
        let task = tokio::spawn(async move {
            let mut ring = VecDeque::new();
            let mut truncated = false;
            let mut buffer = [0; 4096];
            loop {
                let read = stderr.read(&mut buffer).await;
                let complete = matches!(read, Ok(0));
                match read {
                    Ok(0) | Err(_) => {
                        diagnostics.observation.lock_recover().stderr =
                            McpDiagnosticContent::Captured {
                                bytes: ring.into_iter().collect(),
                                truncated,
                                complete,
                            };
                        return;
                    }
                    Ok(count) => {
                        for byte in &buffer[..count] {
                            if ring.len() == limit {
                                ring.pop_front();
                                truncated = true;
                            }
                            ring.push_back(*byte);
                        }
                        diagnostics.observation.lock_recover().stderr =
                            McpDiagnosticContent::Captured {
                                bytes: ring.iter().copied().collect(),
                                truncated,
                                complete: false,
                            };
                    }
                }
            }
        });
        Ok(Self { task })
    }

    pub(crate) async fn finish(&mut self, bound: std::time::Duration) {
        let _ = tokio::time::timeout(bound, &mut self.task).await;
        self.task.abort();
    }
}
impl Drop for StderrCapture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
