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
}

fn marker_lines(path: &std::path::Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents.lines().count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read external-effect marker {}: {error}", path.display()),
    }
}
