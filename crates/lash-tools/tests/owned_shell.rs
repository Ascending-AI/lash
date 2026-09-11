use std::future::Future;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use lash_core::ToolOutcome;
use lash_tools::shell::StandardShell;
use serde_json::json;
#[cfg(unix)]
use tokio::sync::{Barrier, Notify, oneshot};
use tokio_util::sync::CancellationToken;

fn assert_send_static<F>(_: &F)
where
    F: Future<Output = ToolOutcome> + Send + 'static,
{
}

#[tokio::test]
async fn owned_exec_is_send_static_and_reaches_terminal_output() {
    let shell = StandardShell::new().with_cwd("/");
    let future = shell.clone().exec_command_owned(
        json!({ "cmd": "printf owned-shell" }),
        CancellationToken::new(),
    );
    assert_send_static(&future);

    let outcome = tokio::time::timeout(Duration::from_secs(5), tokio::spawn(future))
        .await
        .expect("owned shell execution timed out")
        .expect("owned shell task panicked");
    assert!(outcome.is_success(), "{}", outcome.value_for_projection());
    assert_eq!(outcome.value_for_projection()["status"], "completed");
    assert_eq!(outcome.value_for_projection()["exit_code"], 0);
    assert_eq!(outcome.value_for_projection()["output"], "owned-shell");
}

#[cfg(unix)]
#[tokio::test]
async fn provider_owned_exec_drains_after_consumer_abandons_before_publication() {
    let dir = tempfile::tempdir().expect("provider-owned marker directory");
    let pid_path = dir.path().join("shell-pid");
    let started_path = dir.path().join("shell-started");
    let release_path = dir.path().join("release-shell");
    let shell = StandardShell::new().with_cwd("/");
    let cancellation = CancellationToken::new();
    let (terminal_tx, terminal_rx) = oneshot::channel();
    let consumer_waiting = Arc::new(Notify::new());
    let shell_terminal = Arc::new(Notify::new());
    let publication_gate = Arc::new(Barrier::new(2));
    let published = Arc::new(AtomicBool::new(false));

    let provider_task = {
        let cancellation = cancellation.clone();
        let shell_terminal = Arc::clone(&shell_terminal);
        let publication_gate = Arc::clone(&publication_gate);
        let published = Arc::clone(&published);
        tokio::spawn(async move {
            let outcome = shell
                .exec_command_owned(
                    json!({
                        "cmd": format!(
                            "printf $$ > '{}'; : > '{}'; i=0; while [ ! -e '{}' ] && [ \"$i\" -lt 500 ]; do i=$((i + 1)); sleep 0.01; done; test -e '{}'",
                            pid_path.display(),
                            started_path.display(),
                            release_path.display(),
                            release_path.display(),
                        ),
                        "timeout_ms": 5_000,
                    }),
                    cancellation,
                )
                .await;
            shell_terminal.notify_one();
            publication_gate.wait().await;
            if terminal_tx.send(outcome.clone()).is_ok() {
                published.store(true, Ordering::Release);
            }
            outcome
        })
    };

    let consumer_task = {
        let consumer_waiting = Arc::clone(&consumer_waiting);
        tokio::spawn(async move {
            consumer_waiting.notify_one();
            terminal_rx.await.expect("provider terminal output")
        })
    };

    tokio::time::timeout(Duration::from_secs(5), consumer_waiting.notified())
        .await
        .expect("consumer did not begin awaiting terminal output");
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_path(dir.path().join("shell-started")),
    )
    .await
    .expect("real shell did not reach its release barrier");

    // Abandon the transient consumer while the real shell is still running
    // and before any terminal value has been published.
    consumer_task.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), consumer_task)
            .await
            .expect("consumer abort did not settle")
            .expect_err("consumer must be abandoned before publication")
            .is_cancelled()
    );
    assert!(!published.load(Ordering::Acquire));

    // Let the real shell exit. The provider-owned task reaches terminal output
    // independently, then pauses before its explicit publication step.
    std::fs::write(dir.path().join("release-shell"), b"").expect("release the real shell process");
    tokio::time::timeout(Duration::from_secs(5), shell_terminal.notified())
        .await
        .expect("provider-owned execution did not observe shell termination");
    assert!(!published.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(5), publication_gate.wait())
        .await
        .expect("provider did not reach its publication barrier");

    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), provider_task)
        .await
        .expect("provider drain timed out")
        .expect("provider-owned shell task panicked");
    assert!(outcome.is_success(), "{}", outcome.value_for_projection());
    assert_eq!(outcome.value_for_projection()["status"], "completed");

    let pid: i32 = std::fs::read_to_string(dir.path().join("shell-pid"))
        .expect("read completed shell pid")
        .parse()
        .expect("parse completed shell pid");
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "completed child is still live"
    );
    assert!(!published.load(Ordering::Acquire));
}

#[cfg(unix)]
async fn wait_for_path(path: impl AsRef<Path>) {
    while !path.as_ref().exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
