use std::future::Future;
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
    let shell = StandardShell::new().with_cwd("/");
    let cancellation = CancellationToken::new();
    let (terminal_tx, terminal_rx) = oneshot::channel();
    let terminal_received = Arc::new(Notify::new());
    let publication_gate = Arc::new(Barrier::new(2));
    let published = Arc::new(AtomicBool::new(false));

    let provider_task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            let outcome = shell
                .exec_command_owned(
                    json!({
                        "cmd": format!("printf $$ > {}; exit 0", pid_path.display()),
                    }),
                    cancellation,
                )
                .await;
            let _ = terminal_tx.send(outcome.clone());
            outcome
        })
    };

    let consumer_task = {
        let terminal_received = Arc::clone(&terminal_received);
        let publication_gate = Arc::clone(&publication_gate);
        let published = Arc::clone(&published);
        tokio::spawn(async move {
            let outcome = terminal_rx.await.expect("provider terminal output");
            terminal_received.notify_one();
            publication_gate.wait().await;
            published.store(true, Ordering::Release);
            outcome
        })
    };

    terminal_received.notified().await;
    consumer_task.abort();
    assert!(
        consumer_task
            .await
            .expect_err("consumer must be abandoned before publication")
            .is_cancelled()
    );
    assert!(!published.load(Ordering::Acquire));

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
