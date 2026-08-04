//! Shared parent-side SIGKILL matrix for backend full-turn helpers.

use tokio::io::{AsyncBufReadExt as _, BufReader};

pub async fn assert_real_turn_kill_recovery(
    tempdir: &std::path::Path,
    mut command: impl FnMut(&str, &str, &std::path::Path) -> tokio::process::Command,
) {
    for (action, crashed_count, recovered_count, expected_end_state, known_defect) in
        lash_core::testing::conformance::cold_process_turn_expectations()
    {
        let nonce = uuid::Uuid::new_v4().to_string();
        let marker = tempdir.join(format!("{action}-{nonce}.log"));
        let mut child = command(action, &nonce, &marker)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {action} real-turn helper: {error}"));
        let stdout = child.stdout.take().expect("real-turn helper stdout");
        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("{action} helper did not reach its semantic crash point"))
            .expect("read real-turn helper signal")
            .unwrap_or_else(|| panic!("{action} helper exited before its crash point"));
        assert_eq!(ready, "crash_ready", "unexpected {action} helper signal");

        child
            .kill()
            .await
            .unwrap_or_else(|error| panic!("SIGKILL {action} helper: {error}"));
        let status = child
            .wait()
            .await
            .unwrap_or_else(|error| panic!("reap {action} helper: {error}"));
        assert!(
            !status.success(),
            "SIGKILLed {action} helper exited successfully"
        );
        assert_eq!(
            marker_lines(&marker),
            crashed_count,
            "{action} crash prefix"
        );

        let recovered = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            command("turn_recover", &nonce, &marker).output(),
        )
        .await
        .unwrap_or_else(|_| panic!("{action} recovery helper timed out"))
        .unwrap_or_else(|error| panic!("spawn {action} recovery helper: {error}"));
        let recovery_stdout = String::from_utf8_lossy(&recovered.stdout);
        let recovery_stderr = String::from_utf8_lossy(&recovered.stderr);
        assert!(
            recovered.status.success(),
            "{action} recovery helper failed: {recovery_stderr}; stdout: {recovery_stdout}"
        );
        let expected_summary = format!("turn_complete {expected_end_state}");
        assert!(
            recovery_stdout.lines().any(|line| line == expected_summary),
            "{action} recovery helper did not match the exact durable end state `{expected_summary}`: {recovery_stdout}"
        );
        if let Some(notice) = known_defect {
            eprintln!("{notice}");
        }
        assert_eq!(
            marker_lines(&marker),
            recovered_count,
            "{action} external-effect recovery count"
        );
    }

    let nonce = format!("checkpoint-outcome-gap-{}", uuid::Uuid::new_v4());
    let marker = tempdir.join(format!("{nonce}.log"));
    let crashed = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command(
            "turn_checkpoint_after_execute_before_outcome",
            &nonce,
            &marker,
        )
        .output(),
    )
    .await
    .expect("checkpoint outcome-gap helper timed out")
    .expect("spawn checkpoint outcome-gap helper");
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "checkpoint helper must die after local execution and before outcome finalization: {}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    kill_at_semantic_point(
        &mut command,
        "turn_recover_final_commit_boundary",
        &nonce,
        &marker,
    )
    .await;
    let recovered = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command("turn_recover", &nonce, &marker).output(),
    )
    .await
    .expect("checkpoint outcome-gap final recovery timed out")
    .expect("spawn checkpoint outcome-gap final recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "checkpoint outcome-gap final recovery failed: {stderr}; stdout: {stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == "turn_complete terminal=1 pending_inputs=0 queued_work=0"),
        "checkpoint outcome-gap double-crash recovery did not settle all ingress: {stdout}"
    );

    let nonce = format!("peer-reclaim-{}", uuid::Uuid::new_v4());
    let marker = tempdir.join(format!("{nonce}.log"));
    kill_at_semantic_point(&mut command, "turn_final_commit_boundary", &nonce, &marker).await;
    let peer = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command("turn_peer_reclaim", &nonce, &marker).output(),
    )
    .await
    .expect("peer-reclaim helper timed out")
    .expect("spawn peer-reclaim helper");
    assert!(
        peer.status.success(),
        "peer-reclaim helper failed: {}; stdout: {}",
        String::from_utf8_lossy(&peer.stderr),
        String::from_utf8_lossy(&peer.stdout)
    );
    assert!(
        String::from_utf8_lossy(&peer.stdout)
            .lines()
            .any(|line| line.starts_with("peer_claim row=")),
        "peer-reclaim helper did not report superseding authority"
    );
    let recovered = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command("turn_recover", &nonce, &marker).output(),
    )
    .await
    .expect("peer-reclaim recovery timed out")
    .expect("spawn peer-reclaim recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "peer-reclaim recovery failed: {stderr}; stdout: {stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == "turn_complete terminal=1 pending_inputs=0 queued_work=1"),
        "recovery must commit once while leaving the peer-owned row untouched: {stdout}"
    );
}

async fn kill_at_semantic_point<F>(
    command: &mut F,
    action: &str,
    nonce: &str,
    marker: &std::path::Path,
) where
    F: FnMut(&str, &str, &std::path::Path) -> tokio::process::Command,
{
    let mut child = command(action, nonce, marker)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"));
    let stdout = child.stdout.take().expect("crash helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
        .await
        .unwrap_or_else(|_| panic!("{action} helper did not reach its semantic crash point"))
        .expect("read crash helper signal")
        .unwrap_or_else(|| panic!("{action} helper exited before its crash point"));
    assert_eq!(ready, "crash_ready", "unexpected {action} helper signal");
    child
        .kill()
        .await
        .unwrap_or_else(|error| panic!("SIGKILL {action} helper: {error}"));
    let status = child
        .wait()
        .await
        .unwrap_or_else(|error| panic!("reap {action} helper: {error}"));
    assert!(
        !status.success(),
        "SIGKILLed {action} helper exited successfully"
    );
}

fn marker_lines(path: &std::path::Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents.lines().count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read external-effect marker {}: {error}", path.display()),
    }
}
