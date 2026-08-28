//! Shared parent-side SIGKILL matrix for backend full-turn helpers.
//!
//! # Why this file holds no wall-clock deadline
//!
//! Every wait here is an await on a *process event*: a line the helper prints
//! when it reaches its semantic crash point, or the helper's own exit. Each of
//! those events is already bounded by the helper's own semantic deadlines —
//! `wait_for_hit`'s hit timeout for the crash actions, and the recovery
//! lease-poll timeout for the recovery actions — which panic and so close
//! stdout / exit the process. Wrapping those awaits in a second, *tighter*
//! parent-side wall clock (this file used to spend 30s per wait against the
//! helper's own 60s hit budget) inverts the layering: the outer watchdog can
//! only ever fire on a healthy-but-slow helper, before the inner one has had a
//! chance to say what actually went wrong. That is FIG-2174 — a 4-vcpu runner
//! needed more than 30s to spawn a debug helper, run the SQLite migrations and
//! drive the scripted turn to its first seam, and the parent killed a run that
//! was making progress. It also produced the LEAK half of that report: a
//! cancelled `timeout` drops the helper future without reaping the child.
//!
//! So the waits are event-driven and unbounded here, the helper's semantic
//! deadlines bound each one, and the outermost bound is nextest's measured
//! per-test ceiling (`profile.ci` `slow-timeout`/`terminate-after` in
//! `.config/nextest.toml`), which releases the runner on a genuine hang.
//! Helpers are additionally spawned `kill_on_drop`, so a panic on any parent
//! assertion path reaps the child rather than leaking it.

use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Build a helper command that is always reaped, even if the parent panics
/// between spawn and the explicit kill.
fn helper_command<F>(
    command: &mut F,
    action: &str,
    nonce: &str,
    marker: &std::path::Path,
) -> tokio::process::Command
where
    F: FnMut(&str, &str, &std::path::Path) -> tokio::process::Command,
{
    let mut built = command(action, nonce, marker);
    built.kill_on_drop(true);
    built
}

pub async fn assert_real_turn_kill_recovery(
    tempdir: &std::path::Path,
    mut command: impl FnMut(&str, &str, &std::path::Path) -> tokio::process::Command,
) {
    for (action, crashed_count, recovered_count, expected_end_state, known_defect) in
        lash_core::testing::conformance::cold_process_turn_expectations()
    {
        let nonce = uuid::Uuid::new_v4().to_string();
        let marker = tempdir.join(format!("{action}-{nonce}.log"));
        let mut child = helper_command(&mut command, action, &nonce, &marker)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {action} real-turn helper: {error}"));
        let stdout = child.stdout.take().expect("real-turn helper stdout");
        let mut lines = BufReader::new(stdout).lines();
        let ready = lines
            .next_line()
            .await
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

        let recovered = helper_command(&mut command, "turn_recover", &nonce, &marker)
            .output()
            .await
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
    let crashed = helper_command(
        &mut command,
        "turn_checkpoint_after_execute_before_outcome",
        &nonce,
        &marker,
    )
    .output()
    .await
    .expect("spawn checkpoint outcome-gap helper");
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "checkpoint helper must die after local execution and before outcome finalization: {}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    let recovered = helper_command(&mut command, "turn_recover", &nonce, &marker)
        .output()
        .await
        .expect("spawn checkpoint outcome-gap recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "checkpoint outcome-gap recovery failed: {stderr}; stdout: {stdout}"
    );
    let expected_end_state =
        lash_core::testing::conformance::cold_process_durable_recovery_expectation(
            "checkpoint_execute_finalize",
        );
    let expected_summary = format!("turn_complete {expected_end_state}");
    assert!(
        stdout.lines().any(|line| line == expected_summary),
        "checkpoint outcome-gap recovery did not match `{expected_summary}`: {stdout}"
    );

    let nonce = format!("checkpoint-double-crash-{}", uuid::Uuid::new_v4());
    let marker = tempdir.join(format!("{nonce}.log"));
    let crashed = helper_command(
        &mut command,
        "turn_checkpoint_after_execute_before_outcome",
        &nonce,
        &marker,
    )
    .output()
    .await
    .expect("spawn checkpoint double-crash helper");
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "checkpoint double-crash helper must die after local execution and before outcome finalization: {}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    kill_at_semantic_point(
        &mut command,
        "turn_recover_final_commit_boundary",
        &nonce,
        &marker,
    )
    .await;
    let recovered = helper_command(&mut command, "turn_recover", &nonce, &marker)
        .output()
        .await
        .expect("spawn checkpoint outcome-gap final recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "checkpoint outcome-gap final recovery failed: {stderr}; stdout: {stdout}"
    );
    let expected_end_state =
        lash_core::testing::conformance::cold_process_durable_recovery_expectation(
            "checkpoint_replacement_double_crash",
        );
    let expected_summary = format!("turn_complete {expected_end_state}");
    assert!(
        stdout.lines().any(|line| line == expected_summary),
        "checkpoint outcome-gap double-crash recovery did not match `{expected_summary}`: {stdout}"
    );

    // FIG-1573: the drain-time orphan backstop must be *evaluated* while an
    // active-turn row is still pinned to the turn recovery is about to resume.
    // Crashing mid-stream leaves exactly that row, still undelivered: a row the
    // turn already delivered is settled by its checkpoint commit and no longer
    // pending, so an *undelivered* pinned row is the only shape a crash can
    // leave. This scenario seeds no next-turn row, so the recovering process
    // claims no next-turn input and therefore *evaluates* the backstop, and a
    // peer takes the lane and claims the queued-work row so recovery arrives
    // through its ordinary drain rather than the interrupted-turn path that
    // skips it. The resumed turn must still deliver the row at its own
    // checkpoint and reach the reviewed end state.
    let nonce = format!("peer-reclaim-pinned-active-input-{}", uuid::Uuid::new_v4());
    let marker = tempdir.join(format!("{nonce}.log"));
    kill_at_semantic_point(&mut command, "turn_provider_mid_stream", &nonce, &marker).await;
    let peer = helper_command(&mut command, "turn_peer_reclaim", &nonce, &marker)
        .output()
        .await
        .expect("spawn pinned-active-input peer helper");
    assert!(
        peer.status.success(),
        "pinned-active-input peer helper failed: {}; stdout: {}",
        String::from_utf8_lossy(&peer.stderr),
        String::from_utf8_lossy(&peer.stdout)
    );
    let recovered = helper_command(&mut command, "turn_recover", &nonce, &marker)
        .output()
        .await
        .expect("spawn pinned-active-input recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "pinned-active-input recovery failed: {stderr}; stdout: {stdout}"
    );
    let expected_end_state =
        lash_core::testing::conformance::cold_process_durable_recovery_expectation(
            "active_turn_input_pinned_to_recovered_turn",
        );
    let expected_summary = format!("turn_complete {expected_end_state}");
    assert!(
        stdout.lines().any(|line| line == expected_summary),
        "the resumed turn must keep the input pinned to it and reach `{expected_summary}`: {stdout}"
    );

    let nonce = format!("peer-reclaim-{}", uuid::Uuid::new_v4());
    let marker = tempdir.join(format!("{nonce}.log"));
    kill_at_semantic_point(&mut command, "turn_final_commit_boundary", &nonce, &marker).await;
    let peer = helper_command(&mut command, "turn_peer_reclaim", &nonce, &marker)
        .output()
        .await
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
    let recovered = helper_command(&mut command, "turn_recover", &nonce, &marker)
        .output()
        .await
        .expect("spawn peer-reclaim recovery");
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(
        recovered.status.success(),
        "peer-reclaim recovery failed: {stderr}; stdout: {stdout}"
    );
    let expected_end_state =
        lash_core::testing::conformance::cold_process_durable_recovery_expectation("peer_reclaim");
    let expected_summary = format!("turn_complete {expected_end_state}");
    assert!(
        stdout.lines().any(|line| line == expected_summary),
        "peer-reclaim recovery did not match `{expected_summary}`: {stdout}"
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
    let mut child = helper_command(command, action, nonce, marker)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"));
    let stdout = child.stdout.take().expect("crash helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let ready = lines
        .next_line()
        .await
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
